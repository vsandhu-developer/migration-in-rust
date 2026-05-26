use std::fs;
use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use serde_json::{json, Value};

use crate::models::CommentFailureRecord;

pub struct FailedCommentsCache;

impl FailedCommentsCache {
    pub fn load(path: &Path) -> Result<Vec<CommentFailureRecord>> {
        if !path.exists() { return Ok(Vec::new()); }
        let raw = fs::read_to_string(path)?;
        let v: Value = serde_json::from_str(&raw)?;
        let mut out = Vec::new();
        if let Some(arr) = v.get("failedComments").and_then(|x| x.as_array()) {
            for item in arr {
                if let Ok(rec) = serde_json::from_value::<CommentFailureRecord>(item.clone()) {
                    out.push(rec);
                }
            }
        }
        Ok(out)
    }

    pub fn save_all(path: &Path, records: &[CommentFailureRecord]) -> Result<()> {
        if let Some(p) = path.parent() { fs::create_dir_all(p).ok(); }
        let arr: Vec<Value> = records.iter().map(|r| serde_json::to_value(r).unwrap_or(Value::Null)).collect();
        let body = json!({
            "failedComments": arr,
            "lastUpdated": Utc::now().timestamp(),
            "totalFailedComments": records.len(),
        });
        fs::write(path, serde_json::to_string_pretty(&body)?)?;
        Ok(())
    }

    pub fn save(path: &Path, record: &CommentFailureRecord) -> Result<()> {
        let mut cur = Self::load(path).unwrap_or_default();
        if let Some(slot) = cur.iter_mut().find(|r| r.wp_comment_id == record.wp_comment_id) {
            *slot = record.clone();
        } else {
            cur.push(record.clone());
        }
        Self::save_all(path, &cur)
    }

    pub fn get_by_article(path: &Path, wp_post_id: i64) -> Result<Vec<CommentFailureRecord>> {
        Ok(Self::load(path)?.into_iter().filter(|r| r.wp_post_id == wp_post_id).collect())
    }
}
