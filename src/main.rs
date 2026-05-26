//! Entry point. Drives all migration phases sequentially.
//! Each phase is wrapped so one phase failure does not abort the run.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing::{error, info, warn};

use migration_system::cli::{Cli, Phase};
use migration_system::clients::StrapiClient;
use migration_system::core::{MigrationRunner, PreArticleMigration};
use migration_system::infrastructure::cache::MappingCache;
use migration_system::infrastructure::config::{Config, PreMigrationConfig};
use migration_system::infrastructure::http::HttpClient;
use migration_system::infrastructure::logging::Logger;
use migration_system::services::admin_user_migrator::AdminUserMigrator;
use migration_system::services::author_migrator::AuthorMigrator;
use migration_system::services::user_image_downloader::UserImageDownloader;
use migration_system::services::user_image_uploader::UserImageUploader;
use migration_system::services::user_migrator::UserMigrator;
use migration_system::services::user_reconciliation_service::UserReconciliationService;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Cli::parse();

    fs::create_dir_all("logs").ok();
    fs::create_dir_all("data/incomplete").ok();
    fs::create_dir_all("data/reports").ok();
    fs::create_dir_all("data/processed").ok();
    fs::create_dir_all("data/mappings/users").ok();
    fs::create_dir_all("data/mappings/pre-migration").ok();
    fs::create_dir_all("data/users/downloaded").ok();

    let logger = Arc::new(Logger::new("logs", 50));
    logger.info("main", "Migration system starting");
    if args.dry_run {
        warn!("DRY RUN: no destructive Strapi calls will be made");
    }
    info!(
        only_phase = ?args.only_phase,
        max_articles = args.max_articles,
        dry_run = args.dry_run,
        wp_per_page = args.wp_per_page,
        "CLI args"
    );

    let cfg = Config::load_from_file(&args.config).unwrap_or_else(|e| {
        warn!(path = %args.config, error = %e, "config load failed, using defaults");
        Config::default()
    });
    info!(wp = %cfg.wp_base_url, strapi = %cfg.strapi_base_url, "config loaded");

    let wp_http = HttpClient::new()?;
    let strapi_http = HttpClient::new()?;
    let cache = Arc::new(MappingCache::new());

    let pre_cfg = PreMigrationConfig::load_from_file(&args.pre_config).unwrap_or_default();

    // ----------------- PHASE 1: Pre-article migration -----------------
    if args.should_run(Phase::Pre) {
        let strapi_client = StrapiClient::new(cfg.clone(), strapi_http.clone()).with_dry_run(args.dry_run);
        let pre = PreArticleMigration::new(&strapi_client, &cache);
        match pre.run(
            &pre_cfg,
            &PathBuf::from("data/source/performers/DS-Performers.csv"),
            &PathBuf::from("data/source/studios/DS-studio.csv"),
            &PathBuf::from("data/source/categories/DS-categories.json"),
        ).await {
            Ok(stats) => info!(
                performers_created = stats.performers_created,
                studios_created = stats.studios_created,
                categories_created = stats.categories_created,
                sub_categories_created = stats.sub_categories_created,
                "pre-migration done"
            ),
            Err(e) => error!(error = %e, "pre-migration failed"),
        }
    }

    // ----------------- PHASE 1.5: User images -----------------
    let mut image_mapping: HashMap<String, i64> = HashMap::new();
    if args.should_run(Phase::UserImages) && pre_cfg.download_user_images {
        let downloader = UserImageDownloader::new(strapi_http.clone());
        match downloader.download_all(
            "data/source/users/image_variables.json",
            "data/users/downloaded",
        ).await {
            Ok(batch) => {
                info!(
                    requested = batch.total_requested,
                    succeeded = batch.total_succeeded,
                    failed = batch.total_failed,
                    "user image download"
                );
                if !UserImageDownloader::validate_downloads(&batch) {
                    warn!("some downloaded files missing/empty");
                }
                if pre_cfg.upload_user_images {
                    let strapi_client = StrapiClient::new(cfg.clone(), strapi_http.clone()).with_dry_run(args.dry_run);
                    let uploader = UserImageUploader::new(strapi_client);
                    match uploader.upload_all(&batch.results).await {
                        Ok(uploaded) => {
                            info!(succeeded = uploaded.total_succeeded, "user images uploaded");
                            let _ = UserImageUploader::save_image_mapping(
                                "data/users/mapping-images.json",
                                &uploaded.image_id_to_strapi_media_id,
                            );
                            for (k, v) in &uploaded.image_id_to_strapi_media_id {
                                image_mapping.insert(k.clone(), *v);
                                cache.put_image(k.clone(), *v);
                            }
                        }
                        Err(e) => error!(error = %e, "user image upload failed"),
                    }
                }
            }
            Err(e) => error!(error = %e, "user image download failed"),
        }
    } else if let Ok(map) = UserImageUploader::load_image_mapping("data/users/mapping-images.json") {
        image_mapping = map;
    }

    // ----------------- PHASE 3: Admin users -----------------
    if args.should_run(Phase::AdminUsers) && pre_cfg.migrate_admin_users {
        match AdminUserMigrator::load_admin_users_from_json("data/source/users/admin_users.json") {
            Ok(admins) => {
                info!(count = admins.len(), "admin users loaded");
                let strapi_client = StrapiClient::new(cfg.clone(), strapi_http.clone()).with_dry_run(args.dry_run);
                let migrator = AdminUserMigrator::new(strapi_client).with_logger(logger.clone());
                match migrator.migrate(&admins, &image_mapping, pre_cfg.admin_user_batch_size as usize).await {
                    Ok(r) => {
                        info!(succeeded = r.succeeded, failed = r.failed, "admin users done");
                        for (wp, sid) in &r.success_map { cache.put_admin_user(*wp, *sid); }
                    }
                    Err(e) => error!(error = %e, "admin user migration failed"),
                }
            }
            Err(e) => warn!(error = %e, "no admin users to migrate"),
        }
    }

    // ----------------- PHASE 4: Regular users -----------------
    if args.should_run(Phase::Users) && pre_cfg.migrate_users {
        match UserMigrator::load_users_from_json("data/source/users/users.json") {
            Ok(users) => {
                info!(count = users.len(), "users loaded");
                let strapi_client = StrapiClient::new(cfg.clone(), strapi_http.clone()).with_dry_run(args.dry_run);
                let migrator = UserMigrator::new(strapi_client).with_logger(logger.clone());
                match migrator.migrate(&users, &image_mapping, pre_cfg.user_batch_size as usize).await {
                    Ok(r) => {
                        info!(succeeded = r.succeeded, failed = r.failed, "users done");
                        for (wp, sid) in &r.success_map { cache.put_user(*wp, *sid); }

                        if !r.failed_wp_ids.is_empty() && !args.dry_run {
                            let strapi_client = StrapiClient::new(cfg.clone(), strapi_http.clone());
                            let recon = UserReconciliationService::new(strapi_client).with_logger(logger.clone());
                            match recon.reconcile(&r.failed_wp_ids, &users).await {
                                Ok(summary) => {
                                    info!(recovered = summary.total_recovered, "reconciliation done");
                                    if !summary.recovered_mappings.is_empty() {
                                        let _ = UserReconciliationService::save_recovered_mappings(
                                            "data/mappings/users/users.json",
                                            &summary.recovered_mappings,
                                        );
                                        for (wp, sid) in &summary.recovered_mappings { cache.put_user(*wp, *sid); }
                                        let recovered_ids: Vec<i64> = summary.recovered_mappings.keys().copied().collect();
                                        let _ = UserReconciliationService::remove_recovered_from_failed_file(
                                            "data/reports/users-failed.json",
                                            &recovered_ids,
                                        );
                                    }
                                }
                                Err(e) => error!(error = %e, "reconciliation failed"),
                            }
                        }
                        let _ = cache.save_batch_user_mappings();
                    }
                    Err(e) => error!(error = %e, "user migration failed"),
                }
            }
            Err(e) => warn!(error = %e, "no users to migrate"),
        }
    }

    // ----------------- PHASE 5: Authors -----------------
    if args.should_run(Phase::Authors) && pre_cfg.migrate_authors {
        match AuthorMigrator::load_authors_from_json("data/source/users/author.json") {
            Ok(authors) => {
                info!(count = authors.len(), "authors loaded");
                let strapi_client = StrapiClient::new(cfg.clone(), strapi_http.clone()).with_dry_run(args.dry_run);
                let migrator = AuthorMigrator::new(strapi_client).with_logger(logger.clone());
                match migrator.migrate(&authors, &image_mapping).await {
                    Ok(r) => {
                        info!(succeeded = r.succeeded, failed = r.failed, "authors done");
                        for (wp, sid) in &r.success_map { cache.put_author_mapping(*wp, *sid); }
                        let _ = cache.save_batch_author_mappings();
                    }
                    Err(e) => error!(error = %e, "author migration failed"),
                }
            }
            Err(e) => warn!(error = %e, "no authors to migrate"),
        }
    }

    // ----------------- PHASE 2: Article migration -----------------
    if args.should_run(Phase::Articles) {
        let runner = MigrationRunner::new(cfg.clone(), wp_http.clone(), strapi_http.clone(), cache.clone(), logger.clone())
            .with_dry_run(args.dry_run)
            .with_max_articles(args.max_articles)
            .with_wp_per_page(args.wp_per_page);
        if let Err(e) = runner.run().await {
            error!(error = ?e, "article migration failed");
        }
    }

    logger.info("main", "Migration completed");
    logger.flush().ok();
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false).with_thread_ids(false))
        .init();
}
