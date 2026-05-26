use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn t() -> bool { true }
fn f() -> bool { false }
fn d_admin_batch() -> i32 { 100 }
fn d_user_batch() -> i32 { 1000 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreMigrationConfig {
    #[serde(default = "t")]
    pub enabled: bool,
    #[serde(default = "f", rename = "migratePerformers")]
    pub migrate_performers: bool,
    #[serde(default = "f", rename = "migrateStudios")]
    pub migrate_studios: bool,
    #[serde(default = "t", rename = "migrateCategories")]
    pub migrate_categories: bool,
    #[serde(default = "t", rename = "migrateSubCategories")]
    pub migrate_sub_categories: bool,
    #[serde(default = "t", rename = "downloadUserImages")]
    pub download_user_images: bool,
    #[serde(default = "t", rename = "uploadUserImages")]
    pub upload_user_images: bool,
    #[serde(default = "t", rename = "migrateAdminUsers")]
    pub migrate_admin_users: bool,
    #[serde(default = "t", rename = "migrateUsers")]
    pub migrate_users: bool,
    #[serde(default = "t", rename = "migrateAuthors")]
    pub migrate_authors: bool,
    #[serde(default = "d_admin_batch", rename = "adminUserBatchSize")]
    pub admin_user_batch_size: i32,
    #[serde(default = "d_user_batch", rename = "userBatchSize")]
    pub user_batch_size: i32,

    #[serde(skip)]
    pub raw: Value,
}

impl Default for PreMigrationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            migrate_performers: false,
            migrate_studios: false,
            migrate_categories: true,
            migrate_sub_categories: true,
            download_user_images: true,
            upload_user_images: true,
            migrate_admin_users: true,
            migrate_users: true,
            migrate_authors: true,
            admin_user_batch_size: 100,
            user_batch_size: 1000,
            raw: Value::Null,
        }
    }
}

impl PreMigrationConfig {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let p = path.as_ref();
        if !p.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(p)
            .with_context(|| format!("read pre-migration config: {}", p.display()))?;
        let cfg: Self = serde_json::from_str(&raw)
            .with_context(|| format!("parse pre-migration config: {}", p.display()))?;
        Ok(cfg)
    }

    pub fn load_default() -> Result<Self> {
        Self::load_from_file("data/config/pre-migration-config.json")
    }

    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let p = path.as_ref();
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).ok();
        }
        let s = serde_json::to_string_pretty(self)?;
        fs::write(p, s)?;
        Ok(())
    }
}
