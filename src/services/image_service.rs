//! End-to-end image relocation: download from WP origins, upload to Strapi,
//! return blockIndex -> Strapi URL map. Strict and lenient modes.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use tracing::{info, warn};

const SNAPSHOT_DIR: &str = "data/incomplete/images-snapshot";

use crate::clients::{MediaUploadItem, StrapiClient, UploadedMedia};
use crate::infrastructure::http::HttpClient;
use crate::models::{ContentBlock, ImageProcessingResult};

#[derive(Debug, Clone)]
pub struct DownloadedImageItem {
    pub block_index: i32,
    pub source_url: String,
    pub file_name: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct DownloadSummary {
    pub requested_count: i32,
    pub downloaded_count: i32,
    pub failed_count: i32,
    pub items: Vec<DownloadedImageItem>,
    pub total_bytes: usize,
    pub duration_ms: f64,
}

#[derive(Debug, Clone, Default)]
pub struct UploadSummary {
    pub requested_count: i32,
    pub uploaded_count: i32,
    pub failed_count: i32,
    pub url_mapping: HashMap<i32, String>,
    pub duration_ms: f64,
}

#[derive(Debug, Clone, Default)]
pub struct CoverImageSummary {
    pub strapi_media_id: i64,
    pub strapi_url: String,
    pub download_ms: f64,
    pub upload_ms: f64,
    pub download_bytes: usize,
}

#[derive(Clone)]
pub struct ImageService {
    strapi: StrapiClient,
    http: HttpClient,
}

impl ImageService {
    pub fn new(strapi: StrapiClient, http: HttpClient) -> Self {
        Self { strapi, http }
    }

    /// Lenient: returns whatever mapping was obtained, even partial.
    pub async fn process_images(&self, blocks: &[&ContentBlock]) -> HashMap<i32, String> {
        let requests = Self::collect_requests(blocks);
        if requests.is_empty() { return HashMap::new(); }
        let download = self.download_images(&requests).await;
        if download.items.is_empty() { return HashMap::new(); }
        let upload = self.upload_images(&download.items).await;
        info!(
            requested = download.requested_count,
            downloaded = download.downloaded_count,
            uploaded = upload.uploaded_count,
            "image processing (lenient)"
        );
        upload.url_mapping
    }

    /// Strict: all-or-nothing. Returns failure details on any miss.
    pub async fn process_images_strict(&self, blocks: &[&ContentBlock]) -> ImageProcessingResult {
        let mut result = ImageProcessingResult::default();
        let requests = Self::collect_requests(blocks);
        result.total_image_blocks_required = requests.len() as i32;

        if requests.is_empty() {
            result.success = true;
            return result;
        }

        let download = self.download_images(&requests).await;
        // determine failed downloads
        let downloaded_urls: HashSet<&str> = download.items.iter().map(|i| i.source_url.as_str()).collect();
        for (_, src) in &requests {
            if !downloaded_urls.contains(src.as_str()) {
                result.failed_image_urls.push(src.clone());
            }
        }
        if !result.failed_image_urls.is_empty() {
            result.success = false;
            result.failure_reason = format!(
                "Failed to download {} of {} images",
                result.failed_image_urls.len(),
                requests.len()
            );
            return result;
        }

        let upload = self.upload_images(&download.items).await;
        let uploaded_indices: HashSet<i32> = upload.url_mapping.keys().copied().collect();
        let mut failed_uploads = Vec::new();
        for item in &download.items {
            if !uploaded_indices.contains(&item.block_index) {
                failed_uploads.push(item.source_url.clone());
            }
        }
        if !failed_uploads.is_empty() {
            result.success = false;
            result.failed_image_urls = failed_uploads;
            result.failure_reason = format!(
                "Failed to upload {} images to Strapi",
                result.failed_image_urls.len()
            );
            return result;
        }

        result.success = true;
        result.successfully_processed_count = upload.url_mapping.len() as i32;
        result.url_mapping = upload.url_mapping;
        result
    }

    pub async fn process_cover_image(&self, source_url: &str) -> Result<CoverImageSummary> {
        let mut summary = CoverImageSummary::default();
        let url = normalize_image_url(source_url);
        let t0 = Instant::now();
        let bin = self.http.download_to_memory(&url).await?;
        summary.download_ms = t0.elapsed().as_secs_f64() * 1000.0;
        summary.download_bytes = bin.data.len();
        if bin.data.is_empty() {
            anyhow::bail!("cover image: empty body from {url}");
        }

        let file_name = filename_from_url(&url).unwrap_or_else(|| "cover.bin".to_string());
        if self.strapi.is_dry_run() {
            snapshot_to_disk(&file_name, &bin.data, &url);
        }
        let item = MediaUploadItem {
            block_index: -1,
            mime_type: infer_mime(&file_name),
            file_name,
            data: bin.data,
        };
        let t1 = Instant::now();
        let uploaded: UploadedMedia = self.strapi.upload_media(&item).await?;
        summary.upload_ms = t1.elapsed().as_secs_f64() * 1000.0;
        summary.strapi_media_id = uploaded.id;
        summary.strapi_url = uploaded.url;
        Ok(summary)
    }

    // -------- internals --------
    fn collect_requests(blocks: &[&ContentBlock]) -> Vec<(i32, String)> {
        let mut out = Vec::new();
        for blk in blocks {
            if !blk.is_image() { continue; }
            let url = choose_best_image_url(blk);
            if !url.is_empty() {
                out.push((blk.index, url));
            }
        }
        out
    }

    async fn download_images(&self, requests: &[(i32, String)]) -> DownloadSummary {
        let mut summary = DownloadSummary::default();
        summary.requested_count = requests.len() as i32;
        let t0 = Instant::now();
        let urls: Vec<String> = requests.iter().map(|(_, u)| u.clone()).collect();

        // Try concurrent batch download.
        let mut batch = self.http.download_many_to_memory(&urls, 6).await.unwrap_or_default();
        // Fallback for any missed.
        for (idx, src) in requests {
            let bin = if let Some(b) = batch.remove(src) {
                if b.status_code >= 200 && b.status_code < 300 && !b.data.is_empty() {
                    Some(b)
                } else {
                    None
                }
            } else {
                None
            };
            let bin = match bin {
                Some(b) => Some(b),
                None => match self.http.download_to_memory(src).await {
                    Ok(b) if (200..300).contains(&b.status_code) && !b.data.is_empty() => Some(b),
                    Ok(_) => None,
                    Err(e) => { warn!(url = %src, error = %e, "download failed"); None }
                },
            };
            if let Some(b) = bin {
                summary.total_bytes += b.data.len();
                let file_name = filename_from_url(src).unwrap_or_else(|| format!("image-{}.bin", idx));
                if self.strapi.is_dry_run() {
                    snapshot_to_disk(&file_name, &b.data, src);
                }
                summary.items.push(DownloadedImageItem {
                    block_index: *idx,
                    source_url: src.clone(),
                    file_name,
                    data: b.data,
                });
                summary.downloaded_count += 1;
            } else {
                summary.failed_count += 1;
            }
        }
        summary.duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
        summary
    }

    async fn upload_images(&self, items: &[DownloadedImageItem]) -> UploadSummary {
        let mut summary = UploadSummary::default();
        summary.requested_count = items.len() as i32;
        let t0 = Instant::now();
        let upload_items: Vec<MediaUploadItem> = items.iter().map(|i| MediaUploadItem {
            block_index: i.block_index,
            file_name: i.file_name.clone(),
            mime_type: infer_mime(&i.file_name),
            data: i.data.clone(),
        }).collect();

        // Try batch upload, fall back per-item.
        match self.strapi.upload_media_batch(&upload_items, 100 * 1024 * 1024).await {
            Ok(mapping) => {
                summary.url_mapping = mapping;
            }
            Err(e) => {
                warn!(error = %e, "batch upload failed, falling back per-item");
                for item in &upload_items {
                    match self.strapi.upload_media_batch(std::slice::from_ref(item), 100 * 1024 * 1024).await {
                        Ok(m) => summary.url_mapping.extend(m),
                        Err(e2) => warn!(file = %item.file_name, error = %e2, "single upload failed"),
                    }
                }
            }
        }

        summary.uploaded_count = summary.url_mapping.len() as i32;
        summary.failed_count = summary.requested_count - summary.uploaded_count;
        summary.duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
        summary
    }
}

fn choose_best_image_url(block: &ContentBlock) -> String {
    if !block.src.is_empty() {
        return normalize_image_url(&block.src);
    }
    if let Some(first) = block.srcset.first() {
        let url_part = first.split_whitespace().next().unwrap_or("").to_string();
        return normalize_image_url(&url_part);
    }
    String::new()
}

fn normalize_image_url(url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if let Some(stripped) = url.strip_prefix("//") {
        format!("https://{}", stripped)
    } else {
        url.to_string()
    }
}

fn filename_from_url(url: &str) -> Option<String> {
    let without_query = url.split('?').next().unwrap_or(url);
    let without_frag = without_query.split('#').next().unwrap_or(without_query);
    let name = without_frag.rsplit('/').next().unwrap_or("");
    if name.is_empty() { None } else { Some(cap_filename(name)) }
}

// Cap source filename so Strapi's sanitization + thumbnail_/small_/medium_/large_
// prefixes + hash + extension still fit under the 255-byte OS filename limit.
// Strapi crashes hard (uncaught ENAMETOOLONG → process exit) when this is exceeded,
// taking down all in-flight image uploads. Seen with Google-CDN URLs like
// /AD_4nXdQKm9...long_blob — sanitization roughly doubles those, blowing past 255.
fn cap_filename(name: &str) -> String {
    const MAX_LEN: usize = 80;
    if name.len() <= MAX_LEN {
        return name.to_string();
    }
    let (base, ext) = match name.rsplit_once('.') {
        Some((b, e)) if e.len() <= 8 && !e.is_empty() => (b, format!(".{e}")),
        _ => (name, String::new()),
    };
    let take = MAX_LEN.saturating_sub(ext.len());
    let mut out = String::with_capacity(MAX_LEN);
    for (i, ch) in base.char_indices() {
        if i + ch.len_utf8() > take { break; }
        out.push(ch);
    }
    out.push_str(&ext);
    out
}

fn snapshot_to_disk(file_name: &str, data: &[u8], source_url: &str) {
    let dir = Path::new(SNAPSHOT_DIR);
    if let Err(e) = fs::create_dir_all(dir) {
        warn!(error = %e, "snapshot: failed to create directory");
        return;
    }
    let safe = sanitize_filename(file_name);
    let path = dir.join(&safe);
    match fs::write(&path, data) {
        Ok(()) => {
            let manifest = dir.join("manifest.csv");
            let line = format!("{}\t{}\t{}\n", safe, data.len(), source_url);
            let _ = append_line(&manifest, &line);
        }
        Err(e) => warn!(file = %safe, error = %e, "snapshot: write failed"),
    }
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect()
}

fn infer_mime(file_name: &str) -> String {
    let lower = file_name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "jpg" | "jpeg" => "image/jpeg".into(),
        "png" => "image/png".into(),
        "webp" => "image/webp".into(),
        "gif" => "image/gif".into(),
        _ => mime_guess::from_path(file_name).first_or_octet_stream().to_string(),
    }
}
