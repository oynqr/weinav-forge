use crate::http::Client;
use crate::{
    cache::{self, Coverage, Manifest, Source},
    policy::{Flavor, Plan, Role, System},
    rinex::Broadcast,
    rtcm,
    seed::{MAX_DECODED_BYTES, Seed, decompress},
    snapshot,
    sp3::{Antex, Sp3},
    time::Instant,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::{Datelike, Timelike, Utc};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;

const BROADCAST_SNAPSHOT_URL: &str = "https://igs.bkg.bund.de/root_ftp/NTRIP/BRDC/brdc_last.rnx.Z";
const CODE_BASE: &str = "https://zhw-b.s3.cloud.switch.ch/aiub/CODE";
const ESA_DAILY: &str = "ftp://gssc.esa.int/gnss/data/daily";
const ESA_HOURLY: &str = "ftp://gssc.esa.int/gnss/data/hourly";
const WUHAN_DAILY: &str = "ftp://igs.gnsswhu.cn/pub/gps/data/daily";
const GALILEO_ALMANAC_CANDIDATES: usize = 3;
const CELESTRAK_YUMA: &str = "https://celestrak.org/GPS/almanac/Yuma";
const CELESTRAK_PROVIDER: &str = "celestrak";
const CELESTRAK_CANDIDATES: usize = 3;
const SECONDS_PER_WEEK: i64 = 604_800;
const ALMANAC_WEEKS: i64 = 1024;
const ALMANAC_LEAD_S: i64 = 7 * 86_400;
const GALILEO_ISSUE_TOLERANCE_S: i64 = 300;
const SNAPSHOT_PROVIDER: &str = "esa-hourly";
const SNAPSHOT_MIN_STATIONS: usize = 12;
const SNAPSHOT_HOURS_BACK: [i64; 2] = [1, 2];
const SECONDS_PER_HOUR: i64 = 3600;
const SNAPSHOT_STATIONS: [&str; 30] = [
    "TKBG00JPN",
    "USUD00JPN",
    "NTUS00SGP",
    "IISC00IND",
    "GDKG00IND",
    "POHN00FSM",
    "MAJU00MHL",
    "XMIS00AUS",
    "DARW00AUS",
    "KARR00AUS",
    "SYDN00AUS",
    "HOB200AUS",
    "TONG00TON",
    "NRMG00NCL",
    "KIRI00KIR",
    "LAUT00FJI",
    "WTZR00DEU",
    "KIRU00SWE",
    "MAS100ESP",
    "ISTA00TUR",
    "HRAO00ZAF",
    "MAL200KEN",
    "VACS00MUS",
    "ALGO00CAN",
    "FAIR00USA",
    "MKEA00USA",
    "SANT00CHL",
    "KOUR00GUF",
    "THU200GRL",
    "MCM400ATA",
];
pub const BKG_SUBSTITUTES: [&str; 3] = ["dlr-brdm", "igs-brdc", SNAPSHOT_PROVIDER];

#[derive(Debug)]
struct ConnectionRefused;

impl std::fmt::Display for ConnectionRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("connection failed")
    }
}

impl std::error::Error for ConnectionRefused {}

fn refused_connection(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<ConnectionRefused>() || crate::http::refused_connection(cause))
}

fn code_prediction_url(day: chrono::DateTime<Utc>) -> String {
    format!(
        "{CODE_BASE}/COD0OPSPRD_{}0000_05D_05M_ORB.SP3",
        day.format("%Y%j")
    )
}

pub struct Options<'a> {
    pub cache: &'a Path,
    pub flavor: Flavor,
    pub systems: &'a [System],
    pub agnss: bool,
    pub at: Instant,
    pub offline: bool,
    pub local: &'a BTreeMap<Role, Vec<PathBuf>>,
    pub urls: &'a BTreeMap<Role, Vec<String>>,
}

struct Fetcher<'a> {
    root: &'a Path,
    client: Option<Client>,
    offline: bool,
    attempts: Vec<String>,
    unreachable: BTreeSet<String>,
}

pub fn manifest_name(flavor: Flavor) -> String {
    format!("{}.json", flavor.as_str())
}

pub fn validate(role: Role, bytes: &[u8]) -> Result<()> {
    inspect(role, bytes).map(|_| ())
}

pub fn inspect(role: Role, bytes: &[u8]) -> Result<Vec<Coverage>> {
    let mut spans: BTreeMap<System, (f64, f64)> = BTreeMap::new();
    let mut add = |system, start: f64, end: f64| {
        let span = spans.entry(system).or_insert((start, end));
        span.0 = span.0.min(start);
        span.1 = span.1.max(end);
    };
    match role {
        Role::Seed => {
            let seed = Seed::parse(bytes)?;
            for system in System::ALL {
                add(system, seed.start as f64, seed.end as f64);
            }
        }
        Role::Agnss => {
            let bytes = decompress(bytes)?;
            for payload in rtcm::payloads(&bytes)? {
                rtcm::Message::decode(payload)?;
            }
        }
        Role::Prediction | Role::BdsPrediction | Role::QzsPrediction => {
            let mut sp3 = Sp3::default();
            sp3.add(bytes)?;
            for ((system, _), samples) in sp3.satellites {
                for sample in samples {
                    add(system, sample.time, sample.time);
                }
            }
        }
        Role::Broadcast => {
            let mut broadcast = Broadcast::default();
            broadcast.add(bytes)?;
            for ((system, _), records) in broadcast.records {
                for record in records {
                    add(system, record.epoch, record.epoch);
                }
            }
        }
        Role::Antex => {
            Antex::parse(bytes)?;
        }
        Role::Ionosphere => {
            let mut broadcast = Broadcast::default();
            broadcast.add(bytes)?;
            ensure!(
                broadcast.klobuchar.is_some(),
                "no GPS Klobuchar coefficients in the RINEX file"
            );
        }
        Role::GpsAlmanac | Role::QzsAlmanac => {
            let bytes = decompress(bytes)?;
            let text = std::str::from_utf8(&bytes)?;
            ensure!(
                text.contains("ID:") && text.contains("Eccentricity"),
                "invalid YUMA almanac"
            );
        }
        Role::GalileoAlmanac => {
            let bytes = decompress(bytes)?;
            roxmltree::Document::parse(std::str::from_utf8(&bytes)?)?;
        }
    }
    Ok(spans
        .into_iter()
        .map(|(system, (first_sample_gps, last_sample_gps))| Coverage {
            system,
            first_sample_gps,
            last_sample_gps,
        })
        .collect())
}

impl<'a> Fetcher<'a> {
    fn metadata_path(&self, url: &str) -> PathBuf {
        self.root
            .join("urls")
            .join(format!("{}.json", cache::hash(url.as_bytes())))
    }

    fn get(
        &mut self,
        url: &str,
        role: Role,
        provider: &str,
        check: bool,
    ) -> Result<(Source, Vec<u8>)> {
        let metadata = self.metadata_path(url);
        let cached = if metadata.exists() {
            Some(serde_json::from_slice::<Source>(&cache::read(&metadata)?)?)
        } else {
            None
        };
        if let Some(cached) = &cached {
            let retrieved = chrono::DateTime::parse_from_rfc3339(&cached.fetched_at)?;
            let age = Utc::now().signed_duration_since(retrieved).num_seconds();
            if self.offline || (0..120).contains(&age) {
                let bytes = cache::load_source(self.root, cached)?;
                if check {
                    validate(role, &bytes).with_context(|| format!("invalid source from {url}"))?;
                }
                self.attempts.push(format!("cache: {url}"));
                let mut source = cached.clone();
                source.role = role;
                source.provider = provider.into();
                return Ok((source, bytes));
            }
        }
        ensure!(!self.offline, "source is absent from offline cache: {url}");
        let parsed = Url::parse(url)?;
        ensure!(
            parsed.username().is_empty() && parsed.password().is_none(),
            "credentials in source URLs are not supported"
        );
        let host = parsed.host_str().unwrap_or_default().to_owned();
        ensure!(
            !self.unreachable.contains(&host),
            "{url}: {host} refused a connection earlier in this fetch"
        );
        let mut failures = Vec::new();
        let mut refused = 0;
        for attempt in 0..2 {
            let outcome = (|| -> Result<(Vec<u8>, Option<String>, Option<String>)> {
                if parsed.scheme() == "ftp" {
                    return Ok((ftp(&parsed)?, None, None));
                }
                ensure!(
                    matches!(parsed.scheme(), "http" | "https"),
                    "unsupported source protocol"
                );
                let client = self.client.as_ref().context("HTTP client unavailable")?;
                let response = client.get(
                    &parsed,
                    cached.as_ref().and_then(|source| source.etag.as_deref()),
                    cached
                        .as_ref()
                        .and_then(|source| source.last_modified.as_deref()),
                )?;
                if response.not_modified {
                    let old = cached.as_ref().context("304 without cached object")?;
                    return Ok((
                        cache::load_source(self.root, old)?,
                        old.etag.clone(),
                        old.last_modified.clone(),
                    ));
                }
                Ok((response.bytes, response.etag, response.last_modified))
            })();
            match outcome {
                Ok((bytes, etag, last_modified)) => {
                    let coverage = if check {
                        inspect(role, &bytes)
                            .with_context(|| format!("invalid source from {url}"))?
                    } else {
                        Vec::new()
                    };
                    let source = Source {
                        role,
                        provider: provider.into(),
                        url: url.into(),
                        sha256: cache::hash(&bytes),
                        bytes: bytes.len(),
                        fetched_at: Utc::now().to_rfc3339(),
                        etag,
                        last_modified,
                        coverage,
                        version: None,
                    };
                    cache::store_source(self.root, &source, &bytes)?;
                    cache::atomic_write(&metadata, &serde_json::to_vec_pretty(&source)?)?;
                    self.attempts.push(format!("download: {url}"));
                    return Ok((source, bytes));
                }
                Err(error) => {
                    if refused_connection(&error) {
                        refused += 1;
                    }
                    failures.push(format!("{url}: {error:#}"));
                    if attempt == 0 {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }
        }
        if refused == failures.len() {
            self.unreachable.insert(host);
        }
        self.attempts.extend(failures.clone());
        bail!("{}", failures.join("; "))
    }

    fn chain(&mut self, role: Role, candidates: &[(String, String)]) -> Result<Source> {
        let mut errors = Vec::new();
        for (provider, url) in candidates {
            match self.get(url, role, provider, true) {
                Ok((source, _)) => return Ok(source),
                Err(e) => {
                    let detail = format!("{provider}: {e:#}");
                    self.attempts.push(detail.clone());
                    errors.push(detail);
                }
            }
        }
        bail!("{role:?} unavailable: {}", errors.join("; "))
    }

    fn newest_galileo_almanac(&mut self, urls: &[String], at: Instant) -> Result<Source> {
        let role = Role::GalileoAlmanac;
        let provider = provider(role);
        let latest = at.0 + chrono::Duration::seconds(GALILEO_ISSUE_TOLERANCE_S);
        let mut newest: Option<(Instant, Source)> = None;
        let mut errors = Vec::new();
        for url in urls {
            match self
                .get(url, role, provider, true)
                .and_then(|(source, bytes)| {
                    let issue = crate::almanac::galileo_issue(&bytes)?;
                    ensure!(
                        issue.0 <= latest,
                        "{url}: issued at {}, after the build",
                        issue.0.to_rfc3339()
                    );
                    Ok((issue, source))
                }) {
                Ok((issue, source)) => {
                    if newest.as_ref().is_none_or(|(best, _)| issue.0 > best.0) {
                        newest = Some((issue, source));
                    }
                }
                Err(e) => {
                    let detail = format!("{provider}: {e:#}");
                    self.attempts.push(detail.clone());
                    errors.push(detail);
                }
            }
        }
        newest
            .map(|(_, source)| source)
            .with_context(|| format!("{role:?} unavailable: {}", errors.join("; ")))
    }

    fn celestrak_almanac(&mut self, at: Instant) -> Result<Source> {
        let role = Role::GpsAlmanac;
        let mut candidates = Vec::new();
        for year in [at.0.year(), at.0.year() - 1] {
            let index = format!("{CELESTRAK_YUMA}/{year}/");
            let Ok((_, bytes)) = self.get(&index, role, CELESTRAK_PROVIDER, false) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            for part in text.split(['"', '\'']) {
                let name = published_file(part);
                if let Some(applicable) = yuma_applicability(&name, at)
                    && applicable <= at.gps() + ALMANAC_LEAD_S
                {
                    candidates.push((applicable, format!("{index}{name}")));
                }
            }
            if !candidates.is_empty() {
                break;
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        candidates.reverse();
        let candidates: Vec<_> = candidates
            .into_iter()
            .take(CELESTRAK_CANDIDATES)
            .map(|(_, url)| (CELESTRAK_PROVIDER.to_owned(), url))
            .collect();
        ensure!(
            !candidates.is_empty(),
            "{CELESTRAK_PROVIDER}: no Yuma almanac file in the index"
        );
        self.chain(role, &candidates)
    }

    fn broadcast(&mut self, at: Instant) -> Result<Vec<Source>> {
        let mut sources = Vec::new();
        for (required, candidates) in daily_broadcast(at) {
            match self.chain(Role::Broadcast, &candidates) {
                Ok(source) => sources.push(source),
                Err(e) if required => return Err(e),
                Err(_) => {}
            }
        }
        match self.get(BROADCAST_SNAPSHOT_URL, Role::Broadcast, "brdc", true) {
            Ok((source, _)) => sources.push(source),
            Err(error) => sources.push(
                self.station_snapshot(at)
                    .with_context(|| format!("brdc: {error:#}"))?,
            ),
        }
        Ok(sources)
    }

    fn settled(&mut self, url: &str) -> Result<Vec<u8>> {
        let metadata = self.metadata_path(url);
        if metadata.exists() {
            let cached: Source = serde_json::from_slice(&cache::read(&metadata)?)?;
            if let Ok(bytes) = cache::load_source(self.root, &cached) {
                self.attempts.push(format!("cache: {url}"));
                return Ok(bytes);
            }
        }
        Ok(self.get(url, Role::Broadcast, SNAPSHOT_PROVIDER, false)?.1)
    }

    fn station_snapshot(&mut self, at: Instant) -> Result<Source> {
        let mut files = Vec::new();
        for url in station_urls(at) {
            match self.settled(&url) {
                Ok(bytes) => files.push((published_file(&url), bytes)),
                Err(error) => self
                    .attempts
                    .push(format!("{SNAPSHOT_PROVIDER}: {error:#}")),
            }
        }
        let merged = snapshot::merge(&files, SNAPSHOT_MIN_STATIONS)
            .with_context(|| format!("{SNAPSHOT_PROVIDER} unavailable"))?;
        let source = Source {
            role: Role::Broadcast,
            provider: SNAPSHOT_PROVIDER.into(),
            url: format!("merge:{SNAPSHOT_PROVIDER}/{}", snapshot_name(at)),
            sha256: cache::hash(&merged.bytes),
            bytes: merged.bytes.len(),
            fetched_at: Utc::now().to_rfc3339(),
            etag: None,
            last_modified: None,
            coverage: inspect(Role::Broadcast, &merged.bytes)?,
            version: None,
        };
        cache::store_source(self.root, &source, &merged.bytes)?;
        self.attempts.push(format!(
            "merge: {} with records of {} stations",
            source.url, merged.stations
        ));
        Ok(source)
    }

    fn klobuchar_header(&mut self, at: Instant) -> Option<Source> {
        self.chain(Role::Ionosphere, &klobuchar_header(at)).ok()
    }
}

pub fn run(options: Options<'_>) -> Result<Manifest> {
    fs::create_dir_all(options.cache)?;
    let client = if options.offline {
        None
    } else {
        Some(Client::new()?)
    };
    let mut fetcher = Fetcher {
        root: options.cache,
        client,
        offline: options.offline,
        attempts: Vec::new(),
        unreachable: BTreeSet::new(),
    };
    let mut sources = Vec::new();
    for role in Plan::new(options.flavor, options.systems, options.agnss).sources {
        if let Some(paths) = options.local.get(&role) {
            for path in paths {
                let bytes = cache::read(path)?;
                validate(role, &bytes)?;
                let source = Source {
                    role,
                    provider: provider(role).into(),
                    url: format!("local:{}", path.display()),
                    sha256: cache::hash(&bytes),
                    bytes: bytes.len(),
                    fetched_at: Utc::now().to_rfc3339(),
                    etag: None,
                    last_modified: None,
                    coverage: inspect(role, &bytes)?,
                    version: None,
                };
                cache::store_source(options.cache, &source, &bytes)?;
                sources.push(source);
            }
            continue;
        }
        if let Some(urls) = options.urls.get(&role) {
            sources.push(
                fetcher.chain(
                    role,
                    &urls
                        .iter()
                        .map(|url| (provider(role).into(), url.clone()))
                        .collect::<Vec<_>>(),
                )?,
            );
            continue;
        }
        let attempt = (|| -> Result<()> {
            match role {
                Role::Seed => {
                    let config = "https://configserver-dre.platform.hicloud.com/servicesupport/updateserver/data/com.huawei.higeo_dataModule_PGNSSConfig?type=HW_EEV2";
                    let (_, bytes) = fetcher.get(config, role, "hiee", false)?;
                    let json: serde_json::Value = serde_json::from_slice(&decompress(&bytes)?)?;
                    let entry = if json.is_array() {
                        ensure!(
                            json.as_array().unwrap().len() == 1,
                            "ambiguous seed metadata"
                        );
                        &json[0]
                    } else {
                        &json
                    };
                    let url = entry["downloadUrl"]
                        .as_str()
                        .context("seed metadata has no downloadUrl")?;
                    ensure!(
                        Url::parse(url)?.scheme() == "https",
                        "seed download must use HTTPS"
                    );
                    let mut source = fetcher.get(url, role, "hiee", true)?.0;
                    source.version = match &entry["ver"] {
                        serde_json::Value::String(version) => Some(version.clone()),
                        serde_json::Value::Number(version) => Some(version.to_string()),
                        _ => None,
                    };
                    sources.push(source);
                }
                Role::QzsPrediction => {
                    let (_, bytes) = fetcher.get(
                        "https://sys.qzss.go.jp/dod/api/search/ultra-rapid-sp3",
                        role,
                        "qzu",
                        false,
                    )?;
                    let text = std::str::from_utf8(&bytes)?;
                    let document = roxmltree::Document::parse(text)?;
                    let mut ids: Vec<_> = document
                        .descendants()
                        .filter(|n| n.has_tag_name("id"))
                        .filter_map(|n| n.text())
                        .collect();
                    ids.sort_unstable();
                    ids.reverse();
                    let candidates = ids
                        .into_iter()
                        .take(4)
                        .map(|id| {
                            let mut url =
                                Url::parse("https://sys.qzss.go.jp/dod/api/get/ultra-rapid-sp3")
                                    .unwrap();
                            url.query_pairs_mut().append_pair("id", id);
                            ("qzu".into(), url.to_string())
                        })
                        .collect::<Vec<_>>();
                    sources.push(fetcher.chain(role, &candidates)?);
                }
                Role::GpsAlmanac => {
                    let source = match fetcher.chain(role, &candidates(role, options.at)) {
                        Ok(source) => source,
                        Err(error) => fetcher
                            .celestrak_almanac(options.at)
                            .with_context(|| format!("{error:#}"))?,
                    };
                    sources.push(source);
                }
                Role::GalileoAlmanac => {
                    let base = "https://www.gsc-europa.eu/gsc-products/almanac";
                    let (_, bytes) = fetcher.get(base, role, "gsc", false)?;
                    let text = std::str::from_utf8(&bytes)?;
                    let mut candidates = Vec::new();
                    for part in text.split(['\"', '\'']) {
                        if !part.ends_with(".xml") {
                            continue;
                        }
                        let name = part
                            .rsplit('/')
                            .next()
                            .unwrap_or("")
                            .trim_end_matches(".xml");
                        let date = chrono::NaiveDate::parse_from_str(name, "%Y-%m-%d")
                            .or_else(|_| chrono::NaiveDate::parse_from_str(name, "%d-%m-%Y"));
                        if let Ok(date) = date
                            && date <= options.at.0.date_naive()
                            && let Ok(url) = Url::parse(base)?.join(part)
                        {
                            candidates.push((date, url.to_string()));
                        }
                    }
                    candidates.sort_by_key(|a| std::cmp::Reverse(a.0));
                    let urls: Vec<String> = candidates
                        .into_iter()
                        .take(GALILEO_ALMANAC_CANDIDATES)
                        .map(|(_, url)| url)
                        .collect();
                    sources.push(fetcher.newest_galileo_almanac(&urls, options.at)?);
                }
                Role::Broadcast => sources.extend(fetcher.broadcast(options.at)?),
                Role::Ionosphere => sources.extend(fetcher.klobuchar_header(options.at)),
                Role::Prediction => {
                    let source = fetcher.chain(role, &candidates(role, options.at))?;
                    if source.provider == "code5d" {
                        let mut adjacent = Vec::new();
                        for offset in [-2, -1, 0, 1] {
                            let day = options.at.0 + chrono::Duration::days(offset);
                            let url = code_prediction_url(day);
                            if url != source.url
                                && let Ok((extra, _)) = fetcher.get(&url, role, "code5d", true)
                            {
                                adjacent.push(extra);
                            }
                        }
                        adjacent.push(source);
                        adjacent.sort_by(|a, b| a.url.cmp(&b.url));
                        sources.extend(adjacent);
                    } else {
                        sources.push(source);
                    }
                }
                _ => sources.push(fetcher.chain(role, &candidates(role, options.at))?),
            }
            Ok(())
        })();
        if let Err(error) = attempt {
            if matches!(
                role,
                Role::Prediction | Role::BdsPrediction | Role::QzsPrediction
            ) {
                fetcher.attempts.push(format!(
                    "{role:?}: whole-grid flavor fallback selected after source failure: {error:#}"
                ));
            } else {
                return Err(error);
            }
        }
    }
    let manifest = Manifest {
        version: 1,
        flavor: options.flavor,
        systems: options.systems.to_vec(),
        agnss: options.agnss,
        created_at: Utc::now().to_rfc3339(),
        sources,
        attempts: fetcher.attempts,
    };
    cache::atomic_write(
        &options.cache.join(manifest_name(options.flavor)),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(manifest)
}

pub fn provider(role: Role) -> &'static str {
    match role {
        Role::Seed => "hiee",
        Role::Agnss => "hw",
        Role::Prediction => "code5d",
        Role::BdsPrediction => "wum-nrt",
        Role::QzsPrediction => "qzu",
        Role::Broadcast => "brdc",
        Role::Antex => "igs-antex",
        Role::GpsAlmanac => "navcen",
        Role::GalileoAlmanac => "gsc",
        Role::QzsAlmanac => "qzss-almanac",
        Role::Ionosphere => "brdc-header",
    }
}

fn daily_url(
    provider: &str,
    pool: &str,
    kind: &str,
    day: chrono::DateTime<Utc>,
) -> (String, String) {
    (
        provider.to_string(),
        format!(
            "https://igs.bkg.bund.de/root_ftp/{pool}/BRDC/{}/{:03}/BRDC00WRD_{kind}_{}0000_01D_MN.rnx.gz",
            day.year(),
            day.ordinal(),
            day.format("%Y%j")
        ),
    )
}

fn mirror_daily(day: chrono::DateTime<Utc>) -> Vec<(String, String)> {
    let stamp = day.format("%Y%j");
    let (year, ordinal) = (day.year(), day.ordinal());
    [("dlr-brdm", "BRDM00DLR_S"), ("igs-brdc", "BRDC00IGS_R")]
        .into_iter()
        .flat_map(|(provider, product)| {
            let name = format!("{product}_{stamp}0000_01D_MN.rnx.gz");
            [
                (
                    provider.to_owned(),
                    format!("{ESA_DAILY}/{year}/{ordinal:03}/{name}"),
                ),
                (
                    provider.to_owned(),
                    format!("{WUHAN_DAILY}/{year}/brdc/{name}"),
                ),
            ]
        })
        .collect()
}

fn daily_broadcast(at: Instant) -> [(bool, Vec<(String, String)>); 2] {
    let daily = |day| {
        [("IGS", "S"), ("MGEX", "S"), ("IGS", "R")]
            .map(|(pool, kind)| daily_url("brdc", pool, kind, day))
            .to_vec()
    };
    let previous_day = at.0 - chrono::Duration::days(1);
    let mut previous = daily(previous_day);
    previous.extend(mirror_daily(previous_day));
    [(true, previous), (false, daily(at.0))]
}

fn klobuchar_header(at: Instant) -> Vec<(String, String)> {
    let provider = provider(Role::Ionosphere);
    let previous_day = at.0 - chrono::Duration::days(1);
    let mut candidates = [at.0, previous_day]
        .map(|day| daily_url(provider, "EUREF", "R", day))
        .to_vec();
    candidates.extend(mirror_daily(previous_day));
    candidates
}

fn hour_start(time: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
    let timestamp = time.timestamp();
    chrono::DateTime::from_timestamp(timestamp - timestamp.rem_euclid(SECONDS_PER_HOUR), 0)
        .unwrap_or(time)
}

fn station_urls(at: Instant) -> Vec<String> {
    let current = hour_start(at.0);
    SNAPSHOT_HOURS_BACK
        .iter()
        .flat_map(|back| {
            let start = current - chrono::Duration::hours(*back);
            SNAPSHOT_STATIONS.iter().map(move |station| {
                format!(
                    "{ESA_HOURLY}/{}/{:03}/{:02}/{station}_R_{}{:02}00_01H_MN.rnx.gz",
                    start.year(),
                    start.ordinal(),
                    start.hour(),
                    start.format("%Y%j"),
                    start.hour()
                )
            })
        })
        .collect()
}

fn snapshot_name(at: Instant) -> String {
    let newest = hour_start(at.0) - chrono::Duration::hours(SNAPSHOT_HOURS_BACK[0]);
    format!(
        "brdc_stations_{}_{:02}.rnx",
        newest.format("%Y%j"),
        newest.hour()
    )
}

fn yuma_applicability(name: &str, at: Instant) -> Option<i64> {
    let rest = name
        .strip_prefix("almanac.yuma.week")?
        .strip_suffix(".txt")?;
    let (week, toa) = rest.split_once('.')?;
    if week.len() != 4 || toa.len() != 6 {
        return None;
    }
    let week: i64 = week.parse().ok()?;
    let toa: i64 = toa.parse().ok()?;
    if week >= ALMANAC_WEEKS || toa >= SECONDS_PER_WEEK {
        return None;
    }
    let current = at.gps() / SECONDS_PER_WEEK;
    let full = current + (week - current + ALMANAC_WEEKS / 2).rem_euclid(ALMANAC_WEEKS)
        - ALMANAC_WEEKS / 2;
    Some(full * SECONDS_PER_WEEK + toa)
}

fn published_file(url: &str) -> String {
    url.rsplit('/').next().unwrap_or(url).to_owned()
}

fn candidates(role: Role, at: Instant) -> Vec<(String, String)> {
    let week = at.gps() / 604800;
    let day = at.0;
    match role {
        Role::Agnss=>vec![("hw".into(),"https://geo-dre.platform.dbankcloud.com/higeo/v1/gnssinfo?type=0x0024".into())],
        Role::Prediction=>{
            let mut urls=Vec::new();
            for offset in [0,-1,-2] {let date=day+chrono::Duration::days(offset);urls.push(("code5d".into(),code_prediction_url(date)));}
            urls.extend([("code-ult".into(),format!("{CODE_BASE}/COD.EPH_U")),("igs-ult".into(),format!("https://igs.bkg.bund.de/root_ftp/IGS/products/{week}/IGS0OPSULT_{}{:02}00_02D_15M_ORB.SP3.gz",day.format("%Y%j"),day.hour()/6*6))]);
            for offset in 0..3 {
                let date=day-chrono::Duration::hours(3*offset);
                let gps=Instant(date).gps();
                urls.push(("gfz-ult".into(),format!("ftp://ftp.gfz-potsdam.de/pub/GNSS/products/ultra/w{}/gfu{}{}_{:02}.sp3.gz",gps/604800,gps/604800,gps%604800/86400,date.hour()/3*3)));
            }
            urls.extend(candidates(Role::BdsPrediction,at));
            urls
        }
        Role::BdsPrediction=>(0..6).map(|hours|{let date=day-chrono::Duration::hours(24+hours);let week=Instant(date).gps()/604800;("wum-nrt".into(),format!("ftp://igs.gnsswhu.cn/pub/gps/products/mgex/{week}/WUM0MGXNRT_{}{:02}00_02D_05M_ORB.SP3.gz",date.format("%Y%j"),date.hour()))}).collect(),
        Role::Antex=>vec![("igs-antex".into(),"https://files.igs.org/pub/station/general/igs20.atx".into()),("igs-antex".into(),"https://igs.bkg.bund.de/root_ftp/IGS/igscb/station/general/igs20.atx".into())],
        Role::GpsAlmanac=>vec![("navcen".into(),"https://www.navcen.uscg.gov/sites/default/files/gps/almanac/current_yuma.alm".into())],
        Role::QzsAlmanac=>vec![("qzss-almanac".into(),"https://sys.qzss.go.jp/dod/api/get/almanac".into())],
        _=>Vec::new(),
    }
}

fn ftp(url: &Url) -> Result<Vec<u8>> {
    let host = url.host_str().context("FTP host missing")?;
    let mut connection = None;
    for address in (host, url.port().unwrap_or(21)).to_socket_addrs()?.take(4) {
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_secs(8)) {
            connection = Some(stream);
            break;
        }
    }
    let stream = connection.ok_or(ConnectionRefused)?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;
    let peer = stream.peer_addr()?;
    let mut control = BufReader::new(stream);
    fn response(control: &mut BufReader<TcpStream>) -> Result<(u16, String)> {
        let mut all = String::new();
        let mut first = None;
        for _ in 0..100 {
            let mut line = String::new();
            control.by_ref().take(8193).read_line(&mut line)?;
            ensure!(
                line.len() <= 8192 && line.len() >= 4,
                "invalid FTP response"
            );
            all.push_str(&line);
            let code = line[..3].parse::<u16>().ok();
            if first.is_none() {
                first = code;
            }
            if code == first && line.as_bytes()[3] == b' ' {
                return Ok((code.context("invalid FTP code")?, all));
            }
        }
        bail!("FTP response too long")
    }
    fn command(control: &mut BufReader<TcpStream>, text: &str) -> Result<(u16, String)> {
        ensure!(!text.contains(['\r', '\n']), "invalid FTP command");
        write!(control.get_mut(), "{text}\r\n")?;
        response(control)
    }
    ensure!(response(&mut control)?.0 == 220, "FTP greeting refused");
    let user = command(&mut control, "USER anonymous")?;
    if user.0 == 331 {
        ensure!(
            command(&mut control, "PASS weinav-forge@").is_ok_and(|r| r.0 == 230),
            "anonymous FTP login refused"
        );
    } else {
        ensure!(user.0 == 230, "anonymous FTP login refused");
    }
    ensure!(
        command(&mut control, "TYPE I")?.0 == 200,
        "FTP binary mode refused"
    );
    let passive = command(&mut control, "EPSV")?;
    let port = if passive.0 == 229 {
        passive
            .1
            .split('|')
            .nth(3)
            .context("invalid EPSV response")?
            .parse::<u16>()?
    } else {
        let passive = command(&mut control, "PASV")?;
        ensure!(passive.0 == 227, "FTP passive mode refused");
        let address = passive
            .1
            .split_once('(')
            .and_then(|(_, s)| s.split_once(')'))
            .context("invalid PASV response")?
            .0;
        let fields = address
            .split(',')
            .map(|s| s.trim().parse::<u8>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(fields.len() == 6, "invalid PASV address");
        u16::from(fields[4]) * 256 + u16::from(fields[5])
    };
    let mut data = TcpStream::connect_timeout(
        &std::net::SocketAddr::new(peer.ip(), port),
        Duration::from_secs(8),
    )?;
    data.set_read_timeout(Some(Duration::from_secs(30)))?;
    ensure!(
        matches!(
            command(&mut control, &format!("RETR {}", url.path()))?.0,
            125 | 150
        ),
        "FTP file unavailable"
    );
    let mut bytes = Vec::new();
    (&mut data)
        .take(MAX_DECODED_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_DECODED_BYTES,
        "FTP product exceeds size limit"
    );
    drop(data);
    ensure!(response(&mut control)?.0 == 226, "FTP transfer incomplete");
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn code_sources_use_direct_storage_across_year_boundaries() -> Result<()> {
        let at = Instant::parse("2026-01-01T00:00:00Z")?;
        let urls = candidates(Role::Prediction, at);
        for (index, stamp) in ["2026001", "2025365", "2025364"].iter().enumerate() {
            assert_eq!(urls[index].0, "code5d");
            assert_eq!(
                urls[index].1,
                format!(
                    "https://zhw-b.s3.cloud.switch.ch/aiub/CODE/COD0OPSPRD_{stamp}0000_05D_05M_ORB.SP3"
                )
            );
        }
        assert_eq!(
            urls[3].1,
            "https://zhw-b.s3.cloud.switch.ch/aiub/CODE/COD.EPH_U"
        );
        Ok(())
    }

    #[test]
    fn only_the_previous_day_broadcast_file_is_required() -> Result<()> {
        let at = Instant::parse("2026-01-01T00:08:00Z")?;
        let files = daily_broadcast(at);
        let days: Vec<_> = files
            .iter()
            .map(|(required, urls)| (*required, urls[0].1.rsplit('_').nth(2).map(str::to_owned)))
            .collect();
        assert_eq!(
            days,
            [
                (true, Some("20253650000".to_owned())),
                (false, Some("20260010000".to_owned())),
            ]
        );
        assert!(files[0].1[0].1.contains("/IGS/BRDC/2025/365/BRDC00WRD_S_"));
        assert!(
            files
                .iter()
                .flat_map(|(_, urls)| urls)
                .all(|(_, url)| !url.contains("/EUREF/"))
        );
        let header = klobuchar_header(at);
        assert!(
            header[0]
                .1
                .contains("/EUREF/BRDC/2026/001/BRDC00WRD_R_2026001")
        );
        assert!(
            header[1]
                .1
                .contains("/EUREF/BRDC/2025/365/BRDC00WRD_R_2025365")
        );
        let mirrors = [
            (
                "dlr-brdm",
                "ftp://gssc.esa.int/gnss/data/daily/2025/365/BRDM00DLR_S_20253650000_01D_MN.rnx.gz",
            ),
            (
                "dlr-brdm",
                "ftp://igs.gnsswhu.cn/pub/gps/data/daily/2025/brdc/BRDM00DLR_S_20253650000_01D_MN.rnx.gz",
            ),
            (
                "igs-brdc",
                "ftp://gssc.esa.int/gnss/data/daily/2025/365/BRDC00IGS_R_20253650000_01D_MN.rnx.gz",
            ),
            (
                "igs-brdc",
                "ftp://igs.gnsswhu.cn/pub/gps/data/daily/2025/brdc/BRDC00IGS_R_20253650000_01D_MN.rnx.gz",
            ),
        ]
        .map(|(provider, url)| (provider.to_owned(), url.to_owned()));
        assert_eq!(files[0].1[3..], mirrors);
        assert_eq!(files[1].1.len(), 3);
        assert_eq!(header[2..], mirrors);
        Ok(())
    }

    #[test]
    fn station_snapshot_uses_the_two_previous_hours_across_a_day_boundary() -> Result<()> {
        let at = Instant::parse("2026-01-01T00:08:00Z")?;
        let urls = station_urls(at);
        assert_eq!(
            urls.len(),
            SNAPSHOT_HOURS_BACK.len() * SNAPSHOT_STATIONS.len()
        );
        assert_eq!(
            urls[0],
            "ftp://gssc.esa.int/gnss/data/hourly/2025/365/23/TKBG00JPN_R_20253652300_01H_MN.rnx.gz"
        );
        assert_eq!(
            urls[SNAPSHOT_STATIONS.len()],
            "ftp://gssc.esa.int/gnss/data/hourly/2025/365/22/TKBG00JPN_R_20253652200_01H_MN.rnx.gz"
        );
        assert_eq!(snapshot_name(at), "brdc_stations_2025365_23.rnx");
        assert_eq!(
            published_file(&urls[0]),
            "TKBG00JPN_R_20253652300_01H_MN.rnx.gz"
        );
        Ok(())
    }

    fn navigation(header: &str) -> Vec<u8> {
        let mut text = format!(
            "{:>9}{:51}RINEX VERSION / TYPE\n{header}{:60}END OF HEADER\n",
            "3.05", "", ""
        );
        text.push_str(&format!(
            "G01 2025 12 31 22 00 00{:19.12E}{:19.12E}{:19.12E}\n",
            0.0, 0.0, 0.0
        ));
        for _ in 0..7 {
            text.push_str(&format!(
                "    {:19.12E}{:19.12E}{:19.12E}{:19.12E}\n",
                1.0, 1.0, 1.0, 1.0
            ));
        }
        text.into_bytes()
    }

    fn offline_copy(root: &Path, url: &str, role: Role, bytes: &[u8]) -> Result<()> {
        let source = Source {
            role,
            provider: provider(role).into(),
            url: url.into(),
            sha256: cache::hash(bytes),
            bytes: bytes.len(),
            fetched_at: Utc::now().to_rfc3339(),
            etag: None,
            last_modified: None,
            coverage: Vec::new(),
            version: None,
        };
        cache::store_source(root, &source, bytes)?;
        cache::atomic_write(
            &root
                .join("urls")
                .join(format!("{}.json", cache::hash(url.as_bytes()))),
            &serde_json::to_vec(&source)?,
        )
    }

    #[test]
    fn missing_build_day_files_leave_the_day_before_and_its_klobuchar_header() -> Result<()> {
        let root = tempfile::tempdir()?;
        let at = Instant::parse("2026-01-01T00:08:00Z")?;
        let with_header = navigation(&format!(
            "GPSA {:12.4E}{:12.4E}{:12.4E}{:12.4E}{:7}IONOSPHERIC CORR\nGPSB {:12.4E}{:12.4E}{:12.4E}{:12.4E}{:7}IONOSPHERIC CORR\n",
            1e-8, 0.0, 0.0, 0.0, "", 9e4, 0.0, 0.0, 0.0, ""
        ));
        let without_header = navigation("");
        let previous_day = daily_broadcast(at)[0].1[0].1.clone();
        let header_urls: Vec<_> = klobuchar_header(at)
            .into_iter()
            .take(2)
            .map(|(_, url)| url)
            .collect();
        let [build_day_header, previous_day_header] = <[String; 2]>::try_from(header_urls)
            .map_err(|_| anyhow::anyhow!("two Klobuchar header candidates"))?;
        offline_copy(root.path(), &previous_day, Role::Broadcast, &without_header)?;
        offline_copy(
            root.path(),
            BROADCAST_SNAPSHOT_URL,
            Role::Broadcast,
            &without_header,
        )?;
        offline_copy(
            root.path(),
            &build_day_header,
            Role::Ionosphere,
            &without_header,
        )?;
        offline_copy(
            root.path(),
            &previous_day_header,
            Role::Ionosphere,
            &with_header,
        )?;
        let mut fetcher = Fetcher {
            root: root.path(),
            client: None,
            offline: true,
            attempts: vec![],
            unreachable: BTreeSet::new(),
        };
        let mut sources = fetcher.broadcast(at)?;
        sources.extend(fetcher.klobuchar_header(at));
        let fetched: Vec<_> = sources.iter().map(|s| (s.role, s.url.as_str())).collect();
        assert_eq!(
            fetched,
            [
                (Role::Broadcast, previous_day.as_str()),
                (Role::Broadcast, BROADCAST_SNAPSHOT_URL),
                (Role::Ionosphere, previous_day_header.as_str()),
            ]
        );
        assert!(Role::Broadcast < Role::Ionosphere);
        assert!(
            fetcher
                .attempts
                .iter()
                .any(|a| a.contains(&build_day_header) && a.contains("Klobuchar"))
        );
        Ok(())
    }

    fn offline_fetcher(root: &Path) -> Fetcher<'_> {
        Fetcher {
            root,
            client: None,
            offline: true,
            attempts: vec![],
            unreachable: BTreeSet::new(),
        }
    }

    #[test]
    fn without_bkg_a_mirror_day_and_merged_station_files_are_used() -> Result<()> {
        let at = Instant::parse("2026-01-01T00:08:00Z")?;
        let file = navigation("");
        let [(_, previous), _] = daily_broadcast(at);
        let mirror = previous[3].1.clone();
        let stations = station_urls(at);
        let merged_url = format!("merge:{SNAPSHOT_PROVIDER}/{}", snapshot_name(at));
        let root = tempfile::tempdir()?;
        offline_copy(root.path(), &mirror, Role::Broadcast, &file)?;
        for url in &stations[..SNAPSHOT_MIN_STATIONS] {
            offline_copy(root.path(), url, Role::Broadcast, &file)?;
        }
        let mut fetcher = offline_fetcher(root.path());
        let sources = fetcher.broadcast(at)?;
        let fetched: Vec<_> = sources
            .iter()
            .map(|s| (s.provider.as_str(), s.url.as_str()))
            .collect();
        assert_eq!(
            fetched,
            [
                ("dlr-brdm", mirror.as_str()),
                (SNAPSHOT_PROVIDER, merged_url.as_str())
            ]
        );
        assert_eq!(
            cache::load_source(root.path(), &sources[1])?.len(),
            sources[1].bytes
        );
        assert!(
            fetcher
                .attempts
                .iter()
                .any(|a| a.ends_with(&format!("with records of {SNAPSHOT_MIN_STATIONS} stations")))
        );
        let thin = tempfile::tempdir()?;
        offline_copy(thin.path(), &mirror, Role::Broadcast, &file)?;
        for url in &stations[..SNAPSHOT_MIN_STATIONS - 1] {
            offline_copy(thin.path(), url, Role::Broadcast, &file)?;
        }
        let error = offline_fetcher(thin.path())
            .broadcast(at)
            .err()
            .context("a thin station snapshot was accepted")?;
        assert!(format!("{error:#}").contains(&format!(
            "{} station navigation files have usable records; {SNAPSHOT_MIN_STATIONS} are needed",
            SNAPSHOT_MIN_STATIONS - 1
        )));
        Ok(())
    }

    #[test]
    fn the_newest_galileo_almanac_issue_wins_over_the_newest_file_name() -> Result<()> {
        let root = tempfile::tempdir()?;
        let almanac = |issue: &str| {
            format!("<signalData><header><issueDate>{issue}</issueDate></header></signalData>")
                .into_bytes()
        };
        let urls = [
            "https://gsc.example/2026-10-09.xml",
            "https://gsc.example/2026-10-06.xml",
            "https://gsc.example/2026-10-02.xml",
            "https://gsc.example/broken.xml",
        ]
        .map(str::to_owned);
        for (url, issue) in urls.iter().zip([
            "2026-10-01T09:59:59.0Z",
            "2026-10-06T09:59:59.0Z",
            "2026-10-02T09:59:59.0Z",
        ]) {
            offline_copy(root.path(), url, Role::GalileoAlmanac, &almanac(issue))?;
        }
        offline_copy(
            root.path(),
            &urls[3],
            Role::GalileoAlmanac,
            b"<signalData/>",
        )?;
        let mut fetcher = offline_fetcher(root.path());
        let at = Instant::parse("2026-10-11T00:00:00Z")?;
        assert_eq!(fetcher.newest_galileo_almanac(&urls, at)?.url, urls[1]);
        assert!(
            fetcher
                .attempts
                .iter()
                .any(|a| a.contains("Galileo issue date missing"))
        );
        assert!(fetcher.newest_galileo_almanac(&urls[3..], at).is_err());
        let replay = Instant::parse("2026-10-05T00:00:00Z")?;
        assert_eq!(fetcher.newest_galileo_almanac(&urls, replay)?.url, urls[2]);
        assert!(
            fetcher
                .attempts
                .iter()
                .any(|a| a.contains("2026-10-06.xml: issued at 2026-10-06T09:59:59"))
        );
        let early = Instant::parse("2026-10-01T09:30:00Z")?;
        assert!(fetcher.newest_galileo_almanac(&urls, early).is_err());
        Ok(())
    }

    #[test]
    fn yuma_names_resolve_the_ten_bit_week_near_the_build() -> Result<()> {
        let at = Instant::parse("2026-10-16T08:00:00Z")?;
        let week = at.gps() / SECONDS_PER_WEEK;
        assert_eq!(week % ALMANAC_WEEKS, 392);
        assert_eq!(
            yuma_applicability("almanac.yuma.week0393.061440.txt", at),
            Some((week + 1) * SECONDS_PER_WEEK + 61_440)
        );
        assert_eq!(
            yuma_applicability("almanac.yuma.week0392.589824.txt", at),
            Some(week * SECONDS_PER_WEEK + 589_824)
        );
        let rollover = Instant::from_gps((week - 392 + ALMANAC_WEEKS) * SECONDS_PER_WEEK)?;
        assert_eq!(
            yuma_applicability("almanac.yuma.week1023.589824.txt", rollover),
            Some((week - 392 + ALMANAC_WEEKS - 1) * SECONDS_PER_WEEK + 589_824)
        );
        assert_eq!(
            yuma_applicability("almanac.yuma.week0000.061440.txt", rollover),
            Some((week - 392 + ALMANAC_WEEKS) * SECONDS_PER_WEEK + 61_440)
        );
        for name in [
            "almanac.sem.week0393.061440.txt",
            "almanac.yuma.week393.061440.txt",
            "almanac.yuma.week0393.604800.txt",
            "almanac.yuma.week1024.061440.txt",
            "almanac.yuma.week0393.061440.al3",
        ] {
            assert_eq!(yuma_applicability(name, at), None, "{name}");
        }
        Ok(())
    }

    #[test]
    fn celestrak_gives_the_newest_yuma_file_that_is_not_ahead_of_the_build() -> Result<()> {
        let at = Instant::parse("2026-10-16T08:00:00Z")?;
        let yuma = b"******** Week 393 almanac for PRN-01 ********\nID:                         01\nEccentricity:               0.1718759537E-001\n";
        let names = [
            "almanac.yuma.week0392.589824.txt",
            "almanac.yuma.week0393.061440.txt",
            "almanac.yuma.week0394.061440.txt",
        ];
        let index: String = names
            .iter()
            .map(|name| format!("<a href=\"/GPS/almanac/Yuma/2026/{name}\">{name}</a><br>"))
            .collect();
        let root = tempfile::tempdir()?;
        let index_url = format!("{CELESTRAK_YUMA}/2026/");
        offline_copy(root.path(), &index_url, Role::GpsAlmanac, index.as_bytes())?;
        for name in &names[..2] {
            offline_copy(
                root.path(),
                &format!("{index_url}{name}"),
                Role::GpsAlmanac,
                yuma,
            )?;
        }
        let mut fetcher = offline_fetcher(root.path());
        let source = fetcher.celestrak_almanac(at)?;
        assert_eq!(source.provider, CELESTRAK_PROVIDER);
        assert_eq!(source.url, format!("{index_url}{}", names[1]));
        let earlier = Instant::parse("2026-10-11T00:00:00Z")?;
        let source = offline_fetcher(root.path()).celestrak_almanac(earlier)?;
        assert_eq!(source.url, format!("{index_url}{}", names[0]));
        let january = tempfile::tempdir()?;
        let previous_year = format!("{CELESTRAK_YUMA}/2025/");
        let last = "almanac.yuma.week0351.589824.txt";
        offline_copy(
            january.path(),
            &previous_year,
            Role::GpsAlmanac,
            format!("<a href=\"/GPS/almanac/Yuma/2025/{last}\">{last}</a>").as_bytes(),
        )?;
        offline_copy(
            january.path(),
            &format!("{previous_year}{last}"),
            Role::GpsAlmanac,
            yuma,
        )?;
        let new_year = Instant::parse("2026-01-01T06:00:00Z")?;
        let source = offline_fetcher(january.path()).celestrak_almanac(new_year)?;
        assert_eq!(source.url, format!("{previous_year}{last}"));
        Ok(())
    }

    #[test]
    fn a_host_that_refuses_connections_is_not_tried_again() -> Result<()> {
        let root = tempfile::tempdir()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let served = listener.local_addr()?;
        let server = std::thread::spawn(move || -> Result<()> {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut reader = BufReader::new(stream.try_clone()?);
                let mut line = String::new();
                while line != "\r\n" {
                    line.clear();
                    reader.read_line(&mut line)?;
                }
                stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )?;
            }
            Ok(())
        });
        let closed = TcpListener::bind("127.0.0.2:0")?.local_addr()?;
        let mut fetcher = Fetcher {
            root: root.path(),
            client: Some(Client::new()?),
            offline: false,
            attempts: vec![],
            unreachable: BTreeSet::new(),
        };
        assert!(
            fetcher
                .get(
                    &format!("http://{served}/missing"),
                    Role::Seed,
                    "test",
                    false
                )
                .is_err()
        );
        server.join().expect("HTTP test thread panicked")?;
        assert!(fetcher.unreachable.is_empty());
        assert!(
            fetcher
                .get(&format!("http://{closed}/first"), Role::Seed, "test", false)
                .is_err()
        );
        assert!(fetcher.unreachable.contains("127.0.0.2"));
        let error = fetcher
            .get(
                &format!("http://{closed}/second"),
                Role::Seed,
                "test",
                false,
            )
            .err()
            .context("an unreachable host was tried again")?;
        assert!(
            error
                .to_string()
                .contains("127.0.0.2 refused a connection earlier in this fetch")
        );
        Ok(())
    }

    #[test]
    fn revalidates_http_encoding_and_reuses_hash_verified_cache() -> Result<()> {
        let root = tempfile::tempdir()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}/source", listener.local_addr()?);
        let server = std::thread::spawn(move || -> Result<()> {
            for index in 0..2 {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut reader = BufReader::new(stream.try_clone()?);
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line)?;
                    if line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line);
                }
                assert!(headers.to_lowercase().contains("accept-encoding: identity"));
                if index == 0 {
                    let mut encoder =
                        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                    encoder.write_all(b"test source bytes")?;
                    let body = encoder.finish()?;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Encoding: gzip\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n",
                        body.len()
                    )?;
                    stream.write_all(&body)?;
                } else {
                    assert!(headers.to_lowercase().contains("if-none-match: \"v1\""));
                    stream.write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n")?;
                }
            }
            Ok(())
        });
        let mut fetcher = Fetcher {
            root: root.path(),
            client: Some(Client::new()?),
            offline: false,
            attempts: vec![],
            unreachable: BTreeSet::new(),
        };
        let (mut source, bytes) = fetcher.get(&url, Role::Seed, "test", false)?;
        assert_eq!(bytes, b"test source bytes");
        source.fetched_at = "2000-01-01T00:00:00Z".into();
        cache::atomic_write(
            &root
                .path()
                .join("urls")
                .join(format!("{}.json", cache::hash(url.as_bytes()))),
            &serde_json::to_vec(&source)?,
        )?;
        let (next, bytes) = fetcher.get(&url, Role::Seed, "test", false)?;
        assert_eq!(source.sha256, next.sha256);
        assert_eq!(bytes, b"test source bytes");
        server.join().expect("HTTP test thread panicked")?;
        fetcher.client = None;
        fetcher.offline = true;
        assert_eq!(fetcher.get(&url, Role::Seed, "test", false)?.1, bytes);
        assert!(
            fetcher
                .get("http://invalid.example/missing", Role::Seed, "test", false)
                .is_err()
        );
        fs::write(cache::object_path(root.path(), &next.sha256)?, b"corrupt")?;
        assert!(fetcher.get(&url, Role::Seed, "test", false).is_err());
        Ok(())
    }
}
