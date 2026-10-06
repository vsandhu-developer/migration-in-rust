use crate::{
    canonical::{checksum, parse_key, source_key},
    error::{require, Error, Result},
    http::{json_response, Body, Http},
    manifest::{Manifest, Media, Record},
};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use url::Url;

#[derive(Clone)]
pub struct Cms {
    pub http: Http,
    pub origin: Url,
    pub authorization: String,
    pub local_media_base: Option<Url>,
}
impl Cms {
    fn url(&self, path: &str) -> Result<Url> {
        require(
            path.starts_with("/api/ds-migration/") && !path.contains(".."),
            "cms_path_invalid",
        )?;
        self.origin
            .join(path)
            .map_err(|_| Error::new("cms_path_invalid"))
    }
    async fn request(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let v = self
            .http
            .json(method, self.url(path)?, Some(&self.authorization), body)
            .await?;
        require(
            v["meta"]["contractVersion"] == "v1"
                && v["meta"]["accessClass"] == "importer"
                && v.get("data").is_some(),
            "cms_contract_mismatch",
        )?;
        Ok(v)
    }
    pub async fn create_run(
        &self,
        manifest: &Manifest,
        types: &[String],
        ids: &[String],
    ) -> Result<String> {
        let b = json!({"manifestHash":manifest.raw["manifestHash"],"manifestSchemaVersion":"v1","sourceSystem":manifest.raw["sourceSystem"],"sourceAuthority":manifest.raw["sourceAuthority"],"sourceIds":ids,"allowedTypes":types});
        let v = self
            .request(Method::POST, "/api/ds-migration/runs", Some(&b))
            .await?;
        let run = v["data"]["runId"]
            .as_str()
            .ok_or_else(|| Error::new("cms_run_invalid"))?;
        validate_run(run)?;
        Ok(run.into())
    }
    pub async fn run(&self, run: &str) -> Result<Value> {
        validate_run(run)?;
        Ok(self
            .request(
                Method::GET,
                &format!("/api/ds-migration/runs/{run}?page=1&pageSize=100"),
                None,
            )
            .await?["data"]
            .clone())
    }
    pub async fn lookup_all(&self, run: &str, typ: &str) -> Result<BTreeMap<String, String>> {
        validate_run(run)?;
        require(crate::manifest::TYPES.contains(&typ), "cms_type_invalid")?;
        let mut mappings = BTreeMap::new();
        for page in 1..=100 {
            let v=self.request(Method::GET,&format!("/api/ds-migration/runs/{run}/records/{typ}?page={page}&pageSize=100&sort=sourceKey:asc"),None).await?;
            for row in v["data"]
                .as_array()
                .ok_or_else(|| Error::new("cms_lookup_invalid"))?
            {
                let key = row["sourceKey"]
                    .as_str()
                    .ok_or_else(|| Error::new("cms_lookup_invalid"))?;
                parse_key(key)?;
                let id = row["targetDocumentId"]
                    .as_str()
                    .ok_or_else(|| Error::new("cms_document_id_missing"))?;
                require(
                    !id.is_empty() && mappings.insert(key.into(), id.into()).is_none(),
                    "cms_lookup_duplicate",
                )?;
            }
            if page
                >= v["meta"]["pagination"]["pageCount"]
                    .as_u64()
                    .ok_or_else(|| Error::new("cms_pagination_missing"))?
            {
                return Ok(mappings);
            }
        }
        Err(Error::new("cms_pagination_limit"))
    }
    pub async fn lookup(&self, run: &str, typ: &str, key: &str) -> Result<Option<String>> {
        validate_run(run)?;
        require(crate::manifest::TYPES.contains(&typ), "cms_type_invalid")?;
        for page in 1..=100 {
            let v=self.request(Method::GET,&format!("/api/ds-migration/runs/{run}/records/{typ}?page={page}&pageSize=100&sort=sourceKey:asc"),None).await?;
            let rows = v["data"]
                .as_array()
                .ok_or_else(|| Error::new("cms_lookup_invalid"))?;
            for row in rows {
                if row["sourceKey"] == key {
                    return Ok(Some(
                        row["targetDocumentId"]
                            .as_str()
                            .ok_or_else(|| Error::new("cms_document_id_missing"))?
                            .into(),
                    ));
                }
            }
            if page
                >= v["meta"]["pagination"]["pageCount"]
                    .as_u64()
                    .ok_or_else(|| Error::new("cms_pagination_missing"))?
            {
                return Ok(None);
            }
        }
        Err(Error::new("cms_pagination_limit"))
    }
    pub async fn record(&self, run: &str, typ: &str, body: &Value) -> Result<Value> {
        validate_run(run)?;
        require(crate::manifest::TYPES.contains(&typ), "cms_type_invalid")?;
        let mut client = self.clone();
        client.http.attempts = 1;
        for attempt in 0..self.http.attempts {
            // Recover an uncertain success by looking up its stable identity before retrying.
            // The following idempotent upsert still verifies checksum and creates this run's journal.
            if attempt > 0 {
                client
                    .lookup(
                        run,
                        typ,
                        body["source"]["sourceKey"]
                            .as_str()
                            .ok_or_else(|| Error::new("source_key_invalid"))?,
                    )
                    .await?;
            }
            match client
                .request(
                    Method::POST,
                    &format!("/api/ds-migration/runs/{run}/records/{typ}"),
                    Some(body),
                )
                .await
            {
                Ok(v) => return checked_record(run, typ, body, &v["data"]),
                Err(e) if attempt + 1 < self.http.attempts && retryable(&e) => {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        e.retry_delay_ms.unwrap_or(200 * (1 << attempt)),
                    ))
                    .await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::new("retry_exhausted"))
    }
    /// Upserts 1..=MAX_RECORD_BATCH records of one type in one request. The CMS runs each item
    /// through the single-record upsert and answers per item, index-aligned. Same retry rules as
    /// `record`: an uncertain response is reconciled by a source-key lookup of the type before the
    /// idempotent retry (items that were committed come back as `reused`). A 403/404/405 means the
    /// route or its token scope is not available: `record_batch_unsupported`, callers fall back to
    /// `record` for every item (nothing was written: the route policy runs before any item).
    pub async fn record_batch(
        &self,
        run: &str,
        typ: &str,
        bodies: &[Value],
    ) -> Result<Vec<Result<Value>>> {
        validate_run(run)?;
        require(crate::manifest::TYPES.contains(&typ), "cms_type_invalid")?;
        require(
            !bodies.is_empty() && bodies.len() <= MAX_RECORD_BATCH,
            "record_batch_invalid",
        )?;
        let payload = json!({ "data": bodies });
        let mut client = self.clone();
        client.http.attempts = 1;
        for attempt in 0..self.http.attempts {
            if attempt > 0 {
                client.lookup_all(run, typ).await?;
            }
            match client
                .request(
                    Method::POST,
                    &format!("/api/ds-migration/runs/{run}/records/{typ}/batch"),
                    Some(&payload),
                )
                .await
            {
                Ok(v) => return checked_batch(run, typ, bodies, &v["data"]),
                Err(e) if e.status.is_some_and(|s| (403..=405).contains(&s)) => {
                    return Err(Error {
                        code: "record_batch_unsupported",
                        ..e
                    })
                }
                Err(e) if attempt + 1 < self.http.attempts && retryable(&e) => {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        e.retry_delay_ms.unwrap_or(200 * (1 << attempt)),
                    ))
                    .await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::new("retry_exhausted"))
    }
    pub async fn media(
        &self,
        run: &str,
        media: &Media,
        bytes: Vec<u8>,
        system: &str,
    ) -> Result<Value> {
        validate_run(run)?;
        let source = json!({"sourceSystem":system,"sourceId":media.id,"sourceKey":source_key(system,&media.id)?,"sourceUrl":media.url,"sourceChecksum":media.definition["checksum"]});
        let manifest = json!({"source":source,"accessLevel":media.definition["accessLevel"],"mimeType":media.definition["mimeType"],"size":bytes.len(),"checksum":media.definition["checksum"],"transformVersion":"v1"});
        let response = self
            .http
            .send(
                Method::POST,
                self.url(&format!("/api/ds-migration/runs/{run}/media"))?,
                Some(&self.authorization),
                Body::Media {
                    manifest: manifest.to_string(),
                    bytes,
                    mime: media.definition["mimeType"].as_str().unwrap_or("").into(),
                },
                2 * 1024 * 1024,
            )
            .await?;
        let v = json_response(response)?;
        require(
            v["meta"]["accessClass"] == "importer"
                && v["meta"]["contractVersion"] == "v1"
                && v["data"]["sourceKey"] == source["sourceKey"]
                && v["data"]["runId"] == run
                && matches!(v["data"]["operation"].as_str(), Some("created" | "reused"))
                && v["data"]["targetDocumentId"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()),
            "cms_media_mismatch",
        )?;
        let ingress = v["data"]["ingressUrl"]
            .as_str()
            .ok_or_else(|| Error::new("cms_media_ingress_missing"))?;
        if media.definition["accessLevel"] == "explicit" {
            require(
                ingress
                    .strip_prefix("/api/ds-media/asset-")
                    .and_then(|s| s.strip_suffix("/original"))
                    .is_some_and(crate::canonical::hash_valid),
                "cms_media_ingress_invalid",
            )?;
        } else {
            let url = Url::parse(ingress).map_err(|_| Error::new("cms_media_ingress_invalid"))?;
            let canonical_path = url
                .path()
                .split("/ptp/public/daily-squirt/")
                .nth(1)
                .and_then(|s| s.strip_suffix("/original"))
                .is_some_and(crate::canonical::hash_valid);
            let local = self.local_media_base.as_ref().is_some_and(|base| {
                url.origin() == base.origin()
                    && url.path().starts_with(&format!(
                        "{}/ptp/public/daily-squirt/",
                        base.path().trim_end_matches('/')
                    ))
            });
            require(
                (url.scheme() == "https" || local)
                    && canonical_path
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "cms_media_ingress_invalid",
            )?;
        }
        Ok(v["data"].clone())
    }
    pub async fn transition(&self, run: &str, action: &str, keys: &[String]) -> Result<Value> {
        validate_run(run)?;
        require(
            matches!(action, "publish" | "unpublish" | "releaseComments") && keys.len() <= 100,
            "transition_invalid",
        )?;
        Ok(self
            .request(
                Method::POST,
                &format!("/api/ds-migration/runs/{run}/transitions"),
                Some(&json!({"action":action,"sourceKeys":keys})),
            )
            .await?["data"]
            .clone())
    }
    pub async fn rollback(&self, run: &str, cursor: Option<&str>) -> Result<Value> {
        validate_run(run)?;
        let mut b = json!({"limit":100});
        if let Some(c) = cursor {
            b["cursor"] = json!(c);
        };
        Ok(self
            .request(
                Method::POST,
                &format!("/api/ds-migration/runs/{run}/rollback"),
                Some(&b),
            )
            .await?["data"]
            .clone())
    }
}
/// Records per batch request (CMS `RECORD_BATCH_LIMIT`).
pub const MAX_RECORD_BATCH: usize = 100;
/// Serialized-body budget per batch request; the CMS JSON body limit is 1 MiB.
pub const RECORD_BATCH_BYTES: usize = 512 * 1024;
fn retryable(e: &Error) -> bool {
    e.status.is_some_and(|s| s == 429 || s >= 500)
        || matches!(e.code, "transport_failed" | "response_interrupted")
}
fn checked_record(run: &str, typ: &str, body: &Value, data: &Value) -> Result<Value> {
    require(
        data["sourceKey"] == body["source"]["sourceKey"]
            && data["type"] == typ
            && data["runId"] == run
            && data["targetDocumentId"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            && matches!(
                data["operation"].as_str(),
                Some("created" | "updated" | "reused")
            ),
        "cms_record_mismatch",
    )?;
    Ok(data.clone())
}
/// Per-item outcome of a batch response. A malformed, reordered or incomplete response fails the
/// whole batch (`cms_batch_mismatch`); a per-item rejection becomes the same `http_rejected` error
/// (with the item's 4xx status) that the single-record request would have produced.
fn checked_batch(
    run: &str,
    typ: &str,
    bodies: &[Value],
    data: &Value,
) -> Result<Vec<Result<Value>>> {
    let items = data
        .as_array()
        .filter(|items| items.len() == bodies.len())
        .ok_or_else(|| Error::new("cms_batch_mismatch"))?;
    let mut results = Vec::with_capacity(items.len());
    for (i, (item, body)) in items.iter().zip(bodies).enumerate() {
        require(
            item["index"].as_u64() == Some(i as u64)
                && item.as_object().is_some_and(|o| o.len() == 2),
            "cms_batch_mismatch",
        )?;
        if let Some(result) = item.get("result") {
            results.push(Ok(checked_record(run, typ, body, result)
                .map_err(|_| Error::new("cms_batch_mismatch"))?));
        } else {
            let status = item["error"]["status"]
                .as_u64()
                .filter(|s| (400..500).contains(s))
                .ok_or_else(|| Error::new("cms_batch_mismatch"))?;
            results.push(Err(Error::http(status as u16)));
        }
    }
    Ok(results)
}
pub fn validate_run(run: &str) -> Result<()> {
    require(
        run.starts_with("dsrun-")
            && run.len() == 54
            && run[6..].bytes().all(|b| b.is_ascii_hexdigit()),
        "cms_run_invalid",
    )
}
fn target_type(field: &str) -> Option<&'static str> {
    match field {
        "author" => Some("author"),
        "category" => Some("category"),
        "subCategory" => Some("subCategory"),
        "performers" => Some("performer"),
        "studios" => Some("studio"),
        "article" | "featuredArticles" => Some("article"),
        "user" => Some("user"),
        _ => None,
    }
}
pub fn record_body(typ: &str, record: &Record, manifest: &Manifest) -> Result<Value> {
    let (allowed, required): (&[&str], &[&str]) = match typ {
        "article" => (
            &[
                "title",
                "slug",
                "excerpt",
                "publishDate",
                "body",
                "coverImage",
                "author",
                "subCategory",
                "performers",
                "studios",
                "allowComments",
                "commentExpiryDays",
                "registrationCtas",
            ],
            &[
                "title",
                "slug",
                "publishDate",
                "body",
                "coverImage",
                "author",
                "subCategory",
            ],
        ),
        "author" => (&["name", "avatar"], &["name"]),
        "category" => (
            &["name", "accessLevel", "slug"],
            &["name", "accessLevel", "slug"],
        ),
        "subCategory" => (
            &["name", "accessLevel", "slug", "category"],
            &["name", "accessLevel", "slug", "category"],
        ),
        "performer" => (
            &["name", "slug", "image", "bio", "socialLinks"],
            &["name", "slug"],
        ),
        "studio" => (
            &["name", "slug", "logo", "url", "description"],
            &["name", "slug"],
        ),
        "comment" => (
            &["comment", "commentedAt", "user", "article"],
            &["comment", "commentedAt", "user", "article"],
        ),
        "popup" => (
            &["key", "title", "description", "label", "url"],
            &["key", "title", "label"],
        ),
        "adConfig" => (
            &["key", "page", "type", "responsiveAd", "railStack"],
            &["key", "page", "type"],
        ),
        "header" => (
            &["logo", "ctaLabel", "ctaButton", "navLinks", "socialLinks"],
            &["logo"],
        ),
        "footer" => (
            &[
                "logo",
                "primaryButton",
                "socialButton",
                "description",
                "newsletterBanner",
            ],
            &["logo"],
        ),
        "homepage" => (
            &[
                "featuredArticles",
                "latestArticleLimit",
                "discoverMoreLimit",
                "appBanner",
                "newsletterBanner",
            ],
            &["latestArticleLimit", "discoverMoreLimit"],
        ),
        _ => return Err(Error::new("record_type_invalid")),
    };
    let obj = record
        .data
        .as_object()
        .ok_or_else(|| Error::new("record_data_invalid"))?;
    require(
        obj.keys().all(|k| allowed.contains(&k.as_str()))
            && required
                .iter()
                .all(|k| obj.get(*k).is_some_and(|v| !v.is_null())),
        "record_fields_invalid",
    )?;
    let mut relations = BTreeMap::<String, Vec<String>>::new();
    let mut media = BTreeMap::<String, Value>::new();
    fn walk(
        v: &Value,
        field: &str,
        path: &str,
        m: &Manifest,
        relations: &mut BTreeMap<String, Vec<String>>,
        media: &mut BTreeMap<String, Value>,
        depth: usize,
    ) -> Result<()> {
        require(depth <= 10, "record_depth_invalid")?;
        if let Some(key) = v.get("sourceKey").and_then(Value::as_str) {
            require(
                v.as_object().is_some_and(|v| v.len() == 1),
                "relation_shape_invalid",
            )?;
            let (_, id) = parse_key(key)?;
            if let Some(typ) = target_type(field) {
                let allowed = if typ == "user" {
                    m.raw["comments"]["users"][key]
                        .as_u64()
                        .is_some_and(|n| n > 0)
                } else {
                    m.raw["types"][typ]
                        .as_array()
                        .is_some_and(|ids| ids.contains(&json!(id)))
                        && source_key(m.raw["sourceSystem"].as_str().unwrap_or(""), &id)? == key
                        || m.raw["dependencies"][typ]
                            .as_array()
                            .is_some_and(|keys| keys.contains(&json!(key)))
                };
                require(allowed, "relation_unapproved")?;
                relations.entry(path.into()).or_default().push(key.into());
            } else {
                require(
                    [
                        "publicImage",
                        "explicitImage",
                        "image",
                        "logo",
                        "avatar",
                        "icon",
                    ]
                    .contains(&field),
                    "relation_field_invalid",
                )?;
                let approved = &m.raw["media"][&id];
                require(approved["sourceKey"] == key, "media_reference_unapproved")?;
                media.insert(key.into(),json!({"sourceKey":key,"checksum":approved["checksum"],"transformVersion":"v1"}));
            }
            return Ok(());
        }
        if let Some(obj) = v.as_object() {
            for (k, v) in obj {
                require(
                    ![
                        "id",
                        "documentId",
                        "createdBy",
                        "updatedBy",
                        "publishedAt",
                        "website",
                        "moderationStatus",
                        "email",
                        "sourceSystem",
                        "sourceId",
                        "sourceUrl",
                        "sourceChecksum",
                        "migrationRunId",
                        "__proto__",
                        "prototype",
                        "constructor",
                    ]
                    .contains(&k.as_str()),
                    "server_managed_field_forbidden",
                )?;
                walk(v, k, &format!("{path}{k}"), m, relations, media, depth + 1)?;
            }
        } else if let Some(items) = v.as_array() {
            for (i, item) in items.iter().enumerate() {
                let next = if target_type(field).is_some() {
                    path.to_owned()
                } else {
                    format!("{path}{i}")
                };
                walk(item, field, &next, m, relations, media, depth + 1)?;
            }
        } else if target_type(field).is_some() && !v.is_null() {
            return Err(Error::new("relation_must_use_source_key"));
        }
        Ok(())
    }
    require(crate::manifest::TYPES.contains(&typ), "record_type_invalid")?;
    walk(
        &record.data,
        "",
        "",
        manifest,
        &mut relations,
        &mut media,
        0,
    )?;
    let sum = checksum(&record.data, relations, media.into_values().collect())?;
    let system = manifest.raw["sourceSystem"]
        .as_str()
        .ok_or_else(|| Error::new("source_system_invalid"))?;
    Ok(
        json!({"source":{"sourceSystem":system,"sourceId":record.source_id,"sourceKey":source_key(system,&record.source_id)?,"sourceUrl":record.source_url,"sourceChecksum":sum},"data":record.data}),
    )
}
