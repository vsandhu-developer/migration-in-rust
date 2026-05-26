//! Pre-migration: populate Strapi taxonomies (performers, studios, categories, sub-cats)
//! before article migration runs.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::{info, warn};

use crate::clients::StrapiClient;
use crate::infrastructure::cache::MappingCache;
use crate::infrastructure::config::PreMigrationConfig;
use crate::models::{DsPerformer, DsStudio, ParentCategoryGroup, SubCategoryItem};

#[derive(Debug, Clone, Default)]
pub struct PreMigrationStats {
    pub performers_created: i32,
    pub performers_failed: i32,
    pub studios_created: i32,
    pub studios_failed: i32,
    pub categories_created: i32,
    pub categories_failed: i32,
    pub sub_categories_created: i32,
    pub sub_categories_failed: i32,
    pub duration_ms: f64,
    pub completed: bool,
    pub completion_message: String,
}

pub struct PreArticleMigration<'a> {
    strapi: &'a StrapiClient,
    cache: &'a MappingCache,
}

impl<'a> PreArticleMigration<'a> {
    pub fn new(strapi: &'a StrapiClient, cache: &'a MappingCache) -> Self {
        Self { strapi, cache }
    }

    pub async fn run(
        &self,
        cfg: &PreMigrationConfig,
        performers_csv: &Path,
        studios_csv: &Path,
        categories_json: &Path,
    ) -> Result<PreMigrationStats> {
        let mut stats = PreMigrationStats::default();
        let t0 = Instant::now();

        if !cfg.enabled {
            stats.completed = true;
            stats.completion_message = "Pre-migration disabled".into();
            return Ok(stats);
        }

        // Performers
        if cfg.migrate_performers {
            match Self::load_performers(performers_csv) {
                Ok(perfs) => {
                    for p in &perfs {
                        match self.strapi.create_ds_performer(&p.name, &p.slug).await {
                            Ok(r) => {
                                self.cache.put_performer(p.id, r.document_id.clone());
                                stats.performers_created += 1;
                            }
                            Err(e) => {
                                warn!(name = %p.name, error = %e, "performer create failed");
                                stats.performers_failed += 1;
                            }
                        }
                    }
                }
                Err(e) => warn!(error = %e, "load performers failed"),
            }
        }

        // Studios
        if cfg.migrate_studios {
            match Self::load_studios(studios_csv) {
                Ok(studios) => {
                    for s in &studios {
                        match self.strapi.create_ds_studio(&s.name, &s.slug).await {
                            Ok(r) => {
                                self.cache.put_studio(s.id, r.document_id.clone());
                                stats.studios_created += 1;
                            }
                            Err(e) => {
                                warn!(name = %s.name, error = %e, "studio create failed");
                                stats.studios_failed += 1;
                            }
                        }
                    }
                }
                Err(e) => warn!(error = %e, "load studios failed"),
            }
        }

        // Categories + sub-categories
        if cfg.migrate_categories || cfg.migrate_sub_categories {
            match Self::load_categories(categories_json) {
                Ok(mut groups) => {
                    for parent in &mut groups {
                        if cfg.migrate_categories {
                            match self.strapi.create_ds_category(&parent.name, &parent.slug, &parent.access_level).await {
                                Ok(r) => {
                                    parent.document_id = r.document_id.clone();
                                    self.cache.put_category(0, r.document_id);
                                    stats.categories_created += 1;
                                }
                                Err(e) => {
                                    warn!(name = %parent.name, error = %e, "category create failed");
                                    stats.categories_failed += 1;
                                    continue;
                                }
                            }
                        }
                        if cfg.migrate_sub_categories && !parent.document_id.is_empty() {
                            // dedup-by-slug within this parent
                            let mut seen: HashMap<String, String> = HashMap::new();
                            for sub in &parent.sub_categories {
                                if let Some(existing) = seen.get(&sub.slug) {
                                    self.cache.put_sub_category(sub.wp_id, existing.clone());
                                    stats.sub_categories_created += 1;
                                    continue;
                                }
                                match self.strapi.create_ds_sub_category(
                                    &sub.name, &sub.slug, &sub.access_level, &parent.document_id,
                                ).await {
                                    Ok(r) => {
                                        seen.insert(sub.slug.clone(), r.document_id.clone());
                                        self.cache.put_sub_category(sub.wp_id, r.document_id);
                                        stats.sub_categories_created += 1;
                                    }
                                    Err(e) => {
                                        warn!(name = %sub.name, error = %e, "sub-category create failed");
                                        stats.sub_categories_failed += 1;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => warn!(error = %e, "load categories failed"),
            }
        }

        stats.duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
        stats.completed = true;
        stats.completion_message = "Pre-migration completed successfully".into();
        info!(?stats, "pre-migration done");
        Ok(stats)
    }

    // ---------- file loaders ----------
    fn load_performers(path: &Path) -> Result<Vec<DsPerformer>> {
        let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_path(path)
            .with_context(|| format!("open performers csv: {}", path.display()))?;
        let mut out = Vec::new();
        for rec in rdr.records() {
            let rec = rec?;
            let id = rec.get(0).and_then(|s| s.trim().parse::<i64>().ok()).unwrap_or(0);
            let name = rec.get(1).unwrap_or("").trim().to_string();
            let slug = rec.get(2).unwrap_or("").trim().to_string();
            if id > 0 && !name.is_empty() {
                out.push(DsPerformer { id, name, slug });
            }
        }
        Ok(out)
    }

    fn load_studios(path: &Path) -> Result<Vec<DsStudio>> {
        let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_path(path)
            .with_context(|| format!("open studios csv: {}", path.display()))?;
        let mut out = Vec::new();
        for rec in rdr.records() {
            let rec = rec?;
            let id = rec.get(0).and_then(|s| s.trim().parse::<i64>().ok()).unwrap_or(0);
            let name = rec.get(1).unwrap_or("").trim().to_string();
            let slug = rec.get(2).unwrap_or("").trim().to_string();
            if id > 0 && !name.is_empty() {
                out.push(DsStudio { id, name, slug });
            }
        }
        Ok(out)
    }

    fn load_categories(path: &Path) -> Result<Vec<ParentCategoryGroup>> {
        let raw = fs::read_to_string(path).with_context(|| format!("read categories: {}", path.display()))?;
        let v: Value = serde_json::from_str(&raw)?;
        let mut groups = Vec::new();
        if let Some(obj) = v.as_object() {
            for (_, parent_val) in obj {
                let name = parent_val.get("dsName").and_then(|x| x.as_str()).unwrap_or_default().to_string();
                let slug = parent_val.get("dsSlug").and_then(|x| x.as_str()).unwrap_or_default().to_string();
                let access_level = parent_val.get("accessLevel").and_then(|x| x.as_str()).unwrap_or("Public").to_string();
                let mut group = ParentCategoryGroup { name, slug, access_level, ..Default::default() };
                if let Some(subs) = parent_val.get("SubCategories").and_then(|x| x.as_object()) {
                    for (_, sub_val) in subs {
                        let wp_id = sub_val.get("WPCategoryId").and_then(|x| x.as_i64()).unwrap_or(0);
                        let sname = sub_val.get("dsName").and_then(|x| x.as_str()).unwrap_or_default().to_string();
                        let sslug = sub_val.get("dsSlug").and_then(|x| x.as_str()).unwrap_or_default().to_string();
                        let salvl = sub_val.get("accessLevel").and_then(|x| x.as_str()).unwrap_or("Public").to_string();
                        group.sub_categories.push(SubCategoryItem {
                            wp_id, name: sname, slug: sslug, access_level: salvl,
                        });
                    }
                }
                groups.push(group);
            }
        }
        Ok(groups)
    }
}
