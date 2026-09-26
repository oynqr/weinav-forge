use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Mutex,
};
use weinav_forge::{
    build, cache, fetch, gate, pack,
    policy::{Flavor, Plan, Role, System},
    report,
    time::Instant,
};

#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(
    version,
    about = "Build GNSS assistance data for Huawei watches and Gadgetbridge."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Get source data and save a complete cache manifest.")]
    Fetch(Fetch),
    #[command(about = "Build and check a ZIP file from local source data.")]
    Process(Process),
}

#[derive(Args, Clone)]
struct Common {
    #[arg(long, value_enum, help = "Select the source policy.")]
    flavor: Option<Flavor>,
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        default_value = "gps,glonass,galileo,bds,qzs",
        help = "Select the constellations. GPS is required."
    )]
    systems: Vec<System>,
    #[arg(long, help = "Do not include AGNSS data.")]
    no_agnss: bool,
    #[arg(
        long,
        value_name = "UTC",
        help = "Use this RFC 3339 time instead of the current time."
    )]
    at: Option<String>,
    #[arg(
        long,
        default_value = "cache",
        help = "Read or write source data in this directory."
    )]
    cache: PathBuf,
    #[arg(
        long,
        value_name = "ROLE=PATH",
        help = "Use a local file for a source role. Repeat for more files."
    )]
    source: Vec<String>,
    #[arg(
        long,
        help = "Print the source plan and exit without reading source files."
    )]
    plan: bool,
}

#[derive(Args)]
#[command(mut_arg("flavor", |arg| arg.required(true)))]
struct Fetch {
    #[command(flatten)]
    common: Common,
    #[arg(long, help = "Use only files that are already in the cache.")]
    offline: bool,
    #[arg(
        long,
        value_name = "ROLE=URL",
        help = "Use this source URL. Repeat to set a fallback chain."
    )]
    url: Vec<String>,
}

#[derive(Args, Clone)]
#[command(mut_arg("flavor", |arg| arg.required_unless_present("variants")))]
struct Process {
    #[command(flatten)]
    common: Common,
    #[arg(long, value_name = "PATH", conflicts_with_all = ["flavor", "systems", "no_agnss", "source", "manifest", "report", "fit_rms", "allow_degraded", "development_bypass_gates"], help = "Build the variants in this JSON file. Use one output directory per name.")]
    variants: Option<PathBuf>,
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..), help = "Set the maximum number of processing threads. Default: available CPU threads.")]
    threads: Option<u16>,
    #[arg(
        long,
        help = "Read this manifest instead of the flavor manifest in the cache."
    )]
    manifest: Option<PathBuf>,
    #[arg(
        long,
        default_value = "output",
        help = "Write ephemeris.zip, report.json and the broadcast health file to this staging directory."
    )]
    output: PathBuf,
    #[arg(long, help = "Write the build report to this path.")]
    report: Option<PathBuf>,
    #[arg(
        long,
        default_value_t = 1.0,
        help = "Set the maximum standard error of the orbit fit in metres."
    )]
    fit_rms: f64,
    #[arg(
        long,
        help = "Permit the open flavor's empty horizons and partial EXTRA data."
    )]
    allow_degraded: bool,
    #[arg(long, hide = true)]
    development_bypass_gates: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Variant {
    name: String,
    flavor: Flavor,
    #[serde(default = "all_systems")]
    systems: Vec<String>,
    #[serde(default = "include_agnss")]
    agnss: bool,
    #[serde(default = "default_fit_rms")]
    fit_rms: f64,
    #[serde(default)]
    allow_degraded: bool,
}

fn all_systems() -> Vec<String> {
    System::ALL
        .iter()
        .map(|s| s.name().to_ascii_lowercase())
        .collect()
}

fn include_agnss() -> bool {
    true
}

fn default_fit_rms() -> f64 {
    1.0
}

fn variants(options: &Process) -> Result<Vec<Process>> {
    let Some(path) = &options.variants else {
        return Ok(vec![options.clone()]);
    };
    let variants: Vec<Variant> = serde_json::from_slice(&cache::read(path)?)?;
    ensure!(!variants.is_empty(), "select at least one variant");
    let mut names = BTreeSet::new();
    variants.into_iter().map(|variant| {
        ensure!(
            variant.name.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
                && variant.name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "variant name must start with a letter or digit and contain only ASCII letters, digits, underscores or hyphens"
        );
        ensure!(names.insert(variant.name.clone()), "duplicate variant name: {}", variant.name);
        let mut item = options.clone();
        item.variants = None;
        item.common.flavor = Some(variant.flavor);
        item.common.systems = variant.systems.iter().map(|s| System::from_str(s, false).map_err(anyhow::Error::msg)).collect::<Result<_>>()?;
        item.common.no_agnss = !variant.agnss;
        item.fit_rms = variant.fit_rms;
        item.allow_degraded = variant.allow_degraded;
        item.output = options.output.join(variant.name);
        Ok(item)
    }).collect()
}

fn assignments(values: &[String]) -> Result<BTreeMap<Role, Vec<String>>> {
    let mut result: BTreeMap<Role, Vec<String>> = BTreeMap::new();
    for value in values {
        let (role, path) = value
            .split_once('=')
            .context("use ROLE=VALUE for each source")?;
        let role = Role::from_str(role, false).map_err(anyhow::Error::msg)?;
        ensure!(!path.is_empty(), "empty source value");
        result.entry(role).or_default().push(path.into());
    }
    Ok(result)
}

fn local(common: &Common) -> Result<BTreeMap<Role, Vec<PathBuf>>> {
    Ok(assignments(&common.source)?
        .into_iter()
        .map(|(r, p)| (r, p.into_iter().map(PathBuf::from).collect()))
        .collect())
}

fn validate(common: &Common) -> Result<Instant> {
    ensure!(
        !common.systems.is_empty() && common.systems.contains(&System::Gps),
        "select GPS and at least one constellation"
    );
    ensure!(
        common
            .systems
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            == common.systems.len(),
        "duplicate constellation"
    );
    common
        .at
        .as_deref()
        .map(Instant::parse)
        .transpose()
        .map(|at| at.unwrap_or_else(Instant::now))
}

fn remove_output(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn process(options: Process, at: Instant, source_loading: &Mutex<()>) -> Result<u8> {
    let common = &options.common;
    let flavor = common.flavor.context("select a flavor")?;
    ensure!(
        options.fit_rms.is_finite() && options.fit_rms > 0.0,
        "fit RMS must be finite and positive"
    );
    let local = local(common)?;
    let report_path = options
        .report
        .clone()
        .unwrap_or_else(|| options.output.join("report.json"));
    let zip_path = options.output.join(if options.development_bypass_gates {
        "ephemeris.UNSAFE.zip"
    } else {
        "ephemeris.zip"
    });
    ensure!(
        report_path != zip_path,
        "report path cannot be the ZIP path"
    );
    fs::create_dir_all(&options.output)?;
    let started = std::time::Instant::now();
    let loaded = {
        let _guard = source_loading
            .lock()
            .map_err(|e| anyhow::anyhow!("source loading lock failed: {e}"))?;
        build::Inputs::load(
            &common.cache,
            options.manifest.as_deref(),
            flavor,
            &common.systems,
            !common.no_agnss,
            &local,
        )
    };
    let inputs = match loaded {
        Ok(inputs) => inputs,
        Err(e) => {
            remove_output(&zip_path)?;
            cache::atomic_write(
                &report_path,
                &serde_json::to_vec_pretty(
                    &json!({"version":1,"status":"source-unavailable","at":at.0.to_rfc3339(),"policy":Plan::new(flavor,&common.systems,!common.no_agnss),"error":format!("{e:#}")}),
                )?,
            )?;
            eprintln!("Source unavailable: {e:#}");
            return Ok(3);
        }
    };
    let products = match build::assemble(
        &inputs,
        flavor,
        &common.systems,
        !common.no_agnss,
        at,
        options.fit_rms,
    ) {
        Ok(products) => products,
        Err(e) => {
            remove_output(&zip_path)?;
            cache::atomic_write(
                &report_path,
                &serde_json::to_vec_pretty(
                    &json!({"version":1,"status":"refused","at":at.0.to_rfc3339(),"policy":Plan::new(flavor,&common.systems,!common.no_agnss),"sources":inputs.sources,"error":format!("{e:#}")}),
                )?,
            )?;
            eprintln!("Output refused: {e:#}");
            return Ok(4);
        }
    };
    let mut checks = gate::inspect(
        &products,
        &inputs,
        flavor,
        &common.systems,
        !common.no_agnss,
        at,
        options.allow_degraded,
    );
    checks.check(
        "build-duration",
        "ZIP",
        started.elapsed().as_secs() < 1800,
        "build completed before its ZIP timestamp expires",
    );
    let mut packed = None;
    if checks.passed() || options.development_bypass_gates {
        match pack::zip(&products.files, at) {
            Ok(bytes) => {
                checks.check(
                    "S10",
                    "ZIP",
                    true,
                    "STORED entries read back and compared with the exact gated bytes",
                );
                packed = Some(bytes);
            }
            Err(e) => checks.check("S10", "ZIP", false, format!("{e:#}")),
        }
    } else {
        checks.skip(
            "S10",
            "ZIP",
            "packing did not run because the gate refused output",
        );
    }
    let accepted = packed.is_some() && (checks.passed() || options.development_bypass_gates);
    let status = if accepted {
        if options.development_bypass_gates {
            "unsafe-development-output"
        } else {
            "passed"
        }
    } else {
        "refused"
    };
    let file_hashes: BTreeMap<_, _> = products
        .files
        .iter()
        .map(|(n, b)| (n, cache::hash(b)))
        .collect();
    let mut report = json!({"version":1,"status":status,"at":at.0.to_rfc3339(),"timestamp_ms":at.0.timestamp_millis(),"elapsed_seconds":started.elapsed().as_secs_f64(),"development_bypass":options.development_bypass_gates,"policy":Plan::new(flavor,&common.systems,!common.no_agnss),"sources":inputs.sources,"epochs":products.epochs,"notes":products.notes,"payload_sha256":file_hashes,"zip_sha256":packed.as_ref().map(|b|cache::hash(b)),"gate":checks});
    report
        .as_object_mut()
        .context("report is not a JSON object")?
        .extend(report::gate_fields(
            &inputs,
            &products,
            &Plan::new(flavor, &common.systems, !common.no_agnss),
            at,
            options.allow_degraded,
        ));
    cache::atomic_write(&report_path, &serde_json::to_vec_pretty(&report)?)?;
    if accepted {
        if let Some(source) = inputs.health_source() {
            cache::atomic_write(
                &options.output.join(report::published_name(source)),
                &inputs.health_snapshot,
            )?;
        }
        cache::atomic_write(&zip_path, &packed.unwrap())?;
        println!("Wrote {} and {}", zip_path.display(), report_path.display());
        Ok(0)
    } else {
        remove_output(&zip_path)?;
        for check in &checks.checks {
            if matches!(check.status, gate::Status::Fail) {
                eprintln!(
                    "{}: {} {} failed: {}",
                    report_path.display(),
                    check.product,
                    check.id,
                    check.detail
                );
            }
        }
        eprintln!(
            "Output refused. Read {} for the failed checks.",
            report_path.display()
        );
        Ok(4)
    }
}

impl Process {
    fn worker_count(&self) -> usize {
        self.threads.map_or_else(
            || std::thread::available_parallelism().map_or(1, |count| count.get()),
            usize::from,
        )
    }
}

fn process_all(options: Process) -> Result<u8> {
    let at = validate(&options.common)?;
    let variants = variants(&options)?;
    for variant in &variants {
        validate(&variant.common)?;
        ensure!(
            variant.fit_rms.is_finite() && variant.fit_rms > 0.0,
            "fit RMS must be finite and positive"
        );
    }
    if options.common.plan {
        let plans: Vec<_> = variants
            .iter()
            .map(|variant| {
                let common = &variant.common;
                Plan::new(common.flavor.unwrap(), &common.systems, !common.no_agnss)
            })
            .collect();
        let value = if options.variants.is_some() {
            serde_json::to_value(&plans)?
        } else {
            serde_json::to_value(&plans[0])?
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(0);
    }
    let threads = options.worker_count();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()?;
    let mut code = 0;
    let source_loading = Mutex::new(());
    for chunk in variants.chunks(threads) {
        let results: Vec<_> = pool.install(|| {
            chunk
                .par_iter()
                .map(|variant| process(variant.clone(), at, &source_loading))
                .collect()
        });
        for result in results {
            let status = match result {
                Ok(status) => status,
                Err(e) => {
                    eprintln!("Error: {e:#}");
                    1
                }
            };
            if code != 1 && status != 0 {
                code = if status == 1 { 1 } else { code.max(status) };
            }
        }
    }
    Ok(code)
}

fn run(cli: Cli) -> Result<u8> {
    match cli.command {
        Command::Fetch(options) => {
            let common = options.common;
            let at = validate(&common)?;
            let flavor = common.flavor.context("select a flavor")?;
            if common.plan {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&Plan::new(
                        flavor,
                        &common.systems,
                        !common.no_agnss
                    ))?
                );
                return Ok(0);
            }
            let local = local(&common)?;
            let urls = assignments(&options.url)?;
            match fetch::run(fetch::Options {
                cache: &common.cache,
                flavor,
                systems: &common.systems,
                agnss: !common.no_agnss,
                at,
                offline: options.offline,
                local: &local,
                urls: &urls,
            }) {
                Ok(manifest) => {
                    println!(
                        "Saved {} sources in {}",
                        manifest.sources.len(),
                        common.cache.join(fetch::manifest_name(flavor)).display()
                    );
                    Ok(0)
                }
                Err(e) => {
                    eprintln!("Source unavailable: {e:#}");
                    Ok(3)
                }
            }
        }
        Command::Process(options) => process_all(options),
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("Error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processing_defaults_to_available_cpu_threads_and_accepts_an_override() {
        let Command::Process(automatic) =
            Cli::try_parse_from(["weinav-forge", "process", "--flavor", "huawei"])
                .unwrap()
                .command
        else {
            panic!("expected processing command");
        };
        assert!(automatic.threads.is_none());
        assert_eq!(
            automatic.worker_count(),
            std::thread::available_parallelism().unwrap().get()
        );
        let Command::Process(explicit) = Cli::try_parse_from([
            "weinav-forge",
            "process",
            "--variants",
            "variants.json",
            "--threads",
            "3",
        ])
        .unwrap()
        .command
        else {
            panic!("expected processing command");
        };
        assert_eq!(explicit.worker_count(), 3);
    }
}
