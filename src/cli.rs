use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Phase {
    Pre,
    UserImages,
    AdminUsers,
    Users,
    Authors,
    Articles,
    All,
}

#[derive(Debug, Parser)]
#[command(
    name = "migration-system",
    about = "WordPress -> Strapi migration",
    version
)]
pub struct Cli {
    /// Path to config.json
    #[arg(long, default_value = "data/config/config.json")]
    pub config: String,

    /// Path to pre-migration-config.json
    #[arg(long, default_value = "data/config/pre-migration-config.json")]
    pub pre_config: String,

    /// Only run a single phase (skip everything else).
    #[arg(long, value_enum, default_value_t = Phase::All)]
    pub only_phase: Phase,

    /// Limit number of articles to migrate (0 = no limit).
    #[arg(long, default_value_t = 0)]
    pub max_articles: usize,

    /// Don't make destructive Strapi calls. Logs payloads but skips POSTs and uploads.
    #[arg(long)]
    pub dry_run: bool,

    /// Articles-phase page size for WP API.
    #[arg(long, default_value_t = 50)]
    pub wp_per_page: i32,
}

impl Cli {
    pub fn should_run(&self, phase: Phase) -> bool {
        matches!(self.only_phase, Phase::All) || self.only_phase == phase
    }
}
