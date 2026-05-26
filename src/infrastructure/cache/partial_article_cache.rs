use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use serde_json::{json, Value};

use crate::models::PartialArticleRecord;

pub struct PartialArticleCache;

impl PartialArticleCache {
    pub fn load(path: &Path) -> Result<HashMap<i64, PartialArticleRecord>> {
        if !path.exists() { return Ok(HashMap::new()); }
        let raw = fs::read_to_string(path)?;
        let v: Value = serde_json::from_str(&raw)?;
        let mut map = HashMap::new();
        if let Some(arr) = v.get("articles").and_then(|x| x.as_array()) {
            for item in arr {
                if let Ok(rec) = serde_json::from_value::<PartialArticleRecord>(item.clone()) {
                    map.insert(rec.wp_post_id, rec);
                }
            }
        }
        Ok(map)
    }

    pub fn save_all(path: &Path, records: &HashMap<i64, PartialArticleRecord>) -> Result<()> {
        if let Some(p) = path.parent() { fs::create_dir_all(p).ok(); }
        let arr: Vec<Value> = records.values().map(|r| serde_json::to_value(r).unwrap_or(Value::Null)).collect();
        let body = json!({
            "articles": arr,
            "lastUpdated": Utc::now().timestamp(),
        });
        fs::write(path, serde_json::to_string_pretty(&body)?)?;
        Ok(())
    }

    pub fn save(path: &Path, record: &PartialArticleRecord) -> Result<()> {
        let mut current = Self::load(path).unwrap_or_default();
        current.insert(record.wp_post_id, record.clone());
        Self::save_all(path, &current)
    }

    pub fn remove(path: &Path, wp_post_id: i64) -> Result<()> {
        let mut current = Self::load(path).unwrap_or_default();
        current.remove(&wp_post_id);
        Self::save_all(path, &current)
    }
}
