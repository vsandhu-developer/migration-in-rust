use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, Method};

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status_code: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status_code)
    }
}

#[derive(Debug, Clone)]
pub struct BinaryResponse {
    pub status_code: u16,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct MultipartFile {
    pub field_name: String,
    pub file_name: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Async reqwest-based HTTP client with persistent connection pool, HTTP/2, and
/// compression support. Equivalent to the libcurl wrapper in the C++ codebase.
#[derive(Clone)]
pub struct HttpClient {
    client: Client,
}

impl HttpClient {
    pub fn new() -> Result<Self> {
        Self::with_timeout(120)
    }

    pub fn with_timeout(seconds: u64) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(seconds))
            .connect_timeout(Duration::from_secs(10))
            .tcp_keepalive(Some(Duration::from_secs(120)))
            .pool_max_idle_per_host(16)
            .pool_idle_timeout(Some(Duration::from_secs(90)))
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .http2_prior_knowledge_overrideable() // negotiate http2 over TLS
            .build()
            .context("build reqwest client")?;
        Ok(Self { client })
    }

    fn build_headers(headers: &[String]) -> Result<HeaderMap> {
        let mut map = HeaderMap::new();
        for raw in headers {
            if let Some((k, v)) = raw.split_once(':') {
                let name = HeaderName::from_bytes(k.trim().as_bytes())
                    .with_context(|| format!("invalid header name: {k}"))?;
                let value = HeaderValue::from_str(v.trim())
                    .with_context(|| format!("invalid header value: {v}"))?;
                map.insert(name, value);
            }
        }
        Ok(map)
    }

    async fn execute(
        &self,
        method: Method,
        url: &str,
        body: Option<String>,
        headers: &[String],
    ) -> Result<HttpResponse> {
        let mut req = self.client.request(method, url).headers(Self::build_headers(headers)?);
        if let Some(b) = body {
            req = req.body(b);
        }
        let resp = req.send().await.with_context(|| format!("HTTP request to {url}"))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok(HttpResponse { status_code: status, body })
    }

    pub async fn get(&self, url: &str, headers: &[String]) -> Result<HttpResponse> {
        self.execute(Method::GET, url, None, headers).await
    }

    pub async fn post(&self, url: &str, body: &str, headers: &[String]) -> Result<HttpResponse> {
        self.execute(Method::POST, url, Some(body.to_string()), headers).await
    }

    pub async fn delete(&self, url: &str, headers: &[String]) -> Result<HttpResponse> {
        self.execute(Method::DELETE, url, None, headers).await
    }

    pub async fn post_multipart(
        &self,
        url: &str,
        files: &[MultipartFile],
        headers: &[String],
    ) -> Result<HttpResponse> {
        let mut form = Form::new();
        for f in files {
            let mut part = Part::bytes(f.data.clone()).file_name(f.file_name.clone());
            if !f.content_type.is_empty() {
                part = part.mime_str(&f.content_type)
                    .with_context(|| format!("invalid mime: {}", f.content_type))?;
            }
            form = form.part(f.field_name.clone(), part);
        }
        let resp = self
            .client
            .post(url)
            .headers(Self::build_headers(headers)?)
            .multipart(form)
            .send()
            .await
            .with_context(|| format!("multipart POST {url}"))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok(HttpResponse { status_code: status, body })
    }

    pub async fn download_to_memory(&self, url: &str) -> Result<BinaryResponse> {
        let resp = self.client.get(url).send().await
            .with_context(|| format!("download {url}"))?;
        let status = resp.status().as_u16();
        let bytes: Bytes = resp.bytes().await.unwrap_or_default();
        Ok(BinaryResponse { status_code: status, data: bytes.to_vec() })
    }

    /// Concurrent batch download with bounded concurrency.
    pub async fn download_many_to_memory(
        &self,
        urls: &[String],
        max_concurrent: usize,
    ) -> Result<HashMap<String, BinaryResponse>> {
        async fn fetch_one(client: Client, url: String) -> (String, Result<BinaryResponse>) {
            let res = match client.get(&url).send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let bytes = resp.bytes().await.unwrap_or_default();
                    Ok(BinaryResponse { status_code: status, data: bytes.to_vec() })
                }
                Err(e) => Err(anyhow!("download {url}: {e}")),
            };
            (url, res)
        }

        let mut results: HashMap<String, BinaryResponse> = HashMap::new();
        let mut iter = urls.iter().cloned();
        let mut in_flight: FuturesUnordered<_> = FuturesUnordered::new();

        for _ in 0..max_concurrent.max(1) {
            if let Some(url) = iter.next() {
                in_flight.push(fetch_one(self.client.clone(), url));
            }
        }

        while let Some((url, res)) = in_flight.next().await {
            match res {
                Ok(b) => { results.insert(url, b); }
                Err(e) => {
                    tracing::warn!(error = %e, "download failed");
                }
            }
            if let Some(next) = iter.next() {
                in_flight.push(fetch_one(self.client.clone(), next));
            }
        }

        Ok(results)
    }
}

// Helper trait to keep builder ergonomic across reqwest versions.
trait Http2Negotiate {
    fn http2_prior_knowledge_overrideable(self) -> reqwest::ClientBuilder;
}

impl Http2Negotiate for reqwest::ClientBuilder {
    fn http2_prior_knowledge_overrideable(self) -> reqwest::ClientBuilder {
        // Use HTTP/2 via ALPN, not prior knowledge (TLS handles negotiation).
        self
    }
}
