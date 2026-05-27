//! Strapi v5 REST client + custom /migration plugin endpoints.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::infrastructure::config::Config;
use crate::infrastructure::http::{HttpClient, HttpResponse, MultipartFile};

#[derive(Debug, Clone)]
pub struct MediaUploadItem {
    pub block_index: i32,
    pub file_name: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct UploadedMedia {
    pub id: i64,
    pub url: String,
}

#[derive(Debug, Clone, Default)]
pub struct StrapiEntityResult {
    pub id: i64,
    pub document_id: String,
    pub name: String,
    pub slug: String,
}

#[derive(Clone)]
pub struct StrapiClient {
    cfg: Config,
    http: HttpClient,
    dry_run: bool,
}

impl StrapiClient {
    pub fn new(cfg: Config, http: HttpClient) -> Self {
        Self { cfg, http, dry_run: false }
    }

    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    pub fn is_dry_run(&self) -> bool { self.dry_run }

    fn fake_ok(body: &str) -> HttpResponse {
        HttpResponse { status_code: 200, body: body.to_string() }
    }

    fn auth_headers(&self, content_json: bool) -> Vec<String> {
        let mut h = Vec::new();
        if !self.cfg.token.is_empty() {
            h.push(format!("Authorization: Bearer {}", self.cfg.token));
        }
        if content_json {
            h.push("Content-Type: application/json".to_string());
        }
        h
    }

    /// Headers for the custom `/migration/*` routes — Strapi checks
    /// `MIGRATION_API_TOKEN` via `Authorization: Bearer`, not the regular API token.
    fn migration_auth_headers(&self, content_json: bool) -> Vec<String> {
        let mut h = Vec::new();
        let token = if !self.cfg.migration_token.is_empty() {
            &self.cfg.migration_token
        } else {
            &self.cfg.token
        };
        if !token.is_empty() {
            h.push(format!("Authorization: Bearer {}", token));
        }
        if content_json {
            h.push("Content-Type: application/json".to_string());
        }
        h
    }

    fn build_api_url(&self, endpoint: &str) -> String {
        let mut base = self.cfg.strapi_base_url.trim_end_matches('/').to_string();
        if !endpoint.starts_with('/') {
            base.push('/');
        }
        base.push_str(endpoint);
        base
    }

    fn make_absolute_url(base: &str, candidate: &str) -> String {
        if candidate.starts_with("http://") || candidate.starts_with("https://") {
            return candidate.to_string();
        }
        let base = base.trim_end_matches('/');
        if candidate.starts_with('/') {
            format!("{base}{candidate}")
        } else {
            format!("{base}/{candidate}")
        }
    }

    // ---------- standard collection endpoints ----------
    pub async fn get_articles(&self) -> Result<HttpResponse> {
        let url = self.build_api_url("/api/ds-articles");
        self.http.get(&url, &self.auth_headers(false)).await
    }

    /// Single media upload. Returns {id, url}.
    pub async fn upload_media(&self, item: &MediaUploadItem) -> Result<UploadedMedia> {
        if self.dry_run {
            return Ok(UploadedMedia { id: -1, url: format!("dry-run://{}", item.file_name) });
        }
        let url = self.build_api_url("/api/upload");
        let file = MultipartFile {
            field_name: "files".to_string(),
            file_name: item.file_name.clone(),
            content_type: item.mime_type.clone(),
            data: item.data.clone(),
        };
        let resp = self.http.post_multipart(&url, &[file], &self.auth_headers(false)).await?;
        if !resp.is_success() {
            bail!("upload_media: HTTP {}: {}", resp.status_code, resp.body);
        }
        let v: Value = serde_json::from_str(&resp.body)
            .with_context(|| format!("parse upload_media response: {}", resp.body))?;
        let arr = v.as_array().ok_or_else(|| anyhow!("upload_media: not an array"))?;
        let first = arr.first().ok_or_else(|| anyhow!("upload_media: empty response"))?;
        let id = first.get("id").and_then(|x| x.as_i64()).ok_or_else(|| anyhow!("missing id"))?;
        let raw_url = first.get("url").and_then(|x| x.as_str()).ok_or_else(|| anyhow!("missing url"))?;
        Ok(UploadedMedia {
            id,
            url: Self::make_absolute_url(&self.cfg.strapi_base_url, raw_url),
        })
    }

    /// Batched multipart upload for content-body images. Returns blockIndex -> absolute Strapi URL.
    pub async fn upload_media_batch(
        &self,
        items: &[MediaUploadItem],
        max_payload_bytes: usize,
    ) -> Result<HashMap<i32, String>> {
        const PER_FILE_OVERHEAD: usize = 2048;
        let mut url_map: HashMap<i32, String> = HashMap::new();
        if items.is_empty() {
            return Ok(url_map);
        }
        if self.dry_run {
            for item in items {
                url_map.insert(item.block_index, format!("dry-run://{}", item.file_name));
            }
            return Ok(url_map);
        }

        let mut batches: Vec<Vec<&MediaUploadItem>> = vec![Vec::new()];
        let mut current_bytes = 0usize;
        for item in items {
            let item_bytes = item.data.len() + PER_FILE_OVERHEAD;
            if item_bytes > max_payload_bytes {
                bail!("file too large for single batch: {} ({} bytes)", item.file_name, item.data.len());
            }
            if current_bytes + item_bytes > max_payload_bytes && !batches.last().unwrap().is_empty() {
                batches.push(Vec::new());
                current_bytes = 0;
            }
            batches.last_mut().unwrap().push(item);
            current_bytes += item_bytes;
        }

        let url = self.build_api_url("/api/upload");
        for batch in batches {
            if batch.is_empty() { continue; }
            let files: Vec<MultipartFile> = batch.iter().map(|i| MultipartFile {
                field_name: "files".to_string(),
                file_name: i.file_name.clone(),
                content_type: i.mime_type.clone(),
                data: i.data.clone(),
            }).collect();
            let resp = self.http.post_multipart(&url, &files, &self.auth_headers(false)).await?;
            if !resp.is_success() {
                bail!("upload_media_batch: HTTP {}: {}", resp.status_code, resp.body);
            }
            let v: Value = serde_json::from_str(&resp.body)
                .with_context(|| format!("parse upload_media_batch response: {}", resp.body))?;
            let arr = v.as_array().ok_or_else(|| anyhow!("upload_media_batch: not an array"))?;
            if arr.len() != batch.len() {
                bail!("upload_media_batch: response size {} != batch {}", arr.len(), batch.len());
            }
            for (i, item) in batch.iter().enumerate() {
                let entry = &arr[i];
                let raw_url = entry.get("url").and_then(|x| x.as_str()).unwrap_or("");
                let abs = Self::make_absolute_url(&self.cfg.strapi_base_url, raw_url);
                url_map.insert(item.block_index, abs);
            }
        }
        Ok(url_map)
    }

    /// User-avatar batch upload. Returns imageId -> media id.
    pub async fn upload_user_images_batch(
        &self,
        images: &[(String, String, String, Vec<u8>)], // (imageId, fileName, mimeType, data)
    ) -> Result<HashMap<String, i64>> {
        let mut out = HashMap::new();
        if images.is_empty() { return Ok(out); }
        if self.dry_run {
            for (id, _, _, _) in images {
                out.insert(id.clone(), -1);
            }
            return Ok(out);
        }

        let url = self.build_api_url("/api/upload");
        let files: Vec<MultipartFile> = images.iter().map(|(_, name, mime, data)| MultipartFile {
            field_name: "files".to_string(),
            file_name: name.clone(),
            content_type: mime.clone(),
            data: data.clone(),
        }).collect();
        let resp = self.http.post_multipart(&url, &files, &self.auth_headers(false)).await?;
        if !resp.is_success() {
            bail!("upload_user_images_batch: HTTP {}: {}", resp.status_code, resp.body);
        }
        let v: Value = serde_json::from_str(&resp.body)?;
        let arr = v.as_array().ok_or_else(|| anyhow!("upload_user_images_batch: not an array"))?;
        if arr.len() != images.len() {
            bail!("upload_user_images_batch: response size mismatch");
        }
        for (i, (image_id, _, _, _)) in images.iter().enumerate() {
            if let Some(id) = arr[i].get("id").and_then(|x| x.as_i64()) {
                out.insert(image_id.clone(), id);
            }
        }
        Ok(out)
    }

    // ---------- entity creation ----------
    pub async fn create_ds_performer(&self, name: &str, slug: &str) -> Result<StrapiEntityResult> {
        self.create_entity("/api/ds-performers", name, slug).await
    }

    pub async fn create_ds_studio(&self, name: &str, slug: &str) -> Result<StrapiEntityResult> {
        self.create_entity("/api/ds-studios", name, slug).await
    }

    pub async fn create_ds_category(
        &self, name: &str, slug: &str, access_level: &str,
    ) -> Result<StrapiEntityResult> {
        if self.dry_run {
            return Ok(StrapiEntityResult {
                id: -1, document_id: format!("dry-{slug}"),
                name: name.into(), slug: slug.into(),
            });
        }
        let body = json!({ "data": { "Name": name, "Slug": slug, "Access_Level": access_level } });
        let url = self.build_api_url("/api/ds-categories");
        let resp = self.http.post(&url, &body.to_string(), &self.auth_headers(true)).await?;
        Self::parse_created_entity(&resp, name, slug)
    }

    pub async fn create_ds_sub_category(
        &self, name: &str, slug: &str, access_level: &str, parent_document_id: &str,
    ) -> Result<StrapiEntityResult> {
        if self.dry_run {
            let _ = parent_document_id;
            return Ok(StrapiEntityResult {
                id: -1, document_id: format!("dry-{slug}"),
                name: name.into(), slug: slug.into(),
            });
        }
        let body = json!({
            "data": {
                "Name": name,
                "Slug": slug,
                "Access_Level": access_level,
                "ds_category": { "connect": [parent_document_id] }
            }
        });
        let url = self.build_api_url("/api/ds-sub-categories");
        let resp = self.http.post(&url, &body.to_string(), &self.auth_headers(true)).await?;
        Self::parse_created_entity(&resp, name, slug)
    }

    async fn create_entity(&self, endpoint: &str, name: &str, slug: &str) -> Result<StrapiEntityResult> {
        if self.dry_run {
            return Ok(StrapiEntityResult {
                id: -1,
                document_id: format!("dry-{slug}"),
                name: name.into(),
                slug: slug.into(),
            });
        }
        let body = json!({ "data": { "Name": name, "Slug": slug } });
        let url = self.build_api_url(endpoint);
        let resp = self.http.post(&url, &body.to_string(), &self.auth_headers(true)).await?;
        Self::parse_created_entity(&resp, name, slug)
    }

    /// Parses Strapi v4 or v5 entity-create response. Treats "duplicate slug" 4xx
    /// as success (returns id=-1 with slug as documentId placeholder) to match C++ behavior.
    fn parse_created_entity(resp: &HttpResponse, name: &str, slug: &str) -> Result<StrapiEntityResult> {
        if !resp.is_success() {
            // Try to detect duplicate-slug error.
            if let Ok(v) = serde_json::from_str::<Value>(&resp.body) {
                if let Some(msg) = v.pointer("/error/message").and_then(|x| x.as_str()) {
                    if msg.to_lowercase().contains("unique") {
                        return Ok(StrapiEntityResult {
                            id: -1,
                            document_id: slug.to_string(),
                            name: name.to_string(),
                            slug: slug.to_string(),
                        });
                    }
                }
            }
            bail!("create_entity: HTTP {}: {}", resp.status_code, resp.body);
        }
        if resp.body.trim().is_empty() {
            return Ok(StrapiEntityResult {
                id: -1, document_id: slug.into(), name: name.into(), slug: slug.into(),
            });
        }
        let v: Value = serde_json::from_str(&resp.body)
            .with_context(|| format!("parse_created_entity body: {}", resp.body))?;

        // Try v5 flat shape: { data: {id, documentId, Name, Slug, ...} } or { id, documentId, ...}
        let data = v.get("data").unwrap_or(&v);
        let (out_name, out_slug) = (
            data.get("Name").or_else(|| data.get("attributes").and_then(|a| a.get("Name"))).and_then(|x| x.as_str()).unwrap_or(name).to_string(),
            data.get("Slug").or_else(|| data.get("attributes").and_then(|a| a.get("Slug"))).and_then(|x| x.as_str()).unwrap_or(slug).to_string(),
        );
        let id = data.get("id").and_then(|x| x.as_i64()).unwrap_or(-1);
        let doc_id = data.get("documentId").or_else(|| data.get("attributes").and_then(|a| a.get("documentId"))).and_then(|x| x.as_str()).unwrap_or(&out_slug).to_string();

        Ok(StrapiEntityResult { id, document_id: doc_id, name: out_name, slug: out_slug })
    }

    // ---------- raw JSON endpoints ----------
    pub async fn post_article(&self, body: &str) -> Result<HttpResponse> {
        if self.dry_run {
            tracing::info!(target: "dry_run", endpoint = "/api/ds-articles", size = body.len(), "skipped POST");
            return Ok(Self::fake_ok(r#"{"data":{"id":-1,"documentId":"dry-article"}}"#));
        }
        let url = self.build_api_url("/api/ds-articles");
        self.http.post(&url, body, &self.auth_headers(true)).await
    }

    pub async fn post_migration_comment(&self, body: &str) -> Result<HttpResponse> {
        if self.dry_run {
            tracing::info!(target: "dry_run", endpoint = "/migration/add-comment", size = body.len(), "skipped POST");
            return Ok(Self::fake_ok(r#"{"data":{"id":-1}}"#));
        }
        let url = self.build_api_url("/migration/add-comment");
        self.http.post(&url, body, &self.migration_auth_headers(true)).await
    }

    pub async fn create_ds_author(&self, body: &str) -> Result<HttpResponse> {
        if self.dry_run {
            tracing::info!(target: "dry_run", endpoint = "/api/ds-authors", size = body.len(), "skipped POST");
            // AuthorMigrator requires id > 0 to populate MappingCache; a positive
            // stub is what unblocks the article-transformer ds_author validation.
            return Ok(Self::fake_ok(r#"{"data":{"id":1}}"#));
        }
        let url = self.build_api_url("/api/ds-authors");
        self.http.post(&url, body, &self.auth_headers(true)).await
    }

    pub async fn bulk_add_admin_users(&self, body: &str) -> Result<HttpResponse> {
        if self.dry_run {
            tracing::info!(target: "dry_run", endpoint = "/migration/bulk-add-admin-users", size = body.len(), "skipped POST");
            return Ok(Self::fake_ok(r#"{"data":{"successMap":{},"failedAdmins":[]}}"#));
        }
        let url = self.build_api_url("/migration/bulk-add-admin-users");
        self.http.post(&url, body, &self.migration_auth_headers(true)).await
    }

    pub async fn bulk_add_users(&self, body: &str) -> Result<HttpResponse> {
        if self.dry_run {
            tracing::info!(target: "dry_run", endpoint = "/migration/bulk-add-users", size = body.len(), "skipped POST");
            return Ok(Self::fake_ok(r#"{"data":{"successMap":{},"failedUsers":[]}}"#));
        }
        let url = self.build_api_url("/migration/bulk-add-users");
        self.http.post(&url, body, &self.migration_auth_headers(true)).await
    }

    pub async fn find_existing_users(&self, identifiers: &[String]) -> Result<Value> {
        let url = self.build_api_url("/migration/find-users");
        let body = json!({ "identifiers": identifiers });
        match self.http.post(&url, &body.to_string(), &self.migration_auth_headers(true)).await {
            Ok(resp) if resp.is_success() => {
                Ok(serde_json::from_str::<Value>(&resp.body).unwrap_or(Value::Object(Default::default())))
            }
            Ok(resp) => {
                eprintln!("[find_existing_users] HTTP {}: {}", resp.status_code, resp.body);
                Ok(Value::Object(Default::default()))
            }
            Err(e) => {
                eprintln!("[find_existing_users] {e}");
                Ok(Value::Object(Default::default()))
            }
        }
    }
}
