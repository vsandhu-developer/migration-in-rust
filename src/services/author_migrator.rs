//! Author migration: creates ds-author content type entries one at a time.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::clients::StrapiClient;
use crate::infrastructure::logging::{LogLevel, LogMetadata, Logger};

const COMPONENT: &str = "AuthorMigrator";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Author {
    #[serde(rename = "wpId", default)]
    pub wp_id: i64,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub image: String,
}

#[derive(Debug, Clone, Default)]
pub struct AuthorMigrationResult {
    pub success_map: HashMap<i64, i64>,
    pub failed_wp_ids: Vec<i64>,
    pub failed_with_reasons: HashMap<i64, String>,
    pub total: i32,
    pub succeeded: i32,
    pub failed: i32,
    pub duration_ms: f64,
}

pub struct AuthorMigrator {
    strapi: StrapiClient,
    mapping_path: PathBuf,
    failed_path: PathBuf,
    logger: Option<Arc<Logger>>,
}

impl AuthorMigrator {
    pub fn new(strapi: StrapiClient) -> Self {
        Self {
            strapi,
            mapping_path: PathBuf::from("data/mappings/users/authors.json"),
            failed_path: PathBuf::from("data/reports/authors-failed.json"),
            logger: None,
        }
    }

    pub fn with_logger(mut self, logger: Arc<Logger>) -> Self {
        self.logger = Some(logger);
        self
    }

    fn jlog(&self, meta: LogMetadata) {
        if let Some(l) = &self.logger { l.log(meta); }
    }

    pub fn load_authors_from_json(path: impl AsRef<Path>) -> Result<Vec<Author>> {
        let raw = fs::read_to_string(path.as_ref())
            .with_context(|| format!("read authors: {}", path.as_ref().display()))?;
        let authors: Vec<Author> = serde_json::from_str(&raw)?;
        Ok(authors)
    }

    pub async fn migrate(&self, authors: &[Author], image_mapping: &HashMap<String, i64>) -> Result<AuthorMigrationResult> {
        let mut out = AuthorMigrationResult::default();
        out.total = authors.len() as i32;
        let t0 = std::time::Instant::now();

        self.jlog(LogMetadata::new(LogLevel::Info, COMPONENT)
            .with_operation("author_migration_start")
            .with_message(&format!("Migrating {} authors", authors.len())));

        for author in authors {
            // Match C++ AuthorMigrator.cpp:165-179 exactly: Name + (email if non-empty)
            // + (Avtar if image mapped) + hardcoded publishedAt. No Slug, no Email when empty.
            let mut data = json!({ "Name": author.username });
            if !author.email.is_empty() {
                data["email"] = json!(author.email);
            }
            if !author.image.is_empty() {
                if let Some(media_id) = image_mapping.get(&author.image).copied() {
                    data["Avtar"] = json!(media_id); // typo preserved from C++ + Strapi schema
                }
            }
            data["publishedAt"] = json!("2026-05-25T00:00:00Z");
            let body = json!({ "data": data });
            match self.strapi.create_ds_author(&body.to_string()).await {
                Ok(resp) if resp.is_success() => {
                    if let Ok(v) = serde_json::from_str::<Value>(&resp.body) {
                        let id = v.pointer("/data/id").and_then(|x| x.as_i64()).unwrap_or(0);
                        if id > 0 {
                            out.success_map.insert(author.wp_id, id);
                            out.succeeded += 1;
                            self.jlog(LogMetadata {
                                level: Some(LogLevel::Debug),
                                component: Some(COMPONENT.into()),
                                operation: Some("author_created".into()),
                                strapi_doc_id: Some(id.to_string()),
                                message: Some(format!("Author wpId={} -> strapiId={}", author.wp_id, id)),
                                ..Default::default()
                            });
                        } else {
                            out.failed_wp_ids.push(author.wp_id);
                            out.failed_with_reasons.insert(author.wp_id, "no id in response".into());
                            out.failed += 1;
                            self.jlog(LogMetadata {
                                level: Some(LogLevel::Warn),
                                component: Some(COMPONENT.into()),
                                operation: Some("author_creation_failed".into()),
                                error_type: Some("PARSE_ERROR".into()),
                                message: Some(format!("Author wpId={}: no id in response", author.wp_id)),
                                ..Default::default()
                            });
                        }
                    }
                }
                Ok(resp) => {
                    warn!(wp_id = author.wp_id, status = resp.status_code, "author create failed");
                    let reason = format!("HTTP {}: {}", resp.status_code, truncate(&resp.body, 200));
                    out.failed_wp_ids.push(author.wp_id);
                    out.failed_with_reasons.insert(author.wp_id, reason.clone());
                    out.failed += 1;
                    self.jlog(LogMetadata {
                        level: Some(LogLevel::Warn),
                        component: Some(COMPONENT.into()),
                        operation: Some("author_creation_failed".into()),
                        http_status: Some(resp.status_code as i32),
                        response_snippet: Some(truncate(&resp.body, 200)),
                        message: Some(reason),
                        ..Default::default()
                    });
                }
                Err(e) => {
                    warn!(wp_id = author.wp_id, error = %e, "author create error");
                    let reason = e.to_string();
                    out.failed_wp_ids.push(author.wp_id);
                    out.failed_with_reasons.insert(author.wp_id, reason.clone());
                    out.failed += 1;
                    self.jlog(LogMetadata {
                        level: Some(LogLevel::Warn),
                        component: Some(COMPONENT.into()),
                        operation: Some("author_creation_error".into()),
                        message: Some(reason),
                        ..Default::default()
                    });
                }
            }
        }

        self.save_batch_author_mappings(&out.success_map)?;
        self.save_batch_author_failures(&out.failed_with_reasons, authors)?;
        out.duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
        info!(succeeded = out.succeeded, failed = out.failed, "authors complete");
        self.jlog(LogMetadata {
            level: Some(LogLevel::Info),
            component: Some(COMPONENT.into()),
            operation: Some("author_migration_complete".into()),
            total_articles: Some(out.total),
            successful_articles: Some(out.succeeded),
            failed_articles: Some(out.failed),
            total_duration_ms: Some(out.duration_ms),
            ..Default::default()
        });
        Ok(out)
    }

    fn save_batch_author_mappings(&self, map: &HashMap<i64, i64>) -> Result<()> {
        if let Some(p) = self.mapping_path.parent() { fs::create_dir_all(p).ok(); }
        let stringified: HashMap<String, i64> = map.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        fs::write(&self.mapping_path, serde_json::to_string(&stringified)?)?;
        Ok(())
    }

    fn save_batch_author_failures(&self, failures: &HashMap<i64, String>, authors: &[Author]) -> Result<()> {
        if failures.is_empty() { return Ok(()); }
        if let Some(p) = self.failed_path.parent() { fs::create_dir_all(p).ok(); }
        let now = Utc::now().to_rfc3339();
        let recs: Vec<Value> = authors.iter()
            .filter_map(|a| failures.get(&a.wp_id).map(|r| json!({
                "wpId": a.wp_id, "username": a.username, "email": a.email,
                "slug": a.slug, "reason": r, "timestamp": now,
            })))
            .collect();
        let body = json!({
            "failedWpIds": failures.keys().collect::<Vec<_>>(),
            "failureReasons": failures.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<HashMap<_, _>>(),
            "failedRecords": recs,
        });
        fs::write(&self.failed_path, serde_json::to_string_pretty(&body)?)?;
        Ok(())
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { s.chars().take(n).collect() }
}
