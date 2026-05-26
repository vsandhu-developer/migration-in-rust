//! Upload user avatars to Strapi and persist IMG_N -> mediaId mapping.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::clients::StrapiClient;
use crate::services::user_image_downloader::ImageDownloadResult;

#[derive(Debug, Clone, Default)]
pub struct UploadBatchResult {
    pub image_id_to_strapi_media_id: HashMap<String, i64>,
    pub total_requested: usize,
    pub total_succeeded: usize,
    pub total_failed: usize,
    pub duration_ms: f64,
}

pub struct UserImageUploader {
    strapi: StrapiClient,
}

impl UserImageUploader {
    pub fn new(strapi: StrapiClient) -> Self { Self { strapi } }

    pub async fn upload_all(&self, downloaded: &[ImageDownloadResult]) -> Result<UploadBatchResult> {
        let mut result = UploadBatchResult::default();
        result.total_requested = downloaded.iter().filter(|r| r.success).count();
        let t0 = Instant::now();

        let mut to_upload: Vec<(String, String, String, Vec<u8>)> = Vec::new();
        for d in downloaded {
            if !d.success { continue; }
            let path = PathBuf::from(&d.local_file_path);
            let bytes = match fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    warn!(image_id = %d.image_id, error = %e, "read failed");
                    continue;
                }
            };
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("avatar.bin").to_string();
            let mime = mime_guess::from_path(&path).first_or_octet_stream().to_string();
            to_upload.push((d.image_id.clone(), file_name, mime, bytes));
        }

        match self.strapi.upload_user_images_batch(&to_upload).await {
            Ok(map) => {
                result.total_succeeded = map.len();
                result.image_id_to_strapi_media_id = map;
            }
            Err(e) => {
                warn!(error = %e, "user image batch upload failed");
            }
        }
        result.total_failed = result.total_requested - result.total_succeeded;
        result.duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
        info!(
            requested = result.total_requested,
            succeeded = result.total_succeeded,
            failed = result.total_failed,
            duration_ms = result.duration_ms,
            "user image upload complete"
        );
        Ok(result)
    }

    pub fn save_image_mapping(path: impl AsRef<Path>, mapping: &HashMap<String, i64>) -> Result<()> {
        let p = path.as_ref();
        if let Some(parent) = p.parent() { fs::create_dir_all(parent).ok(); }
        let v: Value = json!(mapping);
        fs::write(p, serde_json::to_string_pretty(&v)?)
            .with_context(|| format!("write image mapping: {}", p.display()))?;
        Ok(())
    }

    pub fn load_image_mapping(path: impl AsRef<Path>) -> Result<HashMap<String, i64>> {
        let p = path.as_ref();
        if !p.exists() { return Ok(HashMap::new()); }
        let raw = fs::read_to_string(p)?;
        let map: HashMap<String, i64> = serde_json::from_str(&raw)?;
        Ok(map)
    }
}
