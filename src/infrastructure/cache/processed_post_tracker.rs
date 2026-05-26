use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;

/// Idempotency tracker: stores WP post IDs that have already been migrated.
pub struct ProcessedPostTracker {
    inner: Mutex<Inner>,
}

struct Inner {
    path: PathBuf,
    set: HashSet<i64>,
    loaded: bool,
}

impl ProcessedPostTracker {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            inner: Mutex::new(Inner { path: path.into(), set: HashSet::new(), loaded: false }),
        }
    }

    pub fn default_path() -> Self {
        Self::new("data/processed/processed_posts.json")
    }

    fn ensure_loaded(inner: &mut Inner) {
        if inner.loaded { return; }
        if inner.path.exists() {
            if let Ok(raw) = fs::read_to_string(&inner.path) {
                if let Ok(v) = serde_json::from_str::<Vec<i64>>(&raw) {
                    inner.set = v.into_iter().collect();
                }
            }
        }
        inner.loaded = true;
    }

    pub fn contains(&self, post_id: i64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        Self::ensure_loaded(&mut inner);
        inner.set.contains(&post_id)
    }

    pub fn mark_processed(&self, post_id: i64) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        Self::ensure_loaded(&mut inner);
        if inner.set.insert(post_id) {
            if let Some(parent) = inner.path.parent() {
                fs::create_dir_all(parent).ok();
            }
            let v: Vec<i64> = inner.set.iter().copied().collect();
            fs::write(&inner.path, serde_json::to_string(&v)?)?;
        }
        Ok(())
    }

    pub fn size(&self) -> usize {
        let mut inner = self.inner.lock().unwrap();
        Self::ensure_loaded(&mut inner);
        inner.set.len()
    }
}
