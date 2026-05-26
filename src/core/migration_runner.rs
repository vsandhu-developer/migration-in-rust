//! Article-phase runner: fetch posts, dedupe, drive ArticleMigrationOrchestrator.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use std::panic::AssertUnwindSafe;

use anyhow::Result;
use futures::FutureExt;
use tracing::{info, warn};

use crate::clients::{StrapiClient, WpClient};
use crate::core::ArticleMigrationOrchestrator;
use crate::infrastructure::cache::{MappingCache, ProcessedPostTracker};
use crate::infrastructure::config::Config;
use crate::infrastructure::http::HttpClient;
use crate::infrastructure::logging::{LogLevel, LogMetadata, Logger};

pub struct MigrationRunner {
    cfg: Config,
    wp_http: HttpClient,
    strapi_http: HttpClient,
    cache: Arc<MappingCache>,
    logger: Arc<Logger>,
    processed: ProcessedPostTracker,
    partial_path: PathBuf,
    failed_comments_path: PathBuf,
    dry_run: bool,
    max_articles: usize,
    wp_per_page: i32,
}

impl MigrationRunner {
    pub fn new(
        cfg: Config,
        wp_http: HttpClient,
        strapi_http: HttpClient,
        cache: Arc<MappingCache>,
        logger: Arc<Logger>,
    ) -> Self {
        Self {
            cfg,
            wp_http,
            strapi_http,
            cache,
            logger,
            processed: ProcessedPostTracker::default_path(),
            partial_path: PathBuf::from("data/incomplete/partial-articles.json"),
            failed_comments_path: PathBuf::from("data/incomplete/failed-comments.json"),
            dry_run: false,
            max_articles: 0,
            wp_per_page: 50,
        }
    }

    pub fn with_dry_run(mut self, dry_run: bool) -> Self { self.dry_run = dry_run; self }
    pub fn with_max_articles(mut self, n: usize) -> Self { self.max_articles = n; self }
    pub fn with_wp_per_page(mut self, n: i32) -> Self { self.wp_per_page = n; self }

    pub async fn run(&self) -> Result<()> {
        let wall_start = Instant::now();

        let strapi_client = StrapiClient::new(self.cfg.clone(), self.strapi_http.clone())
            .with_dry_run(self.dry_run);
        let wp_client = WpClient::new(self.cfg.clone(), self.wp_http.clone());

        let orchestrator = ArticleMigrationOrchestrator::new(
            strapi_client,
            self.wp_http.clone(),
            self.cache.clone(),
            self.logger.clone(),
            self.partial_path.clone(),
            self.failed_comments_path.clone(),
        );

        // Fetch posts
        let t = Instant::now();
        let mut posts = wp_client.get_posts(self.wp_per_page, true).await?;
        if self.max_articles > 0 && posts.len() > self.max_articles {
            posts.truncate(self.max_articles);
            info!(limit = self.max_articles, "truncating to max_articles");
        }
        let fetch_ms = t.elapsed().as_secs_f64() * 1000.0;
        info!(count = posts.len(), duration_ms = fetch_ms, "fetched WP posts");

        if posts.is_empty() {
            warn!("no posts to migrate");
            return Ok(());
        }

        let mut success: i32 = 0;
        let mut failure: i32 = 0;
        for post in &posts {
            if self.processed.contains(post.id) {
                warn!(wp_post_id = post.id, "already processed, skipping");
                continue;
            }
            // Wrap each per-article call so a panic in one post doesn't crash the run.
            let fut = AssertUnwindSafe(orchestrator.migrate_post(post)).catch_unwind();
            let ok = match fut.await {
                Ok(b) => b,
                Err(panic_payload) => {
                    let msg = panic_message(panic_payload);
                    self.logger.log(LogMetadata {
                        level: Some(LogLevel::Fatal),
                        component: Some("MigrationRunner".into()),
                        operation: Some("critical_exception".into()),
                        wp_post_id: Some(post.id),
                        error_type: Some("UNKNOWN_ERROR".into()),
                        message: Some(format!("Panic during migrate_post: {msg}")),
                        ..Default::default()
                    });
                    false
                }
            };
            if ok {
                if !self.dry_run {
                    if let Err(e) = self.processed.mark_processed(post.id) {
                        warn!(error = %e, "mark_processed failed");
                    }
                }
                success += 1;
            } else {
                failure += 1;
            }
        }

        let total_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
        info!(
            successful = success,
            failed = failure,
            total = posts.len(),
            duration_ms = total_ms,
            "article migration complete"
        );
        self.logger.flush().ok();
        Ok(())
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}
