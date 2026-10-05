//! `admin-users` / `users` phases: restores the pre-rewrite account migration
//! (old `AdminUserMigrator`, `UserMigrator`, `UserReconciliationService`) on the
//! fixed `/api/ds-migration/{users,admin-users,users/lookup}` contract.
//!
//! Privacy: emails/usernames are only ever sent to the configured CMS. They never
//! enter stdout, stderr, errors or the checkpoint; reports name numeric wpIds only.
//! User images are accepted in the input and deliberately ignored (the old
//! importer's user image upload was disabled by default).
use crate::{
    canonical::sha256,
    checkpoint::{atomic_write, open_locked_dir},
    cli::{Cli, Phase},
    error::{require, Error, Result},
    http::{Boundary, Http},
    manifest::{read_file, Config},
};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;

pub const USERS_MAX_BATCH: usize = 200;
pub const ADMIN_USERS_MAX_BATCH: usize = 100;
pub const LOOKUP_MAX_BATCH: usize = 200;
const MAX_REPORTED_FAILURES: usize = 1000;
const STATE_FILE: &str = "users-state.json";

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceUser {
    pub wp_id: u64,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub slug: String,
    /// Accepted for input parity with users.json/admin_users.json; never uploaded.
    #[serde(default)]
    #[allow(dead_code)]
    image: String,
    #[serde(default)]
    pub roles: Vec<String>,
}
impl SourceUser {
    /// Parity with the old importer: a missing email becomes a unique,
    /// non-deliverable placeholder derived from the WordPress ID.
    pub fn email_or_placeholder(&self) -> (String, bool) {
        let email = self.email.trim();
        if email.is_empty() {
            (format!("migrated+{}@example.invalid", self.wp_id), true)
        } else {
            (email.to_owned(), false)
        }
    }
    fn payload(&self) -> Value {
        json!({"wpId":self.wp_id,"username":self.username,"email":self.email_or_placeholder().0,"slug":self.slug,"roles":self.roles})
    }
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserReport {
    pub phase: &'static str,
    pub selected: usize,
    pub created: usize,
    pub existing: usize,
    pub failed: usize,
    /// Records sent with the `migrated+{wpId}@example.invalid` placeholder.
    pub placeholder_email: usize,
    /// Already created/existing in the checkpoint; not re-sent on resume.
    pub already_complete: usize,
    /// Subset of `existing` recovered by the post-batch lookup (users phase only).
    pub recovered: usize,
    pub batches: usize,
    pub dry_run: bool,
    pub failures: Vec<UserFailure>,
    pub failures_truncated: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserFailure {
    pub wp_id: u64,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}
impl UserReport {
    pub fn passed(&self) -> bool {
        self.failed == 0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}
impl Entry {
    fn complete(&self) -> bool {
        matches!(self.status.as_str(), "created" | "existing") && self.user_id.is_some()
    }
}

/// Per-wpId status and wpId -> CMS userId mapping. Contains no emails/usernames.
pub struct UserCheckpoint {
    dir: PathBuf,
    _lock: File,
    identity: Value,
    pub entries: BTreeMap<u64, Entry>,
}
impl UserCheckpoint {
    pub fn open(path: &Path, identity: Value, resume: bool) -> Result<Self> {
        let (dir, lock) = open_locked_dir(path, resume)?;
        let state = dir.join(STATE_FILE);
        let mut entries = BTreeMap::new();
        if state.exists() {
            let saved: Value = serde_json::from_slice(&read_file(&state, 256 * 1024 * 1024)?)
                .map_err(|_| Error::new("checkpoint_corrupt"))?;
            require(
                saved["checksum"] == sha256(saved["entries"].to_string().as_bytes()),
                "checkpoint_checksum_mismatch",
            )?;
            require(saved["identity"] == identity, "checkpoint_identity_changed")?;
            let raw: BTreeMap<String, Entry> = serde_json::from_value(saved["entries"].clone())
                .map_err(|_| Error::new("checkpoint_corrupt"))?;
            for (k, v) in raw {
                entries.insert(
                    k.parse::<u64>()
                        .map_err(|_| Error::new("checkpoint_corrupt"))?,
                    v,
                );
            }
        } else {
            require(!resume, "checkpoint_state_missing")?;
        }
        let checkpoint = Self {
            dir,
            _lock: lock,
            identity,
            entries,
        };
        checkpoint.save()?;
        Ok(checkpoint)
    }
    pub fn save(&self) -> Result<()> {
        let entries = serde_json::to_value(
            self.entries
                .iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<BTreeMap<_, _>>(),
        )
        .map_err(|_| Error::new("checkpoint_json_invalid"))?;
        let mapping = self
            .entries
            .iter()
            .filter(|(_, e)| e.complete())
            .map(|(k, e)| (k.to_string(), json!(e.user_id)))
            .collect::<serde_json::Map<_, _>>();
        atomic_write(
            &self.dir.join(STATE_FILE),
            &json!({"version":1,"identity":self.identity,"checksum":sha256(entries.to_string().as_bytes()),"entries":entries,"mapping":mapping}),
        )
    }
    pub fn set(&mut self, wp_id: u64, entry: Entry) {
        // A completed mapping is never downgraded by a later failure.
        if self.entries.get(&wp_id).is_some_and(Entry::complete) && !entry.complete() {
            return;
        }
        self.entries.insert(wp_id, entry);
    }
}

pub fn load_users(path: &Path) -> Result<Vec<SourceUser>> {
    let path = path
        .canonicalize()
        .map_err(|_| Error::new("users_file_unreadable"))?;
    let bytes = read_file(&path, 256 * 1024 * 1024)?;
    // serde errors can quote input; never surface them.
    let users: Vec<SourceUser> =
        serde_json::from_slice(&bytes).map_err(|_| Error::new("users_file_invalid"))?;
    require(!users.is_empty(), "users_file_empty")?;
    let mut seen = BTreeSet::new();
    for u in &users {
        require(u.wp_id > 0, "users_wp_id_invalid")?;
        require(seen.insert(u.wp_id), "users_wp_id_duplicate")?;
        require(
            u.roles.len() <= 20 && u.roles.iter().all(|r| !r.is_empty() && r.len() <= 64),
            "users_roles_invalid",
        )?;
    }
    Ok(users)
}

fn safe_code(raw: &Value) -> String {
    raw.as_str()
        .filter(|s| {
            (2..=80).contains(&s.len())
                && s.as_bytes()[0].is_ascii_lowercase()
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
        .unwrap_or("cms_failed")
        .to_owned()
}
fn user_id(raw: &Value) -> Option<u64> {
    raw.as_u64().filter(|n| *n > 0)
}

#[derive(Clone)]
pub struct AccountCms {
    pub http: Http,
    pub origin: Url,
    pub authorization: String,
}
impl AccountCms {
    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let url = self
            .origin
            .join(path)
            .map_err(|_| Error::new("cms_path_invalid"))?;
        let v = self
            .http
            .json(Method::POST, url, Some(&self.authorization), Some(body))
            .await?;
        require(
            v["data"].is_object()
                && v.get("meta")
                    .and_then(|m| m.get("contractVersion"))
                    .is_none_or(|c| c == "v1"),
            "cms_contract_mismatch",
        )?;
        Ok(v["data"].clone())
    }
    /// Returns per-wpId results for exactly this batch. Missing/duplicate/foreign
    /// results are contract errors for that wpId, never silent successes.
    pub async fn create(
        &self,
        phase: &Phase,
        batch: &[&SourceUser],
    ) -> Result<BTreeMap<u64, Entry>> {
        let (path, max) = match phase {
            Phase::Users => ("/api/ds-migration/users", USERS_MAX_BATCH),
            Phase::AdminUsers => ("/api/ds-migration/admin-users", ADMIN_USERS_MAX_BATCH),
            _ => return Err(Error::new("account_phase_invalid")),
        };
        require(
            !batch.is_empty() && batch.len() <= max,
            "users_batch_invalid",
        )?;
        let body = json!({"data":batch.iter().map(|u|u.payload()).collect::<Vec<_>>()});
        let data = self.post(path, &body).await?;
        let results = data["results"]
            .as_array()
            .ok_or_else(|| Error::new("cms_users_response_invalid"))?;
        require(results.len() <= batch.len(), "cms_users_response_invalid")?;
        let wanted = batch.iter().map(|u| u.wp_id).collect::<BTreeSet<_>>();
        let mut out = BTreeMap::new();
        for r in results {
            let wp = r["wpId"]
                .as_u64()
                .ok_or_else(|| Error::new("cms_users_response_invalid"))?;
            require(wanted.contains(&wp), "cms_users_response_foreign")?;
            let entry = match (r["status"].as_str(), user_id(&r["userId"])) {
                (Some(s @ ("created" | "existing")), Some(id)) => Entry {
                    status: s.into(),
                    user_id: Some(id),
                    code: None,
                    http_status: None,
                },
                (Some("created" | "existing"), None) => Entry {
                    status: "failed".into(),
                    user_id: None,
                    code: Some("cms_user_id_invalid".into()),
                    http_status: None,
                },
                (Some("failed"), _) => Entry {
                    status: "failed".into(),
                    user_id: None,
                    code: Some(safe_code(&r["code"])),
                    http_status: None,
                },
                _ => Entry {
                    status: "failed".into(),
                    user_id: None,
                    code: Some("cms_user_status_invalid".into()),
                    http_status: None,
                },
            };
            require(
                out.insert(wp, entry).is_none(),
                "cms_users_response_duplicate",
            )?;
        }
        for wp in wanted {
            out.entry(wp).or_insert_with(|| Entry {
                status: "failed".into(),
                user_id: None,
                code: Some("cms_result_missing".into()),
                http_status: None,
            });
        }
        Ok(out)
    }
    /// `identifier -> userId` for found identifiers only.
    pub async fn lookup(&self, identifiers: &[String]) -> Result<BTreeMap<String, u64>> {
        require(
            !identifiers.is_empty() && identifiers.len() <= LOOKUP_MAX_BATCH,
            "users_lookup_batch_invalid",
        )?;
        let data = self
            .post(
                "/api/ds-migration/users/lookup",
                &json!({"identifiers":identifiers}),
            )
            .await?;
        let wanted = identifiers.iter().collect::<BTreeSet<_>>();
        let mut found = BTreeMap::new();
        for row in data["found"]
            .as_array()
            .ok_or_else(|| Error::new("cms_lookup_response_invalid"))?
        {
            let ident = row["identifier"]
                .as_str()
                .ok_or_else(|| Error::new("cms_lookup_response_invalid"))?;
            let id =
                user_id(&row["userId"]).ok_or_else(|| Error::new("cms_lookup_response_invalid"))?;
            require(
                wanted.contains(&ident.to_owned()),
                "cms_lookup_response_foreign",
            )?;
            require(
                found
                    .insert(ident.to_owned(), id)
                    .is_none_or(|old| old == id),
                "cms_lookup_response_conflict",
            )?;
        }
        require(data["notFound"].is_array(), "cms_lookup_response_invalid")?;
        Ok(found)
    }
}

fn phase_name(phase: &Phase) -> &'static str {
    match phase {
        Phase::AdminUsers => "admin-users",
        _ => "users",
    }
}

pub async fn execute(cli: &Cli) -> Result<UserReport> {
    require(cli.phase.is_account(), "account_phase_invalid")?;
    require(
        !cli.run_id.is_empty()
            && cli.run_id.len() <= 80
            && cli
                .run_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "run_label_invalid",
    )?;
    require(
        cli.manifest.is_none() && cli.source_ids.is_empty() && cli.types.is_empty(),
        "manifest_arguments_forbidden",
    )?;
    require(
        !cli.skip_images && cli.media_concurrency.is_none(),
        "media_arguments_not_applicable",
    )?;
    require(!(cli.dry_run && cli.resume), "resume_phase_invalid")?;
    let max = if cli.phase == Phase::Users {
        USERS_MAX_BATCH
    } else {
        ADMIN_USERS_MAX_BATCH
    };
    let batch_size = cli.batch_size.unwrap_or(max);
    require((1..=max).contains(&batch_size), "users_batch_size_invalid")?;
    let users_path = cli
        .users_file
        .as_ref()
        .ok_or_else(|| Error::new("users_file_required"))?;
    require(
        cli.dry_run || cli.checkpoint.is_some(),
        "checkpoint_required",
    )?;
    let config = Config::load(&cli.config)?;
    let boundary = Boundary::target(&config)?;
    let users = load_users(users_path)?;
    let file_hash = sha256(&read_file(
        &users_path
            .canonicalize()
            .map_err(|_| Error::new("users_file_unreadable"))?,
        256 * 1024 * 1024,
    )?);
    let mut report = UserReport {
        phase: phase_name(&cli.phase),
        selected: users.len(),
        dry_run: cli.dry_run,
        ..Default::default()
    };
    if cli.dry_run {
        // Validation and counts only: no token, network or checkpoint.
        report.placeholder_email = users.iter().filter(|u| u.email_or_placeholder().1).count();
        report.batches = users.len().div_ceil(batch_size);
        return Ok(report);
    }
    let token =
        std::env::var(&config.target_token_env).map_err(|_| Error::new("target_token_missing"))?;
    require(
        token.len() >= 16 && !token.chars().any(char::is_whitespace),
        "target_token_invalid",
    )?;
    let cms = AccountCms {
        http: Http {
            boundary,
            attempts: config.max_attempts,
            timeout: Duration::from_secs(config.timeout_seconds),
        },
        origin: config.target.clone(),
        authorization: format!("Bearer {token}"),
    };
    cms.http.boundary.addresses(&config.target).await?;
    let identity = json!({"version":1,"phase":report.phase,"label":cli.run_id,"usersFileSha256":file_hash,"targetFingerprint":sha256(config.target.as_str().as_bytes())});
    let mut cp = UserCheckpoint::open(
        cli.checkpoint
            .as_ref()
            .ok_or_else(|| Error::new("checkpoint_required"))?,
        identity,
        cli.resume,
    )?;
    let pending = users
        .iter()
        .filter(|u| !cp.entries.get(&u.wp_id).is_some_and(Entry::complete))
        .collect::<Vec<_>>();
    report.already_complete = users.len() - pending.len();
    let mut failed_now = Vec::new();
    for batch in pending.chunks(batch_size) {
        report.batches += 1;
        report.placeholder_email += batch.iter().filter(|u| u.email_or_placeholder().1).count();
        let results = match cms.create(&cli.phase, batch).await {
            Ok(results) => results,
            // Transport/5xx/429 were already retried by Http; the whole batch is
            // recorded as failed and remains eligible for lookup and --resume.
            Err(error) => batch
                .iter()
                .map(|u| {
                    (
                        u.wp_id,
                        Entry {
                            status: "failed".into(),
                            user_id: None,
                            code: Some(error.code.into()),
                            http_status: error.status,
                        },
                    )
                })
                .collect(),
        };
        for (wp, entry) in results {
            match entry.status.as_str() {
                "created" => report.created += 1,
                "existing" => report.existing += 1,
                _ => failed_now.push(wp),
            }
            cp.set(wp, entry);
        }
        cp.save()?;
    }
    // Old UserReconciliationService parity: failed regular users are looked up
    // by the email actually sent (real or placeholder) and mapped if they exist.
    if cli.phase == Phase::Users && !failed_now.is_empty() {
        let by_id = users
            .iter()
            .map(|u| (u.wp_id, u))
            .collect::<BTreeMap<_, _>>();
        let mut by_ident = BTreeMap::<String, Vec<u64>>::new();
        for wp in &failed_now {
            by_ident
                .entry(by_id[wp].email_or_placeholder().0)
                .or_default()
                .push(*wp);
        }
        let idents = by_ident.keys().cloned().collect::<Vec<_>>();
        for chunk in idents.chunks(LOOKUP_MAX_BATCH) {
            // A failed lookup leaves the original failure codes in place.
            let Ok(found) = cms.lookup(chunk).await else {
                continue;
            };
            for (ident, id) in found {
                for wp in &by_ident[&ident] {
                    cp.set(
                        *wp,
                        Entry {
                            status: "existing".into(),
                            user_id: Some(id),
                            code: None,
                            http_status: None,
                        },
                    );
                    report.existing += 1;
                    report.recovered += 1;
                }
            }
            cp.save()?;
        }
    }
    for wp in failed_now {
        let entry = &cp.entries[&wp];
        if entry.complete() {
            continue;
        }
        report.failed += 1;
        if report.failures.len() < MAX_REPORTED_FAILURES {
            report.failures.push(UserFailure {
                wp_id: wp,
                code: entry.code.clone().unwrap_or_else(|| "cms_failed".into()),
                status: entry.http_status,
            });
        } else {
            report.failures_truncated = true;
        }
    }
    cp.save()?;
    Ok(report)
}
