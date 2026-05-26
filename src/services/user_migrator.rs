//! Bulk regular-user migration (batches of N to /migration/bulk-add-users).

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

const COMPONENT: &str = "UserMigrator";

/// Per-user (or per-record) failure detail captured during a batch.
#[derive(Debug, Clone)]
pub struct UserBatchFailure {
    pub wp_id: i64,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct User {
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
pub struct UserMigrationResult {
    pub success_map: HashMap<i64, i64>,
    pub failed_wp_ids: Vec<i64>,
    pub failure_reasons: HashMap<i64, String>,
    pub total: i32,
    pub succeeded: i32,
    pub failed: i32,
    pub duration_ms: f64,
}

pub struct UserMigrator {
    strapi: StrapiClient,
    mapping_path: PathBuf,
    failed_path: PathBuf,
    logger: Option<Arc<Logger>>,
}

impl UserMigrator {
    pub fn new(strapi: StrapiClient) -> Self {
        Self {
            strapi,
            mapping_path: PathBuf::from("data/mappings/users/users.json"),
            failed_path: PathBuf::from("data/reports/users-failed.json"),
            logger: None,
        }
    }

    pub fn with_logger(mut self, logger: Arc<Logger>) -> Self {
        self.logger = Some(logger);
        self
    }

    pub fn with_paths(strapi: StrapiClient, mapping_path: PathBuf, failed_path: PathBuf) -> Self {
        Self { strapi, mapping_path, failed_path, logger: None }
    }

    fn jlog(&self, meta: LogMetadata) {
        if let Some(l) = &self.logger { l.log(meta); }
    }

    pub fn load_users_from_json(path: impl AsRef<Path>) -> Result<Vec<User>> {
        let raw = fs::read_to_string(path.as_ref())
            .with_context(|| format!("read users: {}", path.as_ref().display()))?;
        let users: Vec<User> = serde_json::from_str(&raw)?;
        Ok(users)
    }

    pub async fn migrate(&self, users: &[User], image_mapping: &HashMap<String, i64>, batch_size: usize) -> Result<UserMigrationResult> {
        let mut out = UserMigrationResult::default();
        out.total = users.len() as i32;
        let t0 = std::time::Instant::now();

        self.jlog(LogMetadata::new(LogLevel::Info, COMPONENT)
            .with_operation("user_migration_start")
            .with_message(&format!("Migrating {} users in batches of {}", users.len(), batch_size)));

        let chunks = users.chunks(batch_size.max(1));
        for (i, chunk) in chunks.enumerate() {
            let batch_num = i + 1;
            let arr: Vec<Value> = chunk.iter().map(|u| {
                let email = if u.email.is_empty() {
                    format!("dailysquirt+{}@placeholder.com", u.username)
                } else {
                    u.email.clone()
                };
                let mut entry = json!({
                    "wpId": u.wp_id,
                    "username": u.username,
                    "email": email,
                    "slug": u.slug,
                });
                if !u.roles.is_empty() {
                    entry["roles"] = json!(u.roles);
                }
                // Match C++: field name "image" (not "mediaId"), populated only when avatar known.
                if !u.image.is_empty() {
                    if let Some(media_id) = image_mapping.get(&u.image).copied() {
                        entry["image"] = json!(media_id);
                    }
                }
                entry
            }).collect();

            // Match C++: payload root key "data" (not "users").
            let payload = json!({ "data": arr });
            let mut batch_failures: Vec<UserBatchFailure> = Vec::new();

            match self.strapi.bulk_add_users(&payload.to_string()).await {
                Ok(resp) => {
                    if !resp.is_success() {
                        warn!(batch = batch_num, status = resp.status_code, "user batch failed");
                        let reason = format!("HTTP {}: {}", resp.status_code, truncate(&resp.body, 200));
                        for u in chunk {
                            out.failed_wp_ids.push(u.wp_id);
                            out.failure_reasons.insert(u.wp_id, reason.clone());
                            out.failed += 1;
                            batch_failures.push(UserBatchFailure { wp_id: u.wp_id, reason: reason.clone() });
                        }
                        save_batch_failed_with_reasons(&self.failed_path, chunk, &batch_failures)?;
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(&resp.body) {
                        let data = v.pointer("/data").unwrap_or(&v);
                        if let Some(succ) = data.get("successMap").and_then(|x| x.as_object()) {
                            for (k, v) in succ {
                                if let (Ok(wp), Some(sid)) = (k.parse::<i64>(), v.as_i64()) {
                                    out.success_map.insert(wp, sid);
                                    out.succeeded += 1;
                                }
                            }
                        }
                        // C++ reads BOTH failedUsers (with reasons) and failedWpIds (id-only legacy).
                        if let Some(arr) = data.get("failedUsers").and_then(|x| x.as_array()) {
                            for it in arr {
                                if let Some(wp) = it.get("wpId").and_then(|x| x.as_i64()) {
                                    let reason = it.get("reason").and_then(|x| x.as_str()).unwrap_or("unknown").to_string();
                                    out.failed_wp_ids.push(wp);
                                    out.failure_reasons.insert(wp, reason.clone());
                                    out.failed += 1;
                                    batch_failures.push(UserBatchFailure { wp_id: wp, reason });
                                }
                            }
                        } else if let Some(arr) = data.get("failedWpIds").and_then(|x| x.as_array()) {
                            for it in arr {
                                if let Some(wp) = it.as_i64() {
                                    let reason = "unknown (legacy failedWpIds response)".to_string();
                                    out.failed_wp_ids.push(wp);
                                    out.failure_reasons.insert(wp, reason.clone());
                                    out.failed += 1;
                                    batch_failures.push(UserBatchFailure { wp_id: wp, reason });
                                }
                            }
                        }
                    }
                    save_batch_mapping(&self.mapping_path, &out.success_map)?;
                    if !batch_failures.is_empty() {
                        save_batch_failed_with_reasons(&self.failed_path, chunk, &batch_failures)?;
                    }
                }
                Err(e) => {
                    warn!(batch = batch_num, error = %e, "user batch error");
                    let reason = e.to_string();
                    for u in chunk {
                        out.failed_wp_ids.push(u.wp_id);
                        out.failure_reasons.insert(u.wp_id, reason.clone());
                        out.failed += 1;
                        batch_failures.push(UserBatchFailure { wp_id: u.wp_id, reason: reason.clone() });
                    }
                    save_batch_failed_with_reasons(&self.failed_path, chunk, &batch_failures)?;
                }
            }
            info!(batch = batch_num, succeeded = out.succeeded, failed = out.failed, "user batch done");
            self.jlog(LogMetadata {
                level: Some(LogLevel::Info),
                component: Some(COMPONENT.into()),
                operation: Some("user_batch_migrated".into()),
                successful_articles: None,
                message: Some(format!("Batch {batch_num}: {} succeeded, {} failed (cumulative)", out.succeeded, out.failed)),
                ..Default::default()
            });
        }

        out.duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.jlog(LogMetadata {
            level: Some(LogLevel::Info),
            component: Some(COMPONENT.into()),
            operation: Some("user_migration_complete".into()),
            total_articles: Some(out.total),
            successful_articles: Some(out.succeeded),
            failed_articles: Some(out.failed),
            total_duration_ms: Some(out.duration_ms),
            ..Default::default()
        });
        Ok(out)
    }
}

fn save_batch_mapping(path: &Path, map: &HashMap<i64, i64>) -> Result<()> {
    if let Some(p) = path.parent() { fs::create_dir_all(p).ok(); }
    let stringified: HashMap<String, i64> = map.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    fs::write(path, serde_json::to_string(&stringified)?)?;
    Ok(())
}

/// Append per-user failure records with their actual per-user reasons.
/// JSON shape mirrors C++ UserMigrator.cpp:466-505: failedWpIds[], failureReasons{}, failedRecords[].
fn save_batch_failed_with_reasons(
    path: &Path,
    chunk: &[User],
    failures: &[UserBatchFailure],
) -> Result<()> {
    if failures.is_empty() { return Ok(()); }
    if let Some(p) = path.parent() { fs::create_dir_all(p).ok(); }
    let mut existing: Value = if path.exists() {
        fs::read_to_string(path).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or_else(|| json!({ "failedWpIds": [], "failureReasons": {}, "failedRecords": [] }))
    } else {
        json!({ "failedWpIds": [], "failureReasons": {}, "failedRecords": [] })
    };
    // Match C++ timestamp format: integer nanoseconds since epoch.
    let now_ns: i64 = Utc::now().timestamp_nanos_opt().unwrap_or_else(|| Utc::now().timestamp() * 1_000_000_000);

    let by_id: HashMap<i64, &User> = chunk.iter().map(|u| (u.wp_id, u)).collect();

    if let Some(arr) = existing.get_mut("failedWpIds").and_then(|x| x.as_array_mut()) {
        for f in failures { arr.push(json!(f.wp_id)); }
    }
    if let Some(m) = existing.get_mut("failureReasons").and_then(|x| x.as_object_mut()) {
        for f in failures { m.insert(f.wp_id.to_string(), json!(f.reason)); }
    }
    if let Some(arr) = existing.get_mut("failedRecords").and_then(|x| x.as_array_mut()) {
        for f in failures {
            let u = by_id.get(&f.wp_id);
            arr.push(json!({
                "wpId": f.wp_id,
                "username": u.map(|x| x.username.clone()).unwrap_or_default(),
                "email": u.map(|x| x.email.clone()).unwrap_or_default(),
                "slug": u.map(|x| x.slug.clone()).unwrap_or_default(),
                "reason": f.reason,
                "timestamp": now_ns,
            }));
        }
    }
    fs::write(path, serde_json::to_string_pretty(&existing)?)?;
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { s.chars().take(n).collect() }
}
