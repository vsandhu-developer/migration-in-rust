use crate::{
    error::{require, Error, Result},
    manifest::{Config, Manifest},
};
use futures::StreamExt;
use reqwest::{
    header::{HeaderMap, AUTHORIZATION, CONTENT_TYPE, LOCATION, RETRY_AFTER},
    Method,
};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use url::Url;

#[derive(Clone)]
pub struct Boundary {
    origins: Vec<String>,
    local: Option<(String, IpAddr)>,
}
impl Boundary {
    pub fn new(origins: Vec<String>, local: Option<(String, IpAddr)>) -> Result<Self> {
        require(!origins.is_empty(), "network_origin_missing")?;
        if let Some((_, ip)) = &local {
            require(
                match ip {
                    IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
                    IpAddr::V6(ip) => ip.is_loopback(),
                },
                "local_address_forbidden",
            )?;
        }
        for o in &origins {
            let u = Url::parse(o).map_err(|_| Error::new("network_origin_invalid"))?;
            require(
                u.origin().ascii_serialization() == *o
                    && (u.scheme() == "https" || local.as_ref().is_some_and(|(l, _)| l == o)),
                "network_origin_invalid",
            )?;
        }
        Ok(Self { origins, local })
    }
    pub fn target(config: &Config) -> Result<Self> {
        let origin = config.target.origin().ascii_serialization();
        let local = if config.local {
            let u = config
                .local_target_origin
                .as_ref()
                .ok_or_else(|| Error::new("local_target_missing"))?;
            require(
                u.origin() == config.target.origin(),
                "local_target_mismatch",
            )?;
            Some((
                origin.clone(),
                config
                    .local_target_address
                    .ok_or_else(|| Error::new("local_target_missing"))?,
            ))
        } else {
            None
        };
        Self::new(vec![origin], local)
    }
    pub fn source(config: &Config, manifest: &Manifest) -> Result<Self> {
        let origins = manifest.raw["sourceOrigins"]
            .as_array()
            .ok_or_else(|| Error::new("source_origins_invalid"))?
            .iter()
            .map(|s| s.as_str().unwrap_or("").to_owned())
            .collect();
        let local = if config.local && manifest.raw["sourceAuthority"] == "synthetic.local" {
            Some((
                config
                    .local_source_origin
                    .as_ref()
                    .ok_or_else(|| Error::new("local_source_missing"))?
                    .origin()
                    .ascii_serialization(),
                config
                    .local_source_address
                    .ok_or_else(|| Error::new("local_source_missing"))?,
            ))
        } else {
            None
        };
        require(
            config.local || manifest.raw["sourceAuthority"] != "synthetic.local",
            "synthetic_source_forbidden",
        )?;
        Self::new(origins, local)
    }
    pub fn validate_url(&self, url: &Url) -> Result<()> {
        require(
            self.origins.contains(&url.origin().ascii_serialization())
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
                && url.as_str().len() <= 2048,
            "network_origin_denied",
        )
    }
    pub async fn addresses(&self, url: &Url) -> Result<Vec<SocketAddr>> {
        self.validate_url(url)?;
        let host = url
            .host_str()
            .ok_or_else(|| Error::new("network_host_invalid"))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| Error::new("network_port_invalid"))?;
        let addresses = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| Error::new("dns_timeout"))?
        .map_err(|_| Error::new("dns_failed"))?
        .collect::<Vec<_>>();
        require(
            !addresses.is_empty() && addresses.len() <= 32,
            "dns_invalid",
        )?;
        for a in &addresses {
            require(
                if let Some((origin, ip)) = &self.local {
                    if *origin == url.origin().ascii_serialization() {
                        a.ip() == *ip
                    } else {
                        public_address(a.ip())
                    }
                } else {
                    public_address(a.ip())
                },
                "network_address_denied",
            )?;
        }
        Ok(addresses)
    }
}
/// Conservative public-only policy; mapped IPv4 and non-global IPv6 are denied.
pub fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 168 || b == 0 || b == 2))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && s[0] != 0x2002
        }
    }
}
#[derive(Clone)]
pub enum Body {
    Empty,
    Json(Vec<u8>),
    Media {
        manifest: String,
        bytes: Vec<u8>,
        mime: String,
    },
}
pub struct Response {
    pub headers: HeaderMap,
    pub bytes: Vec<u8>,
}
#[derive(Clone)]
pub struct Http {
    pub boundary: Boundary,
    pub attempts: usize,
    pub timeout: Duration,
}
impl Http {
    pub async fn send(
        &self,
        method: Method,
        url: Url,
        authorization: Option<&str>,
        body: Body,
        max_bytes: usize,
    ) -> Result<Response> {
        let mut last = Error::new("transport_failed");
        for attempt in 0..self.attempts {
            let mut current = url.clone();
            let mut retry = None;
            for redirect in 0..=4 {
                let addresses = match self.boundary.addresses(&current).await {
                    Ok(addresses) => addresses,
                    Err(error) if matches!(error.code, "dns_failed" | "dns_timeout") => {
                        last = error;
                        retry = Some(Duration::from_millis(200 * (1 << attempt)));
                        break;
                    }
                    Err(error) => return Err(error),
                };
                // Each new hop/attempt resolves and pins ALL checked addresses. No proxy bypass.
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(self.timeout)
                    .connect_timeout(Duration::from_secs(5))
                    .resolve_to_addrs(
                        current
                            .host_str()
                            .ok_or_else(|| Error::new("network_host_invalid"))?,
                        &addresses,
                    )
                    .build()
                    .map_err(|_| Error::new("http_client_failed"))?;
                let mut req = client
                    .request(method.clone(), current.clone())
                    .header("Accept", "application/json");
                if let Some(auth) = authorization {
                    require(
                        current.origin() == url.origin(),
                        "authenticated_redirect_denied",
                    )?;
                    req = req.header(AUTHORIZATION, auth);
                }
                req = match &body {
                    Body::Empty => req,
                    Body::Json(bytes) => req
                        .header(CONTENT_TYPE, "application/json")
                        .body(bytes.clone()),
                    Body::Media {
                        manifest,
                        bytes,
                        mime,
                    } => req.multipart(
                        reqwest::multipart::Form::new()
                            .text("manifest", manifest.clone())
                            .part(
                                "file",
                                reqwest::multipart::Part::bytes(bytes.clone())
                                    .file_name(match mime.as_str() {
                                        "image/png" => "approved-image.png",
                                        "image/jpeg" => "approved-image.jpg",
                                        "image/webp" => "approved-image.webp",
                                        "image/avif" => "approved-image.avif",
                                        _ => return Err(Error::new("media_mime_invalid")),
                                    })
                                    .mime_str(mime)
                                    .map_err(|_| Error::new("media_mime_invalid"))?,
                            ),
                    ),
                };
                let response = match req.send().await {
                    Ok(r) => r,
                    Err(_) => {
                        retry = Some(Duration::from_millis(200 * (1 << attempt)));
                        break;
                    }
                };
                let status = response.status().as_u16();
                if (300..400).contains(&status) {
                    require(method == Method::GET && redirect < 4, "redirect_denied")?;
                    let loc = response
                        .headers()
                        .get(LOCATION)
                        .and_then(|x| x.to_str().ok())
                        .ok_or_else(|| Error::new("redirect_invalid"))?;
                    current = current
                        .join(loc)
                        .map_err(|_| Error::new("redirect_invalid"))?;
                    continue;
                }
                if status == 429 || (500..600).contains(&status) {
                    last = Error::http(status);
                    let delay = if let Some(raw) = response.headers().get(RETRY_AFTER) {
                        retry_after(
                            raw.to_str()
                                .map_err(|_| Error::new("retry_after_invalid"))?,
                        )?
                    } else {
                        Duration::from_millis(200 * (1 << attempt))
                    };
                    last.retry_delay_ms = Some(delay.as_millis() as u64);
                    retry = Some(delay);
                    break;
                }
                if !(200..300).contains(&status) {
                    return Err(Error::http(status));
                }
                require(
                    response
                        .content_length()
                        .is_none_or(|n| n <= max_bytes as u64),
                    "response_too_large",
                )?;
                let headers = response.headers().clone();
                let mut stream = response.bytes_stream();
                let mut bytes = Vec::new();
                while let Some(part) = stream.next().await {
                    let part = part.map_err(|_| Error::new("response_interrupted"))?;
                    require(bytes.len() + part.len() <= max_bytes, "response_too_large")?;
                    bytes.extend_from_slice(&part);
                }
                return Ok(Response { headers, bytes });
            }
            if let Some(delay) = retry {
                if attempt + 1 < self.attempts {
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(last);
            }
        }
        Err(last)
    }
    pub async fn json(
        &self,
        method: Method,
        url: Url,
        authorization: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let body = body
            .map(|v| {
                serde_json::to_vec(v)
                    .map(Body::Json)
                    .map_err(|_| Error::new("request_json_invalid"))
            })
            .transpose()?
            .unwrap_or(Body::Empty);
        let r = self
            .send(method, url, authorization, body, 2 * 1024 * 1024)
            .await?;
        json_response(r)
    }
}
pub fn json_response(r: Response) -> Result<serde_json::Value> {
    require(
        r.headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(';').next() == Some("application/json")),
        "source_not_json_or_bot_protection",
    )?;
    serde_json::from_slice(&r.bytes).map_err(|_| Error::new("response_json_invalid"))
}
pub fn retry_after(s: &str) -> Result<Duration> {
    let seconds = if let Ok(n) = s.parse::<u64>() {
        n
    } else {
        let date = chrono::DateTime::parse_from_rfc2822(s)
            .map_err(|_| Error::new("retry_after_invalid"))?;
        (date.timestamp() - chrono::Utc::now().timestamp()).max(0) as u64
    };
    // Do not violate a long Retry-After by capping it and retrying early.
    require(seconds <= 60, "retry_after_exceeds_budget")?;
    Ok(Duration::from_secs(seconds))
}
