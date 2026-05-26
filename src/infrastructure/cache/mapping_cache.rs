use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MappingType {
    Performer,
    Studio,
}

impl MappingType {
    pub fn as_str(self) -> &'static str {
        match self {
            MappingType::Performer => "performer",
            MappingType::Studio => "studio",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "performer" => Some(Self::Performer),
            "studio" => Some(Self::Studio),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MappingEntry {
    pub kind: MappingType,
    pub document_id: String,
}

/// Central WP-id -> Strapi-document-id translation cache.
/// File-backed JSON per mapping namespace, with thread-safe in-memory maps.
pub struct MappingCache {
    inner: Mutex<Inner>,
}

struct Inner {
    loaded: bool,
    global_map: HashMap<i64, MappingEntry>, // performers+studios keyed by WP id
    category_map: HashMap<i64, String>,
    sub_category_map: HashMap<i64, String>,
    admin_user_map: HashMap<i64, i64>,
    user_map: HashMap<i64, i64>,
    author_map: HashMap<i64, i64>,
    image_map: HashMap<String, i64>, // "IMG_N" -> Strapi media id

    base_dir: PathBuf,
}

impl MappingCache {
    pub fn new() -> Self {
        Self::with_base_dir("data/mappings")
    }

    pub fn with_base_dir(base: impl Into<PathBuf>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                loaded: false,
                global_map: HashMap::new(),
                category_map: HashMap::new(),
                sub_category_map: HashMap::new(),
                admin_user_map: HashMap::new(),
                user_map: HashMap::new(),
                author_map: HashMap::new(),
                image_map: HashMap::new(),
                base_dir: base.into(),
            }),
        }
    }

    fn perf_studio_path(base: &Path) -> PathBuf { base.join("pre-migration/performers-studios.json") }
    fn category_path(base: &Path) -> PathBuf { base.join("pre-migration/categories.json") }
    fn sub_category_path(base: &Path) -> PathBuf { base.join("pre-migration/subcategories.json") }
    fn admin_user_path(base: &Path) -> PathBuf { base.join("users/admin-users.json") }
    fn user_path(base: &Path) -> PathBuf { base.join("users/users.json") }
    fn author_path(base: &Path) -> PathBuf { base.join("users/authors.json") }
    fn image_path(base: &Path) -> PathBuf { base.join("users/images.json") }

    fn ensure_loaded(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.loaded {
            return Ok(());
        }

        // perf/studio
        let p = Self::perf_studio_path(&inner.base_dir);
        if p.exists() {
            let raw = fs::read_to_string(&p).unwrap_or_default();
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                if let Some(obj) = v.as_object() {
                    for (k, val) in obj {
                        if let (Ok(wp_id), Some(o)) = (k.parse::<i64>(), val.as_object()) {
                            let doc = o.get("documentId").and_then(|x| x.as_str()).unwrap_or("");
                            let kind = o
                                .get("type")
                                .and_then(|x| x.as_str())
                                .and_then(MappingType::parse)
                                .unwrap_or(MappingType::Performer);
                            inner.global_map.insert(wp_id, MappingEntry { kind, document_id: doc.to_string() });
                        }
                    }
                }
            }
        }

        // category
        let p = Self::category_path(&inner.base_dir);
        if p.exists() {
            if let Ok(raw) = fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<HashMap<String, String>>(&raw) {
                    for (k, doc) in v {
                        if let Ok(id) = k.parse::<i64>() { inner.category_map.insert(id, doc); }
                    }
                }
            }
        }

        // sub-category
        let p = Self::sub_category_path(&inner.base_dir);
        if p.exists() {
            if let Ok(raw) = fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<HashMap<String, String>>(&raw) {
                    for (k, doc) in v {
                        if let Ok(id) = k.parse::<i64>() { inner.sub_category_map.insert(id, doc); }
                    }
                }
            }
        }

        // admin-users
        let p = Self::admin_user_path(&inner.base_dir);
        if p.exists() {
            if let Ok(raw) = fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<HashMap<String, i64>>(&raw) {
                    for (k, sid) in v {
                        if let Ok(id) = k.parse::<i64>() { inner.admin_user_map.insert(id, sid); }
                    }
                }
            }
        }

        // users
        let p = Self::user_path(&inner.base_dir);
        if p.exists() {
            if let Ok(raw) = fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<HashMap<String, i64>>(&raw) {
                    for (k, sid) in v {
                        if let Ok(id) = k.parse::<i64>() { inner.user_map.insert(id, sid); }
                    }
                }
            }
        }

        // authors
        let p = Self::author_path(&inner.base_dir);
        if p.exists() {
            if let Ok(raw) = fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<HashMap<String, i64>>(&raw) {
                    for (k, sid) in v {
                        if let Ok(id) = k.parse::<i64>() { inner.author_map.insert(id, sid); }
                    }
                }
            }
        }

        // images
        let p = Self::image_path(&inner.base_dir);
        if p.exists() {
            if let Ok(raw) = fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<HashMap<String, i64>>(&raw) {
                    inner.image_map = v;
                }
            }
        }

        inner.loaded = true;
        Ok(())
    }

    fn save_perf_studio(&self) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        let path = Self::perf_studio_path(&inner.base_dir);
        if let Some(p) = path.parent() { fs::create_dir_all(p).ok(); }
        let mut obj = serde_json::Map::new();
        for (id, entry) in &inner.global_map {
            obj.insert(id.to_string(), json!({
                "documentId": entry.document_id,
                "type": entry.kind.as_str(),
            }));
        }
        fs::write(&path, Value::Object(obj).to_string())?;
        Ok(())
    }

    fn save_simple_string_map(path: &Path, m: &HashMap<i64, String>) -> Result<()> {
        if let Some(p) = path.parent() { fs::create_dir_all(p).ok(); }
        let stringified: HashMap<String, String> = m.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        fs::write(path, serde_json::to_string(&stringified)?)?;
        Ok(())
    }

    fn save_simple_int_map(path: &Path, m: &HashMap<i64, i64>) -> Result<()> {
        if let Some(p) = path.parent() { fs::create_dir_all(p).ok(); }
        let stringified: HashMap<String, i64> = m.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        fs::write(path, serde_json::to_string(&stringified)?)?;
        Ok(())
    }

    // ---------- putters (eager-write semantics) ----------
    pub fn put_performer(&self, wp_id: i64, document_id: impl Into<String>) {
        let _ = self.ensure_loaded();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.global_map.insert(wp_id, MappingEntry { kind: MappingType::Performer, document_id: document_id.into() });
        }
        let _ = self.save_perf_studio();
    }

    pub fn put_studio(&self, wp_id: i64, document_id: impl Into<String>) {
        let _ = self.ensure_loaded();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.global_map.insert(wp_id, MappingEntry { kind: MappingType::Studio, document_id: document_id.into() });
        }
        let _ = self.save_perf_studio();
    }

    pub fn put_category(&self, wp_id: i64, document_id: impl Into<String>) {
        let _ = self.ensure_loaded();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.category_map.insert(wp_id, document_id.into());
            let path = Self::category_path(&inner.base_dir);
            let _ = Self::save_simple_string_map(&path, &inner.category_map);
        }
    }

    pub fn put_sub_category(&self, wp_id: i64, document_id: impl Into<String>) {
        let _ = self.ensure_loaded();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.sub_category_map.insert(wp_id, document_id.into());
            let path = Self::sub_category_path(&inner.base_dir);
            let _ = Self::save_simple_string_map(&path, &inner.sub_category_map);
        }
    }

    pub fn put_admin_user(&self, wp_id: i64, strapi_id: i64) {
        let _ = self.ensure_loaded();
        let mut inner = self.inner.lock().unwrap();
        inner.admin_user_map.insert(wp_id, strapi_id);
        let path = Self::admin_user_path(&inner.base_dir);
        let _ = Self::save_simple_int_map(&path, &inner.admin_user_map);
    }

    pub fn put_user(&self, wp_id: i64, strapi_id: i64) {
        let _ = self.ensure_loaded();
        let mut inner = self.inner.lock().unwrap();
        inner.user_map.insert(wp_id, strapi_id);
        let path = Self::user_path(&inner.base_dir);
        let _ = Self::save_simple_int_map(&path, &inner.user_map);
    }

    pub fn put_author_mapping(&self, wp_id: i64, strapi_id: i64) {
        // batched: caller must flush via save_batch_author_mappings
        let _ = self.ensure_loaded();
        let mut inner = self.inner.lock().unwrap();
        inner.author_map.insert(wp_id, strapi_id);
    }

    pub fn put_image(&self, image_id: impl Into<String>, strapi_media_id: i64) {
        let _ = self.ensure_loaded();
        let mut inner = self.inner.lock().unwrap();
        inner.image_map.insert(image_id.into(), strapi_media_id);
        let path = Self::image_path(&inner.base_dir);
        if let Some(p) = path.parent() { fs::create_dir_all(p).ok(); }
        let _ = fs::write(&path, serde_json::to_string(&inner.image_map).unwrap_or_default());
    }

    // ---------- batch flushers ----------
    pub fn save_batch_author_mappings(&self) -> Result<()> {
        let _ = self.ensure_loaded();
        let inner = self.inner.lock().unwrap();
        let path = Self::author_path(&inner.base_dir);
        Self::save_simple_int_map(&path, &inner.author_map)
    }

    pub fn save_batch_user_mappings(&self) -> Result<()> {
        let _ = self.ensure_loaded();
        let inner = self.inner.lock().unwrap();
        let path = Self::user_path(&inner.base_dir);
        Self::save_simple_int_map(&path, &inner.user_map)
    }

    // ---------- getters ----------
    pub fn get_performer_document_id(&self, wp_id: i64) -> Option<String> {
        let _ = self.ensure_loaded();
        let inner = self.inner.lock().unwrap();
        inner.global_map.get(&wp_id).and_then(|e| {
            if e.kind == MappingType::Performer { Some(e.document_id.clone()) } else { None }
        })
    }

    pub fn get_studio_document_id(&self, wp_id: i64) -> Option<String> {
        let _ = self.ensure_loaded();
        let inner = self.inner.lock().unwrap();
        inner.global_map.get(&wp_id).and_then(|e| {
            if e.kind == MappingType::Studio { Some(e.document_id.clone()) } else { None }
        })
    }

    pub fn get_category_document_id(&self, wp_id: i64) -> String {
        let _ = self.ensure_loaded();
        let inner = self.inner.lock().unwrap();
        inner.sub_category_map.get(&wp_id)
            .cloned()
            .or_else(|| inner.category_map.get(&wp_id).cloned())
            .unwrap_or_default()
    }

    pub fn get_author_document_id(&self, wp_id: i64) -> String {
        let _ = self.ensure_loaded();
        let inner = self.inner.lock().unwrap();
        inner.author_map.get(&wp_id).map(|v| v.to_string()).unwrap_or_default()
    }

    /// Default Strapi user id used when no mapping is known (matches C++ stub value).
    /// Comment posts use this as a fallback to keep them from failing with userId=0.
    pub const FALLBACK_USER_ID: i64 = 3;

    pub fn get_user_strapi_id(&self, wp_id: i64) -> i64 {
        // C++ MappingCache.cpp:260-264 unconditionally returns 3 as a stub.
        // Rust port: look up the real mapping first, fall back to 3 (so comment
        // posts don't break when an author hasn't been migrated yet).
        let _ = self.ensure_loaded();
        let inner = self.inner.lock().unwrap();
        inner.user_map.get(&wp_id)
            .or_else(|| inner.admin_user_map.get(&wp_id))
            .copied()
            .unwrap_or(Self::FALLBACK_USER_ID)
    }

    pub fn get_admin_user_strapi_id(&self, wp_id: i64) -> Option<i64> {
        let _ = self.ensure_loaded();
        let inner = self.inner.lock().unwrap();
        inner.admin_user_map.get(&wp_id).copied()
    }

    pub fn get_image_strapi_media_id(&self, image_id: &str) -> Option<i64> {
        let _ = self.ensure_loaded();
        let inner = self.inner.lock().unwrap();
        inner.image_map.get(image_id).copied()
    }
}
