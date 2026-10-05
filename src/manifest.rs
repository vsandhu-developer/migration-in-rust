use crate::{
    canonical::{hash_valid, manifest_hash, parse_key, sha256, source_key},
    error::{require, Error, Result},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use url::Url;

pub const TYPES: [&str; 12] = [
    "category",
    "subCategory",
    "author",
    "performer",
    "studio",
    "article",
    "comment",
    "header",
    "footer",
    "homepage",
    "popup",
    "adConfig",
];
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub target: Url,
    pub target_token_env: String,
    pub wordpress: Option<Url>,
    pub wordpress_authorization_env: Option<String>,
    #[serde(default)]
    pub allow_private_posts: bool,
    #[serde(default)]
    pub local: bool,
    pub local_source_origin: Option<Url>,
    pub local_target_origin: Option<Url>,
    pub local_source_address: Option<std::net::IpAddr>,
    pub local_target_address: Option<std::net::IpAddr>,
    pub local_media_base_url: Option<Url>,
    #[serde(default = "attempts")]
    pub max_attempts: usize,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
}
fn attempts() -> usize {
    3
}
fn timeout() -> u64 {
    20
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let c: Self = serde_json::from_slice(&read_file(path, 65536)?)
            .map_err(|_| Error::new("config_invalid"))?;
        require(
            (1..=5).contains(&c.max_attempts) && (1..=120).contains(&c.timeout_seconds),
            "config_limits_invalid",
        )?;
        require(
            c.target.path() == "/"
                && c.target.query().is_none()
                && c.target.fragment().is_none()
                && c.target.username().is_empty()
                && c.target.password().is_none(),
            "target_origin_invalid",
        )?;
        for e in [
            &c.target_token_env,
            c.wordpress_authorization_env
                .as_ref()
                .unwrap_or(&c.target_token_env),
        ] {
            require(
                !e.is_empty()
                    && e.bytes()
                        .all(|x| x.is_ascii_uppercase() || x.is_ascii_digit() || x == b'_'),
                "secret_env_invalid",
            )?;
        }
        Ok(c)
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Record {
    pub source_id: String,
    pub source_url: Url,
    pub data: Value,
    /// Frozen WordPress post metadata plus explicitly approved primary category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wordpress: Option<Value>,
}
#[derive(Clone)]
pub struct Media {
    pub id: String,
    pub url: Url,
    pub aliases: Vec<String>,
    pub definition: Value,
}
#[derive(Clone)]
pub struct Manifest {
    pub raw: Value,
    pub records: BTreeMap<String, Vec<Record>>,
    pub media: BTreeMap<String, Media>,
}
pub fn read_file(path: &Path, max: u64) -> Result<Vec<u8>> {
    let meta = fs::metadata(path).map_err(|_| Error::new("input_unreadable"))?;
    require(meta.is_file() && meta.len() <= max, "input_size_invalid")?;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|_| Error::new("input_unreadable"))?
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::new("input_unreadable"))?;
    require(bytes.len() as u64 <= max, "input_size_invalid")?;
    Ok(bytes)
}
pub fn external_path(path: &Path) -> Result<PathBuf> {
    // Canonicalize before reading: symlinks cannot smuggle tracked legacy fixtures.
    let actual = path
        .canonicalize()
        .map_err(|_| Error::new("input_unreadable"))?;
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .map_err(|_| Error::new("repository_unreadable"))?;
    let lower = actual.to_string_lossy().to_ascii_lowercase();
    require(
        !actual.starts_with(repo)
            && !lower.contains("daily-squirt-code")
            && !lower.contains("migration-in-rust")
            && !lower.contains("ds-categories.json"),
        "repository_fixture_forbidden",
    )?;
    Ok(actual)
}
fn metadata(v: &Value) -> Result<()> {
    require(
        v["schemaVersion"] == "v1" && v["repositoryFixture"] == false,
        "manifest_schema_invalid",
    )?;
    for k in [
        "sourceOwner",
        "sourceAuthority",
        "approvedAt",
        "sourceLocation",
    ] {
        require(
            v[k].as_str()
                .is_some_and(|s| !s.trim().is_empty() && s.len() <= 2048),
            "manifest_metadata_missing",
        )?;
    }
    require(
        chrono::DateTime::parse_from_rfc3339(v["approvedAt"].as_str().unwrap_or("")).is_ok(),
        "manifest_timestamp_invalid",
    )?;
    let location = v["sourceLocation"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    require(
        !location.contains("daily-squirt-code")
            && !location.contains("migration-in-rust")
            && !location.contains("ds-categories.json"),
        "repository_fixture_forbidden",
    )
}
pub fn text<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v[k].as_str()
        .ok_or_else(|| Error::new("manifest_field_invalid"))
}
impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let path = external_path(path)?;
        let raw: Value = serde_json::from_slice(&read_file(&path, 2 * 1024 * 1024)?)
            .map_err(|_| Error::new("manifest_json_invalid"))?;
        metadata(&raw)?;
        require(
            text(&raw, "manifestHash")? == manifest_hash(&raw)?,
            "manifest_hash_mismatch",
        )?;
        let system = text(&raw, "sourceSystem")?;
        source_key(system, "validation")?;
        let types = raw["types"]
            .as_object()
            .ok_or_else(|| Error::new("manifest_types_invalid"))?;
        require(
            !types.is_empty() && types.keys().all(|k| TYPES.contains(&k.as_str())),
            "manifest_type_forbidden",
        )?;
        let origins = raw["sourceOrigins"]
            .as_array()
            .ok_or_else(|| Error::new("manifest_origins_invalid"))?;
        require(
            !origins.is_empty() && origins.len() <= 20,
            "manifest_origins_invalid",
        )?;
        for o in origins {
            let u = Url::parse(o.as_str().unwrap_or(""))
                .map_err(|_| Error::new("manifest_origins_invalid"))?;
            require(
                u.origin().ascii_serialization() == o.as_str().unwrap_or("")
                    && u.username().is_empty()
                    && u.password().is_none(),
                "manifest_origins_invalid",
            )?;
        }
        if types.contains_key("comment") {
            require(
                matches!(
                    raw["comments"]["mode"].as_str(),
                    Some("existing-users" | "anonymized")
                ) && raw["comments"]["approval"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()),
                "comments_excluded",
            )?;
        }
        let files = raw["files"]
            .as_object()
            .ok_or_else(|| Error::new("manifest_files_missing"))?;
        let mut records = BTreeMap::new();
        let mut media = BTreeMap::new();
        for (typ, file) in files {
            require(
                TYPES.contains(&typ.as_str()) || typ == "media",
                "manifest_file_type_invalid",
            )?;
            let source = external_path(
                &path
                    .parent()
                    .ok_or_else(|| Error::new("manifest_path_invalid"))?
                    .join(text(file, "path")?),
            )?;
            let bytes = read_file(&source, 64 * 1024 * 1024)?;
            require(
                hash_valid(text(file, "sha256")?) && sha256(&bytes) == text(file, "sha256")?,
                "export_hash_mismatch",
            )?;
            let export: Value =
                serde_json::from_slice(&bytes).map_err(|_| Error::new("export_json_invalid"))?;
            metadata(&export)?;
            require(
                export["sourceAuthority"] == raw["sourceAuthority"],
                "export_authority_mismatch",
            )?;
            let rows = export["records"]
                .as_array()
                .ok_or_else(|| Error::new("export_records_invalid"))?;
            require(rows.len() <= 10000, "export_records_limit")?;
            if typ == "media" {
                for row in rows {
                    let id = text(row, "sourceId")?.to_owned();
                    let def = raw["media"][&id].clone();
                    require(
                        def.is_object()
                            && def["sourceKey"] == source_key(system, &id)?
                            && hash_valid(text(&def, "checksum")?),
                        "media_manifest_invalid",
                    )?;
                    if def["required"] != false {
                        let scope = def["requiredFor"]
                            .as_object()
                            .ok_or_else(|| Error::new("media_scope_required"))?;
                        require(!scope.is_empty(), "media_scope_required")?;
                        for (kind, ids) in scope {
                            let ids = ids
                                .as_array()
                                .ok_or_else(|| Error::new("media_scope_invalid"))?;
                            require(
                                !ids.is_empty()
                                    && types.get(kind).and_then(Value::as_array).is_some_and(
                                        |approved| ids.iter().all(|id| approved.contains(id)),
                                    ),
                                "media_scope_invalid",
                            )?;
                        }
                    }
                    require(
                        matches!(def["accessLevel"].as_str(), Some("public" | "explicit"))
                            && matches!(
                                def["mimeType"].as_str(),
                                Some("image/png" | "image/jpeg" | "image/webp" | "image/avif")
                            )
                            && def["transformVersion"] == "v1"
                            && def["size"]
                                .as_u64()
                                .is_some_and(|n| (1..=20 * 1024 * 1024).contains(&n)),
                        "media_manifest_invalid",
                    )?;
                    let url = Url::parse(text(row, "url")?)
                        .map_err(|_| Error::new("media_url_invalid"))?;
                    let aliases = serde_json::from_value(
                        row.get("aliases")
                            .cloned()
                            .unwrap_or_else(|| Value::Array(vec![])),
                    )
                    .map_err(|_| Error::new("media_aliases_invalid"))?;
                    require(
                        media
                            .insert(
                                id.clone(),
                                Media {
                                    id,
                                    url,
                                    aliases,
                                    definition: def,
                                },
                            )
                            .is_none(),
                        "media_duplicate",
                    )?;
                }
            } else {
                let rows: Vec<Record> = serde_json::from_value(Value::Array(rows.clone()))
                    .map_err(|_| Error::new("export_records_invalid"))?;
                let expected = types
                    .get(typ)
                    .and_then(Value::as_array)
                    .ok_or_else(|| Error::new("manifest_file_type_invalid"))?;
                let mut ids = BTreeSet::new();
                for row in &rows {
                    source_key(system, &row.source_id)?;
                    require(
                        ids.insert(row.source_id.clone()) && row.data.is_object(),
                        "export_duplicate_or_invalid",
                    )?;
                    require(
                        expected.contains(&Value::String(row.source_id.clone())),
                        "export_selection_mismatch",
                    )?;
                    if typ == "category" || typ == "subCategory" {
                        let approved = &raw["taxonomy"][typ][&row.source_id];
                        require(
                            matches!(approved["accessLevel"].as_str(), Some("public" | "gated"))
                                && row.data["accessLevel"] == approved["accessLevel"],
                            "taxonomy_access_invalid",
                        )?;
                        if typ == "subCategory" {
                            let parent = text(approved, "parentSourceKey")?;
                            let (_, pid) = parse_key(parent)?;
                            require(
                                row.data["category"]["sourceKey"] == parent,
                                "taxonomy_parent_mismatch",
                            )?;
                            require(
                                raw["taxonomy"]["category"][pid]["accessLevel"] != "gated"
                                    || approved["accessLevel"] == "gated",
                                "taxonomy_parent_gated",
                            )?;
                        }
                    }
                }
                require(ids.len() == expected.len(), "export_selection_mismatch")?;
                records.insert(typ.clone(), rows);
            }
        }
        require(
            types.keys().all(|k| records.contains_key(k)),
            "manifest_export_missing",
        )?;
        if let Some(expected) = raw["media"].as_object() {
            require(
                expected.keys().all(|k| media.contains_key(k)),
                "media_export_missing",
            )?;
        }
        Ok(Self {
            raw,
            records,
            media,
        })
    }
    pub fn selected(&self, types: &[String], ids: &[String]) -> Result<Vec<(String, Record)>> {
        require(
            !types.is_empty()
                && !ids.is_empty()
                && ids.len() <= 1000
                && types.iter().collect::<BTreeSet<_>>().len() == types.len()
                && ids.iter().collect::<BTreeSet<_>>().len() == ids.len(),
            "selection_invalid",
        )?;
        let mut out = Vec::new();
        let mut used = BTreeSet::new();
        for typ in TYPES {
            if !types.iter().any(|s| s == typ) {
                continue;
            }
            let rows = self
                .records
                .get(typ)
                .ok_or_else(|| Error::new("selection_type_unapproved"))?;
            let matches = rows
                .iter()
                .filter(|r| ids.contains(&r.source_id))
                .collect::<Vec<_>>();
            require(!matches.is_empty(), "selection_empty")?;
            for row in matches {
                used.insert(row.source_id.clone());
                out.push((typ.to_owned(), row.clone()));
            }
        }
        require(
            types.iter().all(|s| TYPES.contains(&s.as_str())) && used.len() == ids.len(),
            "selection_unapproved",
        )?;
        Ok(out)
    }
}
