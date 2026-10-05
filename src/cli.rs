use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Phase {
    Inventory,
    Import,
    Reconcile,
    Publish,
    Unpublish,
    ReleaseComments,
    Rollback,
    /// WordPress administrators/editors -> CMS admin accounts (`/api/ds-migration/admin-users`).
    AdminUsers,
    /// WordPress subscribers -> CMS reader accounts (`/api/ds-migration/users`).
    Users,
}
impl Phase {
    /// Account phases read `--users-file` instead of an approved content manifest.
    pub fn is_account(&self) -> bool {
        matches!(self, Phase::AdminUsers | Phase::Users)
    }
}
#[derive(Parser)]
#[command(
    version,
    about = "Daily Squirt importer: manifest-authorized content phases plus explicit admin-users/users account phases."
)]
pub struct Cli {
    #[arg(long)]
    pub config: PathBuf,
    /// Required for content phases; forbidden for admin-users/users.
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub phase: Phase,
    /// Operator selection label; persisted separately from the CMS-generated run ID.
    #[arg(long)]
    pub run_id: String,
    /// Comma-separated exact approved source IDs; never means first N. Content phases only.
    #[arg(long, value_delimiter = ',')]
    pub source_ids: Vec<String>,
    /// Content phases only.
    #[arg(long, value_delimiter = ',')]
    pub types: Vec<String>,
    /// Required for writes; parent must already exist outside this repository.
    #[arg(long)]
    pub checkpoint: Option<PathBuf>,
    #[arg(long)]
    pub dry_run: bool,
    /// Only valid with inventory/dry-run; no record can be marked complete.
    #[arg(long)]
    pub skip_images: bool,
    /// Required after interrupted or previously started runs.
    #[arg(long)]
    pub resume: bool,
    #[arg(long, default_value_t = 50)]
    pub wp_per_page: usize,
    /// Content phases: parallel source-fetch + CMS media uploads (1-8, default 2).
    /// Record writes stay sequential.
    #[arg(long)]
    pub media_concurrency: Option<usize>,
    /// admin-users/users only: JSON array of {wpId,username,email,slug,image,roles}.
    #[arg(long)]
    pub users_file: Option<PathBuf>,
    /// admin-users/users only: records per POST (users 1-200, default 200; admin-users 1-100, default 100).
    #[arg(long)]
    pub batch_size: Option<usize>,
}
