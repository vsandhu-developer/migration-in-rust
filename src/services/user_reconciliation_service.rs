//! Reconcile failed users by finding duplicates in Strapi (email/username).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tracing::info;

use crate::clients::StrapiClient;
use crate::infrastructure::logging::{LogLevel, LogMetadata, Logger};
use crate::services::user_migrator::User;

const COMPONENT: &str = "UserReconciliationService";

#[derive(Debug, Clone)]
pub struct ReconciliationResult {
    pub wp_id: i64,
    pub recovered_strapi_id: i64,
    pub found_by: String, // "email" | "username" | "not_found"
    pub success: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct ReconciliationSummary {
    pub total_attempted: i32,
    pub total_recovered: i32,
    pub total_not_found: i32,
    pub results: Vec<ReconciliationResult>,
    pub recovered_mappings: HashMap<i64, i64>,
}

pub struct UserReconciliationService {
    strapi: StrapiClient,
    logger: Option<Arc<Logger>>,
}

impl UserReconciliationService {
    pub fn new(strapi: StrapiClient) -> Self { Self { strapi, logger: None } }

    pub fn with_logger(mut self, logger: Arc<Logger>) -> Self {
        self.logger = Some(logger);
        self
    }

    fn jlog(&self, meta: LogMetadata) {
        if let Some(l) = &self.logger { l.log(meta); }
    }

    pub async fn reconcile(&self, failed_wp_ids: &[i64], all_users: &[User]) -> Result<ReconciliationSummary> {
        let mut summary = ReconciliationSummary::default();
        summary.total_attempted = failed_wp_ids.len() as i32;
        self.jlog(LogMetadata::new(LogLevel::Info, COMPONENT)
            .with_operation("reconciliation_started")
            .with_message(&format!("Reconciling {} failed users", failed_wp_ids.len())));
        if failed_wp_ids.is_empty() {
            return Ok(summary);
        }

        // Build wpId -> (email, username) maps for failed users.
        let by_wp_id: HashMap<i64, &User> = all_users.iter().map(|u| (u.wp_id, u)).collect();
        let mut email_to_wp: HashMap<String, Vec<i64>> = HashMap::new();
        let mut username_to_wp: HashMap<String, Vec<i64>> = HashMap::new();
        let mut identifiers: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for &wp in failed_wp_ids {
            if let Some(u) = by_wp_id.get(&wp) {
                let ident = if !u.email.is_empty() { u.email.clone() } else { u.username.clone() };
                if ident.is_empty() { continue; }
                if !u.email.is_empty() {
                    email_to_wp.entry(u.email.clone()).or_default().push(wp);
                } else {
                    username_to_wp.entry(u.username.clone()).or_default().push(wp);
                }
                if seen.insert(ident.clone()) {
                    identifiers.push(ident);
                }
            }
        }

        let v = self.strapi.find_existing_users(&identifiers).await?;
        // Match C++ UserReconciliationService.cpp:119-185:
        // - foundUsers[i] has nested .user.id (not flat .strapiId)
        // - notFound[i] is an object with .identifier and optional .reason
        if let Some(arr) = v.pointer("/data/foundUsers").and_then(|x| x.as_array()) {
            for it in arr {
                let ident = it.get("identifier").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let sid = it.pointer("/user/id").and_then(|x| x.as_i64()).unwrap_or(0);
                let found_by = it.get("foundBy").and_then(|x| x.as_str()).unwrap_or("unknown").to_string();
                if sid == 0 || ident.is_empty() { continue; }
                let wp_ids = email_to_wp.get(&ident).cloned()
                    .or_else(|| username_to_wp.get(&ident).cloned())
                    .unwrap_or_default();
                for wp in wp_ids {
                    summary.recovered_mappings.insert(wp, sid);
                    summary.results.push(ReconciliationResult {
                        wp_id: wp,
                        recovered_strapi_id: sid,
                        found_by: found_by.clone(),
                        success: true,
                        reason: String::new(),
                    });
                    summary.total_recovered += 1;
                }
            }
        }
        if let Some(arr) = v.pointer("/data/notFound").and_then(|x| x.as_array()) {
            for it in arr {
                let ident = it.get("identifier").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if ident.is_empty() { continue; }
                let reason = it.get("reason").and_then(|x| x.as_str()).unwrap_or("Not found").to_string();
                let wp_ids = email_to_wp.get(&ident).cloned()
                    .or_else(|| username_to_wp.get(&ident).cloned())
                    .unwrap_or_default();
                for wp in wp_ids {
                    summary.results.push(ReconciliationResult {
                        wp_id: wp,
                        recovered_strapi_id: 0,
                        found_by: "not_found".into(),
                        success: false,
                        reason: reason.clone(),
                    });
                    summary.total_not_found += 1;
                }
            }
        }
        info!(
            attempted = summary.total_attempted,
            recovered = summary.total_recovered,
            not_found = summary.total_not_found,
            "reconciliation complete"
        );
        self.jlog(LogMetadata {
            level: Some(LogLevel::Info),
            component: Some(COMPONENT.into()),
            operation: Some("reconciliation_complete".into()),
            message: Some(format!(
                "Reconciled {} users, {} not found",
                summary.total_recovered, summary.total_not_found
            )),
            ..Default::default()
        });
        Ok(summary)
    }

    pub fn save_recovered_mappings(path: impl AsRef<Path>, recovered: &HashMap<i64, i64>) -> Result<()> {
        let p = path.as_ref();
        if let Some(parent) = p.parent() { fs::create_dir_all(parent).ok(); }
        let mut existing: HashMap<String, i64> = if p.exists() {
            fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
        } else {
            HashMap::new()
        };
        for (wp, sid) in recovered {
            existing.insert(wp.to_string(), *sid);
        }
        fs::write(p, serde_json::to_string_pretty(&existing)?)?;
        Ok(())
    }

    pub fn remove_recovered_from_failed_file(path: impl AsRef<Path>, recovered_wp_ids: &[i64]) -> Result<()> {
        let p = path.as_ref();
        if !p.exists() { return Ok(()); }
        let raw = fs::read_to_string(p).context("read failed file")?;
        let mut v: Value = serde_json::from_str(&raw)?;
        let recovered: HashSet<i64> = recovered_wp_ids.iter().copied().collect();
        if let Some(arr) = v.get_mut("failedWpIds").and_then(|x| x.as_array_mut()) {
            arr.retain(|x| x.as_i64().map(|id| !recovered.contains(&id)).unwrap_or(true));
        }
        if let Some(map) = v.get_mut("failureReasons").and_then(|x| x.as_object_mut()) {
            map.retain(|k, _| {
                if let Ok(id) = k.parse::<i64>() { !recovered.contains(&id) } else { true }
            });
        }
        if let Some(arr) = v.get_mut("failedRecords").and_then(|x| x.as_array_mut()) {
            arr.retain(|x| {
                x.get("wpId").and_then(|i| i.as_i64()).map(|id| !recovered.contains(&id)).unwrap_or(true)
            });
        }
        fs::write(p, serde_json::to_string_pretty(&v)?)?;
        Ok(())
    }
}

// Allow downstream code to build `json!` payloads if needed.
#[allow(dead_code)]
fn _placeholder() -> Value { json!({}) }
