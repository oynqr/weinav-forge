use crate::http::Client;
use crate::{
    cache::{self, Coverage, Manifest, Source},
    policy::{Flavor, Plan, Role, System},
    rinex::Broadcast,
    rtcm,
    seed::{MAX_DECODED_BYTES, Seed, decompress},
    sp3::{Antex, Sp3},
    time::Instant,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::{Datelike, Timelike, Utc};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;

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
    fn get(
        &mut self,
        url: &str,
        role: Role,
        provider: &str,
        check: bool,
    ) -> Result<(Source, Vec<u8>)> {
        let metadata = self
            .root
            .join("urls")
            .join(format!("{}.json", cache::hash(url.as_bytes())));
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
                    validate(role, &bytes)?;
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
        let mut failures = Vec::new();
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
                    };
                    cache::store_source(self.root, &source, &bytes)?;
                    cache::atomic_write(&metadata, &serde_json::to_vec_pretty(&source)?)?;
                    self.attempts.push(format!("download: {url}"));
                    return Ok((source, bytes));
                }
                Err(error) => {
                    failures.push(format!("{url}: {error:#}"));
                    if attempt == 0 {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }
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
                    sources.push(fetcher.get(url, role, "hiee", true)?.0);
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
                    sources.push(
                        fetcher.chain(
                            role,
                            &candidates
                                .into_iter()
                                .take(3)
                                .map(|(_, url)| ("gsc".into(), url))
                                .collect::<Vec<_>>(),
                        )?,
                    );
                }
                Role::Broadcast => {
                    for day in [options.at.0 - chrono::Duration::days(1), options.at.0] {
                        let stamp = day.format("%Y%j");
                        let candidates=[("IGS","S"),("MGEX","S"),("IGS","R"),("EUREF","R")].map(|(pool,kind)|("brdc".into(),format!("https://igs.bkg.bund.de/root_ftp/{pool}/BRDC/{}/{:03}/BRDC00WRD_{kind}_{stamp}0000_01D_MN.rnx.gz",day.year(),day.ordinal())));
                        sources.push(fetcher.chain(role, &candidates)?);
                    }
                    sources.push(
                        fetcher
                            .get(
                                "https://igs.bkg.bund.de/root_ftp/NTRIP/BRDC/brdc_last.rnx.Z",
                                role,
                                "brdc",
                                true,
                            )?
                            .0,
                    );
                }
                Role::Prediction => {
                    let source = fetcher.chain(role, &candidates(role, options.at))?;
                    if source.provider == "code5d" {
                        let mut adjacent = Vec::new();
                        for offset in [-2, -1, 0, 1] {
                            let day = options.at.0 + chrono::Duration::days(offset);
                            let url = format!(
                                "https://www.aiub.unibe.ch/download/CODE/COD0OPSPRD_{}0000_05D_05M_ORB.SP3",
                                day.format("%Y%j")
                            );
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
    }
}

fn candidates(role: Role, at: Instant) -> Vec<(String, String)> {
    let week = at.gps() / 604800;
    let day = at.0;
    match role {
        Role::Agnss=>vec![("hw".into(),"https://geo-dre.platform.dbankcloud.com/higeo/v1/gnssinfo?type=0x0024".into())],
        Role::Prediction=>{
            let mut urls=Vec::new();
            for offset in [0,-1,-2] {let date=day+chrono::Duration::days(offset);urls.push(("code5d".into(),format!("https://www.aiub.unibe.ch/download/CODE/COD0OPSPRD_{}0000_05D_05M_ORB.SP3",date.format("%Y%j"))));}
            urls.extend([("code-ult".into(),"https://www.aiub.unibe.ch/download/CODE/COD.EPH_U".into()),("igs-ult".into(),format!("https://igs.bkg.bund.de/root_ftp/IGS/products/{week}/IGS0OPSULT_{}{:02}00_02D_15M_ORB.SP3.gz",day.format("%Y%j"),day.hour()/6*6))]);
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
    let stream = connection.context("FTP connection failed")?;
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
