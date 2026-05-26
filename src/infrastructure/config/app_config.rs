use std::env;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(rename = "wpBaseUrl", default)]
    pub wp_base_url: String,
    #[serde(rename = "strapiBaseUrl", default)]
    pub strapi_base_url: String,
    #[serde(default)]
    pub token: String,
}

impl Config {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let p = path.as_ref();
        let mut cfg: Config = if p.exists() {
            let raw = fs::read_to_string(p)
                .with_context(|| format!("read config: {}", p.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parse config: {}", p.display()))?
        } else {
            Config::default()
        };

        if let Ok(v) = env::var("WP_BASE_URL") {
            cfg.wp_base_url = v;
        }
        if let Ok(v) = env::var("STRAPI_BASE_URL") {
            cfg.strapi_base_url = v;
        }
        if let Ok(v) = env::var("STRAPI_TOKEN") {
            cfg.token = v;
        }
        Ok(cfg)
    }

    pub fn load_default() -> Result<Self> {
        Self::load_from_file("data/config/config.json")
    }
}
