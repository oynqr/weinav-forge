use crate::seed::{MAX_DECODED_BYTES, decompress};
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use std::time::Duration;
use tokio::runtime::Runtime;
use url::Url;
use wreq::{
    ClientBuilder, Emulation, IntoEmulation, StatusCode,
    header::{HeaderMap, HeaderValue, OrigHeaderMap},
    tls::AlpnProtocol,
};

const HUAWEI_USER_AGENT: &str = "okhttp/3.14.9-h0.CBGCloud.WiseCloudPanshi.NetworkKit.r712";
const HUAWEI_CIPHERS: &str = concat!(
    "TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256:TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256:",
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384:TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384:",
    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256"
);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Config,
    Agnss,
    Other,
}

impl Endpoint {
    fn from_url(url: &Url) -> Self {
        match url.host_str() {
            Some("configserver-dre.platform.hicloud.com" | "configdownload-dre.dbankcdn.com") => {
                Self::Config
            }
            Some("geo-dre.platform.dbankcloud.com") => Self::Agnss,
            _ => Self::Other,
        }
    }

    fn headers(self) -> Result<(HeaderMap, OrigHeaderMap)> {
        let mut headers = HeaderMap::new();
        let mut order = OrigHeaderMap::new();
        let mut insert = |name: &'static str, value: &str| -> Result<()> {
            headers.insert(name, HeaderValue::from_str(value)?);
            order.insert(name);
            Ok(())
        };
        match self {
            Self::Config => {
                insert("traceId", &uuid::Uuid::new_v4().to_string())?;
                insert("App-ID", "HealthApp")?;
            }
            Self::Agnss => {
                insert("X-Device-Type", "SportsHealth")?;
                insert("X-Request-ID", &uuid::Uuid::new_v4().to_string())?;
                insert("Content-Type", "application/json")?;
            }
            Self::Other => {}
        }
        if self == Self::Other {
            insert("Accept-Encoding", "identity")?;
            insert(
                "User-Agent",
                concat!("weinav-forge/", env!("CARGO_PKG_VERSION")),
            )?;
        } else {
            order.insert("Host");
            headers.insert("Connection", HeaderValue::from_static("Keep-Alive"));
            order.insert("Connection");
            headers.insert("Accept-Encoding", HeaderValue::from_static("gzip"));
            order.insert("Accept-Encoding");
            headers.insert("User-Agent", HeaderValue::from_static(HUAWEI_USER_AGENT));
            order.insert("User-Agent");
        }
        Ok((headers, order))
    }
}

fn huawei_emulation() -> Emulation {
    let mut profile = wreq_util::Emulation::builder()
        .profile(wreq_util::Profile::OkHttp3_14)
        .headers(false)
        .build()
        .into_emulation();
    let tls = profile.tls_options.as_mut().expect("OkHttp TLS profile");
    tls.cipher_list = Some(HUAWEI_CIPHERS.into());
    tls.alpn_protocols = Some(vec![AlpnProtocol::HTTP2, AlpnProtocol::HTTP1].into());
    tls.alps_protocols = None;
    tls.grease_enabled = Some(false);
    tls.permute_extensions = Some(false);
    tls.session_ticket = true;
    tls.pre_shared_key = true;
    let http2 = profile
        .http2_options
        .as_mut()
        .expect("OkHttp HTTP/2 profile");
    http2.initial_stream_id = Some(3);
    http2.max_frame_size = None;
    http2.max_header_list_size = None;
    profile
}

fn builder() -> ClientBuilder {
    wreq::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(REQUEST_TIMEOUT)
        .redirect(wreq::redirect::Policy::none())
        .referer(false)
}

fn huawei_builder() -> ClientBuilder {
    builder()
        .emulation(huawei_emulation())
        .pool_idle_timeout(Duration::from_secs(120))
        .pool_max_idle_per_host(5)
        .tcp_happy_eyeballs_timeout(Duration::from_millis(500))
}

pub struct Client {
    runtime: Runtime,
    huawei: wreq::Client,
    other: wreq::Client,
}

pub struct Response {
    pub not_modified: bool,
    pub bytes: Vec<u8>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl Client {
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let huawei = huawei_builder().build()?;
        let other = builder().build()?;
        Ok(Self {
            runtime,
            huawei,
            other,
        })
    }

    pub fn get(&self, url: &Url, etag: Option<&str>, modified: Option<&str>) -> Result<Response> {
        self.runtime.block_on(async {
            tokio::time::timeout(REQUEST_TIMEOUT, self.download(url, etag, modified))
                .await
                .context("HTTP download timed out")?
        })
    }

    async fn download(
        &self,
        url: &Url,
        etag: Option<&str>,
        modified: Option<&str>,
    ) -> Result<Response> {
        let mut destination = url.clone();
        for redirect in 0..=MAX_REDIRECTS {
            ensure!(
                matches!(destination.scheme(), "http" | "https")
                    && destination.username().is_empty()
                    && destination.password().is_none(),
                "invalid HTTP destination"
            );
            let endpoint = Endpoint::from_url(&destination);
            let client = if endpoint == Endpoint::Other {
                &self.other
            } else {
                &self.huawei
            };
            let (headers, order) = endpoint.headers()?;
            let mut request = client
                .get(destination.as_str())
                .headers(headers)
                .orig_headers(order);
            if destination.origin() == url.origin() {
                if let Some(etag) = etag {
                    request = request.header("If-None-Match", etag);
                }
                if let Some(modified) = modified {
                    request = request.header("If-Modified-Since", modified);
                }
            }
            let response = request.send().await?.error_for_status()?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                ensure!(redirect < MAX_REDIRECTS, "too many HTTP redirects");
                let location = response
                    .headers()
                    .get("location")
                    .context("redirect without location")?
                    .to_str()?;
                destination = destination.join(location)?;
                continue;
            }
            let not_modified = response.status() == StatusCode::NOT_MODIFIED;
            ensure!(
                not_modified || response.status().is_success(),
                "unexpected HTTP status {}",
                response.status()
            );
            let header = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            };
            let etag = header("etag");
            let last_modified = header("last-modified");
            let encoding = header("content-encoding").unwrap_or_else(|| "identity".into());
            ensure!(
                response
                    .content_length()
                    .is_none_or(|n| n <= MAX_DECODED_BYTES),
                "download exceeds size limit"
            );
            let mut bytes = Vec::new();
            if !not_modified {
                let mut stream = response.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk?;
                    ensure!(
                        bytes.len() as u64 + chunk.len() as u64 <= MAX_DECODED_BYTES,
                        "download exceeds size limit"
                    );
                    bytes.extend_from_slice(&chunk);
                }
                if encoding.eq_ignore_ascii_case("gzip") {
                    ensure!(bytes.starts_with(&[0x1f, 0x8b]), "invalid HTTP gzip body");
                    bytes = decompress(&bytes)?;
                } else {
                    ensure!(
                        encoding.eq_ignore_ascii_case("identity"),
                        "unsupported HTTP content encoding {encoding}"
                    );
                }
            }
            return Ok(Response {
                not_modified,
                bytes,
                etag,
                last_modified,
            });
        }
        bail!("too many HTTP redirects")
    }
}
