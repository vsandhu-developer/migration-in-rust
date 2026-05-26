//! Download user avatar images (IMG_0..N) to disk, in parallel.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::infrastructure::http::HttpClient;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageDownloadResult {
    pub image_id: String,
    pub url: String,
    pub local_file_path: String,
    pub success: bool,
    pub error_message: String,
    pub file_size_bytes: usize,
}

#[derive(Debug, Clone, Default)]
pub struct DownloadBatchResult {
    pub results: Vec<ImageDownloadResult>,
    pub total_requested: usize,
    pub total_succeeded: usize,
    pub total_failed: usize,
    pub duration_ms: f64,
}

pub struct UserImageDownloader {
    http: HttpClient,
}

impl UserImageDownloader {
    pub fn new(http: HttpClient) -> Self { Self { http } }

    pub async fn download_all(
        &self,
        json_path: impl AsRef<Path>,
        output_dir: impl AsRef<Path>,
    ) -> Result<DownloadBatchResult> {
        let images = Self::load_image_variables(json_path.as_ref())?;
        let output_dir = output_dir.as_ref().to_path_buf();
        fs::create_dir_all(&output_dir).ok();

        let t0 = Instant::now();
        let urls: Vec<String> = images.iter().map(|(_, u)| u.clone()).collect();
        let bin_map = self.http.download_many_to_memory(&urls, 6).await.unwrap_or_default();

        let mut results = Vec::new();
        for (image_id, url) in &images {
            if let Some(bin) = bin_map.get(url) {
                if (200..300).contains(&bin.status_code) && !bin.data.is_empty() {
                    let ext = guess_extension(url, &bin.data);
                    let file_name = format!("{image_id}.{ext}");
                    let local_path = output_dir.join(&file_name);
                    match fs::write(&local_path, &bin.data) {
                        Ok(_) => {
                            results.push(ImageDownloadResult {
                                image_id: image_id.clone(),
                                url: url.clone(),
                                local_file_path: local_path.to_string_lossy().to_string(),
                                success: true,
                                error_message: String::new(),
                                file_size_bytes: bin.data.len(),
                            });
                            continue;
                        }
                        Err(e) => {
                            warn!(image_id, error = %e, "write image failed");
                            results.push(ImageDownloadResult {
                                image_id: image_id.clone(),
                                url: url.clone(),
                                local_file_path: String::new(),
                                success: false,
                                error_message: format!("write: {e}"),
                                file_size_bytes: 0,
                            });
                            continue;
                        }
                    }
                }
            }
            results.push(ImageDownloadResult {
                image_id: image_id.clone(),
                url: url.clone(),
                local_file_path: String::new(),
                success: false,
                error_message: "download empty or HTTP error".into(),
                file_size_bytes: 0,
            });
        }

        let dur = t0.elapsed().as_secs_f64() * 1000.0;
        let total_succeeded = results.iter().filter(|r| r.success).count();
        let total_failed = results.len() - total_succeeded;
        info!(
            requested = results.len(),
            succeeded = total_succeeded,
            failed = total_failed,
            duration_ms = dur,
            "user image download complete"
        );
        Ok(DownloadBatchResult {
            total_requested: results.len(),
            total_succeeded,
            total_failed,
            duration_ms: dur,
            results,
        })
    }

    pub fn validate_downloads(batch: &DownloadBatchResult) -> bool {
        batch.results.iter().filter(|r| r.success).all(|r| {
            let p = PathBuf::from(&r.local_file_path);
            p.exists() && fs::metadata(&p).map(|m| m.len() > 0).unwrap_or(false)
        })
    }

    /// Tolerant parser: accepts array of {id,url}, flat object {id: url}, or nested object.
    fn load_image_variables(path: &Path) -> Result<Vec<(String, String)>> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("read image variables: {}", path.display()))?;
        let v: Value = serde_json::from_str(&raw)?;
        let mut out: Vec<(String, String)> = Vec::new();
        match v {
            Value::Array(arr) => {
                for it in arr {
                    if let (Some(id), Some(url)) = (
                        it.get("id").and_then(|x| x.as_str()),
                        it.get("url").and_then(|x| x.as_str()),
                    ) {
                        out.push((id.to_string(), url.to_string()));
                    }
                }
            }
            Value::Object(map) => {
                // try nested keys first
                let candidates = ["images", "data", "items"];
                let chosen = candidates.iter()
                    .find_map(|k| map.get(*k))
                    .cloned()
                    .unwrap_or(Value::Object(map.clone()));
                if let Value::Object(inner) = &chosen {
                    for (k, v) in inner {
                        if let Some(url) = v.as_str() {
                            out.push((k.clone(), url.to_string()));
                        } else if let Some(obj) = v.as_object() {
                            if let Some(url) = obj.get("url").and_then(|x| x.as_str()) {
                                out.push((k.clone(), url.to_string()));
                            }
                        }
                    }
                }
            }
            _ => return Err(anyhow!("unknown image-variables format")),
        }
        Ok(out)
    }
}

fn guess_extension(url: &str, data: &[u8]) -> String {
    if let Some(ext) = url
        .split('?').next().unwrap_or(url)
        .rsplit('.').next()
        .filter(|e| e.len() <= 5)
    {
        return ext.to_string();
    }
    // sniff from magic bytes
    if data.starts_with(b"\x89PNG") { return "png".into(); }
    if data.starts_with(b"\xff\xd8\xff") { return "jpg".into(); }
    if data.starts_with(b"GIF8") { return "gif".into(); }
    if data.starts_with(b"RIFF") { return "webp".into(); }
    "bin".into()
}
