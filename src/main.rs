use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};
use weinav_forge::{
    build, cache, fetch, gate, pack,
    policy::{Flavor, Plan, Role, System},
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

#[derive(Args)]
struct Common {
    #[arg(long, value_enum, help = "Select the source policy.")]
    flavor: Flavor,
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

#[derive(Args)]
struct Process {
    #[command(flatten)]
    common: Common,
    #[arg(
        long,
        help = "Read this manifest instead of the flavor manifest in the cache."
    )]
    manifest: Option<PathBuf>,
    #[arg(
        long,
        default_value = "output",
        help = "Write ephemeris.zip and report.json to this staging directory."
    )]
    output: PathBuf,
    #[arg(long, help = "Write the build report to this path.")]
    report: Option<PathBuf>,
    #[arg(
        long,
        default_value_t = 1.0,
        help = "Set the maximum orbit fit RMS in metres."
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

fn process(options: Process, at: Instant) -> Result<u8> {
    let common = &options.common;
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
    let inputs = match build::Inputs::load(
        &common.cache,
        options.manifest.as_deref(),
        common.flavor,
        &common.systems,
        !common.no_agnss,
        &local,
    ) {
        Ok(inputs) => inputs,
        Err(e) => {
            remove_output(&zip_path)?;
            cache::atomic_write(
                &report_path,
                &serde_json::to_vec_pretty(
                    &json!({"version":1,"status":"source-unavailable","at":at.0.to_rfc3339(),"plan":Plan::new(common.flavor,&common.systems,!common.no_agnss),"error":format!("{e:#}")}),
                )?,
            )?;
            eprintln!("Source unavailable: {e:#}");
            return Ok(3);
        }
    };
    let products = match build::assemble(
        &inputs,
        common.flavor,
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
                    &json!({"version":1,"status":"refused","at":at.0.to_rfc3339(),"plan":Plan::new(common.flavor,&common.systems,!common.no_agnss),"sources":inputs.sources,"error":format!("{e:#}")}),
                )?,
            )?;
            eprintln!("Output refused: {e:#}");
            return Ok(4);
        }
    };
    let mut checks = gate::inspect(
        &products,
        &inputs,
        common.flavor,
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
    let report = json!({"version":1,"status":status,"at":at.0.to_rfc3339(),"timestamp_ms":at.0.timestamp_millis(),"elapsed_seconds":started.elapsed().as_secs_f64(),"development_bypass":options.development_bypass_gates,"plan":Plan::new(common.flavor,&common.systems,!common.no_agnss),"sources":inputs.sources,"epochs":products.epochs,"notes":products.notes,"payload_sha256":file_hashes,"zip_sha256":packed.as_ref().map(|b|cache::hash(b)),"gate":checks});
    cache::atomic_write(&report_path, &serde_json::to_vec_pretty(&report)?)?;
    if accepted {
        cache::atomic_write(&zip_path, &packed.unwrap())?;
        println!("Wrote {} and {}", zip_path.display(), report_path.display());
        Ok(0)
    } else {
        remove_output(&zip_path)?;
        eprintln!(
            "Output refused. Read {} for the failed checks.",
            report_path.display()
        );
        Ok(4)
    }
}

fn run(cli: Cli) -> Result<u8> {
    let common = match &cli.command {
        Command::Fetch(o) => &o.common,
        Command::Process(o) => &o.common,
    };
    let at = validate(common)?;
    if common.plan {
        println!(
            "{}",
            serde_json::to_string_pretty(&Plan::new(
                common.flavor,
                &common.systems,
                !common.no_agnss
            ))?
        );
        return Ok(0);
    }
    match cli.command {
        Command::Fetch(options) => {
            let common = options.common;
            let local = local(&common)?;
            let urls = assignments(&options.url)?;
            match fetch::run(fetch::Options {
                cache: &common.cache,
                flavor: common.flavor,
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
                        common
                            .cache
                            .join(fetch::manifest_name(common.flavor))
                            .display()
                    );
                    Ok(0)
                }
                Err(e) => {
                    eprintln!("Source unavailable: {e:#}");
                    Ok(3)
                }
            }
        }
        Command::Process(options) => process(options, at),
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
