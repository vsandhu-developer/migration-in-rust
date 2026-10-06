use crate::{
    canonical::{sha256, source_key},
    checkpoint::Checkpoint,
    cli::{Cli, Phase},
    error::{require, Error, Result},
    http::{Boundary, Http},
    manifest::{Config, Manifest, Record},
    protocol::{record_body, Cms, MAX_RECORD_BATCH, RECORD_BATCH_BYTES},
    source::{apply_wordpress, fetch_media, rewrite_html_with_frames, Wordpress},
};
use futures::{stream, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

pub const DEFAULT_MEDIA_CONCURRENCY: usize = 2;
pub const MAX_MEDIA_CONCURRENCY: usize = 8;
pub const DEFAULT_RECORD_BATCH: usize = MAX_RECORD_BATCH;

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub selected: usize,
    pub created: usize,
    pub updated: usize,
    pub skipped: usize,
    pub exceptions: usize,
    pub failed: usize,
    pub media_selected: usize,
    pub media_created: usize,
    pub media_reused: usize,
    pub media_failed: usize,
    /// Record write requests sent (single-record POSTs plus batch POSTs, excluding retries).
    pub record_requests: usize,
    pub dry_run: bool,
    pub failures: Vec<Failure>,
    pub run_id: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub record_fingerprint: String,
    pub kind: String,
    pub code: &'static str,
    pub status: Option<u16>,
}
impl Report {
    fn failure(&mut self, kind: &str, id: &str, error: &Error) {
        self.failures.push(Failure {
            record_fingerprint: sha256(id.as_bytes()),
            kind: kind.into(),
            code: error.code,
            status: error.status,
        });
    }
    pub fn passed(&self) -> bool {
        self.failed == 0 && self.media_failed == 0 && self.failures.is_empty()
    }
}
pub fn media_concurrency(cli: &Cli) -> usize {
    cli.media_concurrency.unwrap_or(DEFAULT_MEDIA_CONCURRENCY)
}
pub fn record_batch_size(cli: &Cli) -> usize {
    cli.record_batch_size.unwrap_or(DEFAULT_RECORD_BATCH)
}
/// Articles stay one per request: large HTML bodies and per-article media verification would
/// make a 100-article request slow and close to the CMS 1 MiB JSON body limit.
fn batchable_type(typ: &str) -> bool {
    typ != "article"
}
type PendingRecord = (String, String, Value);
fn apply_record(
    report: &mut Report,
    cp: &mut Checkpoint,
    typ: &str,
    id: &str,
    result: Result<Value>,
) -> Result<()> {
    match result {
        Ok(row) => {
            match row["operation"].as_str() {
                Some("created") => report.created += 1,
                Some("updated") => report.updated += 1,
                _ => report.skipped += 1,
            }
            cp.record(&row)
        }
        Err(e) => {
            report.failed += 1;
            report.failure(typ, id, &e);
            Ok(())
        }
    }
}
/// Writes the pending same-type records: one batch request when batching is available, otherwise
/// (or for a single record, or after the CMS reported the batch route/scope unavailable) one
/// request per record. Every item is journaled in the checkpoint individually, in order.
async fn flush_records(
    cms: &Cms,
    run: &str,
    pending: &mut Vec<PendingRecord>,
    batching: &mut bool,
    report: &mut Report,
    cp: &mut Checkpoint,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let items = std::mem::take(pending);
    let typ = items[0].0.clone();
    let mut results = None;
    if *batching && items.len() > 1 {
        let bodies = items.iter().map(|i| i.2.clone()).collect::<Vec<_>>();
        report.record_requests += 1;
        match cms.record_batch(run, &typ, &bodies).await {
            Ok(r) => results = Some(r),
            Err(e) if e.code == "record_batch_unsupported" => {
                eprintln!(
                    "{}",
                    json!({"notice":"record_batch_unsupported","status":e.status,"fallback":"single-record requests"})
                );
                *batching = false;
            }
            Err(e) => results = Some(items.iter().map(|_| Err(e.clone())).collect()),
        }
    }
    let results = match results {
        Some(r) => r,
        None => {
            let mut r = Vec::with_capacity(items.len());
            for (t, _, body) in &items {
                report.record_requests += 1;
                r.push(cms.record(run, t, body).await);
            }
            r
        }
    };
    for ((t, id, _), result) in items.iter().zip(results) {
        apply_record(report, cp, t, id, result)?;
    }
    Ok(())
}
type PreparedInput = (Config, Manifest, Vec<(String, Record)>);
pub fn preflight(cli: &Cli) -> Result<PreparedInput> {
    require(
        !cli.run_id.is_empty()
            && cli.run_id.len() <= 80
            && cli
                .run_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "run_label_invalid",
    )?;
    require(!cli.phase.is_account(), "account_phase_uses_users_runner")?;
    require(
        cli.users_file.is_none() && cli.batch_size.is_none(),
        "users_arguments_forbidden",
    )?;
    require(
        !cli.source_ids.is_empty() && !cli.types.is_empty(),
        "selection_required",
    )?;
    require(
        (1..=100).contains(&cli.wp_per_page),
        "wordpress_page_size_invalid",
    )?;
    require(
        (1..=MAX_MEDIA_CONCURRENCY).contains(&media_concurrency(cli)),
        "media_concurrency_invalid",
    )?;
    require(
        (1..=MAX_RECORD_BATCH).contains(&record_batch_size(cli)),
        "record_batch_size_invalid",
    )?;
    require(
        !cli.skip_images || (cli.phase == Phase::Inventory || cli.dry_run),
        "skip_images_cannot_write",
    )?;
    require(
        !cli.dry_run || matches!(cli.phase, Phase::Import | Phase::Inventory),
        "dry_run_phase_invalid",
    )?;
    require(
        !cli.resume || (!cli.dry_run && cli.phase != Phase::Inventory),
        "resume_phase_invalid",
    )?;
    require(
        cli.phase == Phase::Import || cli.phase == Phase::Inventory || cli.resume,
        "phase_requires_resume",
    )?;
    require(
        cli.dry_run || cli.phase == Phase::Inventory || cli.checkpoint.is_some(),
        "checkpoint_required",
    )?;
    let config = Config::load(&cli.config)?;
    let manifest = Manifest::load(
        cli.manifest
            .as_ref()
            .ok_or_else(|| Error::new("manifest_required"))?,
    )?;
    require(
        config.local_media_base_url.is_none()
            || (config.local && manifest.raw["sourceAuthority"] == "synthetic.local"),
        "local_media_forbidden",
    )?;
    if let Some(base) = &config.local_media_base_url {
        require(
            base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none()
                && matches!(base.scheme(), "http" | "https"),
            "local_media_base_invalid",
        )?;
    }
    let boundary = Boundary::source(&config, &manifest)?;
    Boundary::target(&config)?;
    if let Some(wp) = &config.wordpress {
        require(
            wp.path().ends_with('/') && wp.query().is_none(),
            "wordpress_endpoint_invalid",
        )?;
        boundary.validate_url(wp)?;
    }
    let selected = manifest.selected(&cli.types, &cli.source_ids)?;
    for (typ, row) in &selected {
        boundary.validate_url(&row.source_url)?;
        record_body(typ, row, &manifest)?;
        if let Some(wp) = &row.wordpress {
            require(
                config.wordpress.is_some() && row.source_id.parse::<u64>().is_ok_and(|n| n > 0),
                "wordpress_config_or_id_invalid",
            )?;
            require(
                crate::canonical::hash_valid(wp["checksum"].as_str().unwrap_or("")),
                "wordpress_snapshot_checksum_missing",
            )?;
            if typ == "article" {
                let status = wp["status"].as_str().unwrap_or("");
                require(
                    status == "publish"
                        || (config.allow_private_posts
                            && config.wordpress_authorization_env.is_some()
                            && matches!(status, "draft" | "private" | "future" | "pending")),
                    "wordpress_status_forbidden",
                )?;
                require(
                    wp["primaryCategoryId"].as_u64().is_some()
                        && wp["modifiedGmt"].as_str().is_some(),
                    "wordpress_approval_missing",
                )?;
            } else {
                require(
                    typ == "comment"
                        && wp["postId"]
                            .as_str()
                            .is_some_and(|s| s.parse::<u64>().is_ok_and(|n| n > 0)),
                    "wordpress_record_mode_invalid",
                )?;
            }
        }
    }
    for media in manifest.media.values() {
        boundary.validate_url(&media.url)?;
    }
    if cli.phase != Phase::Inventory && !cli.dry_run {
        let token = std::env::var(&config.target_token_env)
            .map_err(|_| Error::new("target_token_missing"))?;
        require(
            token.len() >= 16 && !token.chars().any(char::is_whitespace),
            "target_token_invalid",
        )?;
    }
    Ok((config, manifest, selected))
}
fn identity(cli: &Cli, manifest: &Manifest, config: &Config) -> Value {
    let mut types = cli.types.clone();
    types.sort();
    let mut ids = cli.source_ids.clone();
    ids.sort();
    json!({"version":1,"label":cli.run_id,"manifestHash":manifest.raw["manifestHash"],"targetFingerprint":sha256(config.target.as_str().as_bytes()),"types":types,"sourceIds":ids})
}
pub async fn execute(cli: &Cli) -> Result<Report> {
    let (config, manifest, mut selected) = preflight(cli)?;
    let mut report = Report {
        selected: selected.len(),
        dry_run: cli.dry_run || cli.phase == Phase::Inventory,
        ..Default::default()
    };
    let source = Http {
        boundary: Boundary::source(&config, &manifest)?,
        attempts: config.max_attempts,
        timeout: Duration::from_secs(config.timeout_seconds),
    };
    let frames: Vec<String> = serde_json::from_value(
        manifest
            .raw
            .get("frameSources")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(|_| Error::new("iframe_sources_invalid"))?;
    require(frames.len() <= 50, "iframe_sources_limit")?;
    let target = Http {
        boundary: Boundary::target(&config)?,
        attempts: config.max_attempts,
        timeout: Duration::from_secs(config.timeout_seconds),
    };
    let wp = Wordpress::new(&config, source.clone(), cli.wp_per_page)?;
    // Resolve the destination before opening a checkpoint or writing; source hops
    // are independently resolved and checked by the fetching client.
    target.boundary.addresses(&config.target).await?;
    if matches!(cli.phase, Phase::Import | Phase::Inventory) {
        let mut groups = BTreeMap::<String, Vec<String>>::new();
        for (typ, row) in &selected {
            if typ == "article" {
                if let Some(w) = &row.wordpress {
                    groups
                        .entry(
                            w["status"]
                                .as_str()
                                .ok_or_else(|| Error::new("wordpress_status_missing"))?
                                .into(),
                        )
                        .or_default()
                        .push(row.source_id.clone());
                }
            }
        }
        for (status, ids) in groups {
            let posts = wp
                .as_ref()
                .ok_or_else(|| Error::new("wordpress_config_required"))?
                .posts(&ids, &status)
                .await?;
            for (typ, row) in &mut selected {
                if typ == "article" && ids.contains(&row.source_id) {
                    apply_wordpress(
                        row,
                        &posts[&row.source_id],
                        &manifest.raw["wordpressCategoryMap"],
                    )?;
                    source.boundary.validate_url(&row.source_url)?;
                }
            }
        }
        // Approved comment exports are refreshed from ALL WordPress comment pages, preserving dates.
        if selected
            .iter()
            .any(|(t, r)| t == "comment" && r.wordpress.is_some())
        {
            let wp = wp
                .as_ref()
                .ok_or_else(|| Error::new("wordpress_config_required"))?;
            let posts = selected
                .iter()
                .filter(|(t, r)| t == "comment" && r.wordpress.is_some())
                .map(|(_, r)| {
                    r.wordpress
                        .as_ref()
                        .and_then(|w| w["postId"].as_str())
                        .map(str::to_owned)
                        .ok_or_else(|| Error::new("wordpress_comment_post_missing"))
                })
                .collect::<Result<BTreeSet<_>>>()?;
            for post in posts {
                let comments = wp.comments(&post).await?;
                for (t, r) in &mut selected {
                    if t != "comment"
                        || r.wordpress.as_ref().and_then(|v| v["postId"].as_str()) != Some(&post)
                    {
                        continue;
                    }
                    let row = comments
                        .iter()
                        .find(|v| {
                            v["id"]
                                .as_u64()
                                .is_some_and(|n| n.to_string() == r.source_id)
                        })
                        .ok_or_else(|| Error::new("wordpress_comment_missing"))?;
                    require(
                        r.wordpress.as_ref().is_some_and(|v| {
                            v["checksum"]
                                == sha256(
                                    crate::canonical::stringify(row, false)
                                        .unwrap_or_default()
                                        .as_bytes(),
                                )
                        }),
                        "wordpress_comment_snapshot_changed",
                    )?;
                    r.data["comment"] = json!(
                        crate::excerpt_normalizer::ExcerptNormalizer::normalize_comment(
                            row["content"]["rendered"]
                                .as_str()
                                .ok_or_else(|| Error::new("wordpress_comment_invalid"))?
                        )
                    );
                    r.data["commentedAt"] = json!(crate::source::gmt(
                        row["date_gmt"]
                            .as_str()
                            .ok_or_else(|| Error::new("wordpress_date_missing"))?
                    )?);
                }
            }
        }
    }
    let mut media_references = BTreeSet::new();
    fn collect(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::String(s) => {
                out.insert(s.clone());
            }
            Value::Array(a) => {
                for item in a {
                    collect(item, out)
                }
            }
            Value::Object(o) => {
                for item in o.values() {
                    collect(item, out)
                }
            }
            _ => (),
        }
    }
    for (typ, row) in &selected {
        collect(&row.data, &mut media_references);
        for field in html_fields(typ) {
            if let Some(html) = row.data[field].as_str() {
                let doc = scraper::Html::parse_fragment(html);
                for node in doc.tree.nodes() {
                    if let scraper::Node::Element(el) = node.value() {
                        for (k, v) in &el.attrs {
                            if ["src", "href", "data-src", "data-lazy-src", "poster"]
                                .contains(&k.local.as_ref())
                            {
                                media_references.insert(v.to_string());
                            } else if k.local.as_ref() == "srcset" {
                                for part in v.split(',') {
                                    if let Some(url) = part.split_whitespace().next() {
                                        media_references.insert(url.into());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let needed = manifest
        .media
        .values()
        .filter(|m| {
            if media_references.contains(m.definition["sourceKey"].as_str().unwrap_or(""))
                || media_references.contains(m.url.as_str())
                || m.aliases.iter().any(|s| media_references.contains(s))
            {
                return true;
            }
            m.definition["requiredFor"]
                .as_object()
                .map(|scope| {
                    scope.iter().any(|(t, ids)| {
                        selected.iter().any(|(typ, r)| {
                            typ == t
                                && ids
                                    .as_array()
                                    .is_some_and(|ids| ids.contains(&json!(r.source_id)))
                        })
                    })
                })
                .unwrap_or(false)
        })
        .cloned()
        .collect::<Vec<_>>();
    report.media_selected = needed.len();
    if report.dry_run {
        let mut aliases = BTreeMap::new();
        for m in &needed {
            for alias in std::iter::once(m.url.as_str().to_owned()).chain(m.aliases.clone()) {
                aliases.insert(alias, m.url.as_str().to_owned());
            }
            if !cli.skip_images {
                fetch_media(&source, m).await?;
            }
        }
        for (typ, r) in &selected {
            for field in html_fields(typ) {
                if let Some(html) = r.data[field].as_str() {
                    rewrite_html_with_frames(html, &aliases, &frames)?;
                }
            }
            record_body(typ, r, &manifest)?;
        }
        // Inventory is a read-only count, never a created/skipped mapping or synthetic ID.
        return Ok(report);
    }
    let cms = Cms {
        local_media_base: config.local_media_base_url.clone(),
        http: target,
        origin: config.target.clone(),
        authorization: format!(
            "Bearer {}",
            std::env::var(&config.target_token_env)
                .map_err(|_| Error::new("target_token_missing"))?
        ),
    };
    let ident = identity(cli, &manifest, &config);
    let mut cp = Checkpoint::open(
        cli.checkpoint
            .as_ref()
            .ok_or_else(|| Error::new("checkpoint_required"))?,
        &ident,
        cli.resume,
    )?;
    let run = if let Some(run) = &cp.run_id {
        run.clone()
    } else {
        require(cli.phase == Phase::Import, "phase_requires_existing_run")?;
        let run = cms
            .create_run(&manifest, &cli.types, &cli.source_ids)
            .await?;
        cp.set_run(&ident, &run)?;
        run
    };
    report.run_id = Some(run.clone());
    if cli.phase == Phase::Rollback {
        let mut cursor: Option<String> = None;
        let mut previous = None;
        for _ in 0..251 {
            let result = cms.rollback(&run, cursor.as_deref()).await?;
            if result["remaining"] == 0 {
                require(
                    result["status"] == "rolled-back"
                        && result["conflicts"].as_array().is_some_and(Vec::is_empty),
                    "rollback_conflict",
                )?;
                return Ok(report);
            }
            let remaining = result["remaining"]
                .as_u64()
                .ok_or_else(|| Error::new("rollback_response_invalid"))?;
            require(previous.is_none_or(|n| remaining < n), "rollback_stalled")?;
            previous = Some(remaining);
            cursor = Some(
                result["nextCursor"]
                    .as_str()
                    .ok_or_else(|| Error::new("rollback_cursor_missing"))?
                    .to_owned(),
            );
        }
        return Err(Error::new("rollback_limit"));
    }
    if cli.phase == Phase::Import {
        let mut aliases = BTreeMap::new();
        // One paginated lookup per type, never O(N) lookups for every resumed record.
        if cli.resume {
            for typ in &cli.types {
                cms.lookup_all(&run, typ).await?;
            }
        }
        let source_system = manifest.raw["sourceSystem"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let uploads = stream::iter(needed.into_iter().map(|media| {
            let source = source.clone();
            let cms = cms.clone();
            let run = run.clone();
            let source_system = source_system.clone();
            async move {
                let result = match fetch_media(&source, &media).await {
                    Ok(bytes) => cms.media(&run, &media, bytes, &source_system).await,
                    Err(error) => Err(error),
                };
                (media, result)
            }
        }))
        .buffer_unordered(media_concurrency(cli));
        futures::pin_mut!(uploads);
        while let Some((media, result)) = uploads.next().await {
            match result {
                Ok(result) => {
                    if result["operation"] == "created" {
                        report.media_created += 1;
                    } else {
                        report.media_reused += 1;
                    }
                    let ingress = result["ingressUrl"]
                        .as_str()
                        .ok_or_else(|| Error::new("cms_media_ingress_missing"))?
                        .to_owned();
                    for alias in
                        std::iter::once(media.url.as_str().to_owned()).chain(media.aliases.clone())
                    {
                        require(
                            aliases.get(&alias).is_none_or(|old| old == &ingress),
                            "media_alias_collision",
                        )?;
                        aliases.insert(alias, ingress.clone());
                    }
                    cp.record(&result)?;
                }
                Err(error) => {
                    report.media_failed += 1;
                    report.failure("media", &media.id, &error);
                }
            }
        }
        // Records keep their selection order. Consecutive same-type non-article records are sent
        // in batches of <= --record-batch-size and <= RECORD_BATCH_BYTES of JSON.
        let batch_size = record_batch_size(cli);
        let mut batching = batch_size > 1;
        let mut pending: Vec<PendingRecord> = Vec::new();
        let mut pending_bytes = 0;
        for (typ, mut record) in selected {
            let body: Result<Value> = (|| {
                for field in html_fields(&typ) {
                    if let Some(html) = record.data[field].as_str() {
                        record.data[field] =
                            json!(rewrite_html_with_frames(html, &aliases, &frames)?);
                    }
                }
                record_body(&typ, &record, &manifest)
            })();
            let body = match body {
                Ok(body) => body,
                Err(e) => {
                    report.failed += 1;
                    report.failure(&typ, &record.source_id, &e);
                    continue;
                }
            };
            let size = serde_json::to_vec(&body).map_or(usize::MAX, |b| b.len());
            let batchable = batching && batchable_type(&typ) && size < RECORD_BATCH_BYTES;
            if !pending.is_empty()
                && (!batchable
                    || pending[0].0 != typ
                    || pending.len() >= batch_size
                    || pending_bytes + size > RECORD_BATCH_BYTES)
            {
                flush_records(
                    &cms,
                    &run,
                    &mut pending,
                    &mut batching,
                    &mut report,
                    &mut cp,
                )
                .await?;
                pending_bytes = 0;
            }
            // Resume never trusts a local mapping alone; native upsert verifies current storage.
            if batchable && batching {
                pending_bytes += size;
                pending.push((typ, record.source_id, body));
            } else {
                pending.push((typ, record.source_id, body));
                let mut single = false;
                flush_records(&cms, &run, &mut pending, &mut single, &mut report, &mut cp).await?;
            }
        }
        flush_records(
            &cms,
            &run,
            &mut pending,
            &mut batching,
            &mut report,
            &mut cp,
        )
        .await?;
        cp.flush()?;
    }
    if !report.passed() {
        return Ok(report);
    }
    let summary = cms.run(&run).await?;
    let r = &summary["reconciliation"];
    require(
        r["selected"].as_u64() == Some(report.selected as u64)
            && r["missing"] == 0
            && r["failed"] == 0
            && r["conflicts"] == 0
            && r["mediaMissing"] == 0,
        "reconciliation_incomplete",
    )?;
    let count = |key: &str| {
        r[key]
            .as_u64()
            .ok_or_else(|| Error::new("reconciliation_invalid"))
    };
    require(
        count("created")? + count("updated")? + count("reused")? == report.selected as u64,
        "reconciliation_count_mismatch",
    )?;
    if matches!(
        cli.phase,
        Phase::Publish | Phase::Unpublish | Phase::ReleaseComments
    ) {
        let action = match cli.phase {
            Phase::Publish => "publish",
            Phase::Unpublish => "unpublish",
            _ => "releaseComments",
        };
        require(
            manifest.raw["publication"]["approval"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "publication_unapproved",
        )?;
        let mut keys = Vec::new();
        for (typ, rows) in &manifest.records {
            if !cli.types.contains(typ) {
                continue;
            }
            for row in rows {
                if !cli.source_ids.contains(&row.source_id) {
                    continue;
                }
                let key = source_key(
                    manifest.raw["sourceSystem"].as_str().unwrap_or(""),
                    &row.source_id,
                )?;
                require(
                    manifest.raw["publication"][action][typ]
                        .as_array()
                        .is_some_and(|approved| approved.contains(&json!(key))),
                    "publication_selection_unapproved",
                )?;
                keys.push(key);
            }
        }
        keys.sort();
        keys.dedup();
        for chunk in keys.chunks(100) {
            cms.transition(&run, action, chunk).await?;
        }
    }
    Ok(report)
}
fn html_fields(typ: &str) -> Vec<&'static str> {
    match typ {
        "article" => vec!["body"],
        "studio" => vec!["description"],
        _ => Vec::new(),
    }
}
