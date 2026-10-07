use crate::{
    canonical::{sha256, stringify},
    error::{require, Error, Result},
    excerpt_normalizer::ExcerptNormalizer,
    http::{json_response, Body, Http},
    manifest::{Config, Media, Record},
};
use reqwest::{header::CONTENT_TYPE, Method};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

pub struct Wordpress {
    pub http: Http,
    pub endpoint: Url,
    pub authorization: Option<String>,
    pub page_size: usize,
    pub allow_private: bool,
}
impl Wordpress {
    pub fn new(config: &Config, http: Http, page_size: usize) -> Result<Option<Self>> {
        require(
            (1..=100).contains(&page_size),
            "wordpress_page_size_invalid",
        )?;
        let Some(endpoint) = config.wordpress.clone() else {
            return Ok(None);
        };
        http.boundary.validate_url(&endpoint)?;
        let authorization = config
            .wordpress_authorization_env
            .as_ref()
            .map(|name| {
                std::env::var(name).map_err(|_| Error::new("wordpress_credentials_missing"))
            })
            .transpose()?;
        require(
            !config.allow_private_posts || authorization.is_some(),
            "wordpress_private_auth_required",
        )?;
        Ok(Some(Self {
            http,
            endpoint,
            authorization,
            page_size,
            allow_private: config.allow_private_posts,
        }))
    }
    /// Requests the complete exact selection using include + actual server pagination.
    pub async fn posts(&self, ids: &[String], status: &str) -> Result<BTreeMap<String, Value>> {
        require(
            !ids.is_empty()
                && ids.len() <= 1000
                && ids.iter().all(|id| id.parse::<u64>().is_ok_and(|n| n > 0)),
            "wordpress_ids_invalid",
        )?;
        require(
            status == "publish"
                || (self.allow_private
                    && self.authorization.is_some()
                    && matches!(status, "draft" | "private" | "future" | "pending")),
            "wordpress_status_forbidden",
        )?;
        let mut out = BTreeMap::new();
        for chunk in ids.chunks(100) {
            let mut page = 1;
            let mut expected_pages = None;
            loop {
                let mut url = self
                    .endpoint
                    .join("posts")
                    .map_err(|_| Error::new("wordpress_url_invalid"))?;
                url.query_pairs_mut()
                    .append_pair("include", &chunk.join(","))
                    .append_pair("per_page", &self.page_size.to_string())
                    .append_pair("page", &page.to_string())
                    .append_pair("status", status)
                    .append_pair("orderby", "id")
                    .append_pair("order", "asc");
                if self.authorization.is_some() {
                    url.query_pairs_mut().append_pair("context", "edit");
                }
                let response = self
                    .http
                    .send(
                        Method::GET,
                        url,
                        self.authorization.as_deref(),
                        Body::Empty,
                        8 * 1024 * 1024,
                    )
                    .await?;
                let pages = response
                    .headers
                    .get("x-wp-totalpages")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<usize>().ok())
                    .ok_or_else(|| Error::new("wordpress_pagination_missing"))?;
                require(
                    pages > 0 && pages <= 1000 && expected_pages.is_none_or(|p| p == pages),
                    "wordpress_pagination_changed",
                )?;
                expected_pages = Some(pages);
                let rows = json_response(response)?;
                let rows = rows
                    .as_array()
                    .ok_or_else(|| Error::new("wordpress_posts_invalid"))?;
                require(
                    !rows.is_empty() && rows.len() <= self.page_size,
                    "wordpress_pagination_invalid",
                )?;
                for row in rows {
                    let id = row["id"]
                        .as_u64()
                        .ok_or_else(|| Error::new("wordpress_post_invalid"))?
                        .to_string();
                    require(
                        chunk.contains(&id)
                            && row["status"] == status
                            && out.insert(id, row.clone()).is_none(),
                        "wordpress_selection_changed",
                    )?;
                }
                if page == pages {
                    break;
                }
                page += 1;
            }
        }
        // Cached include listings can omit recently published posts. Resolve only
        // the exact missing IDs through their canonical endpoints; identity/status
        // and the caller's snapshot checks remain mandatory.
        for id in ids
            .iter()
            .filter(|id| !out.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>()
        {
            let mut url = self
                .endpoint
                .join(&format!("posts/{id}"))
                .map_err(|_| Error::new("wordpress_url_invalid"))?;
            if self.authorization.is_some() {
                url.query_pairs_mut().append_pair("context", "edit");
            }
            let response = self
                .http
                .send(
                    Method::GET,
                    url,
                    self.authorization.as_deref(),
                    Body::Empty,
                    8 * 1024 * 1024,
                )
                .await?;
            let row = json_response(response)?;
            require(
                row["id"].as_u64().is_some_and(|n| n.to_string() == id) && row["status"] == status,
                "wordpress_selection_changed",
            )?;
            out.insert(id, row);
        }
        require(out.len() == ids.len(), "wordpress_selection_incomplete")?;
        Ok(out)
    }
    /// Explicit comment mode uses the comments endpoint, never embedded first-page replies.
    pub async fn comments(&self, post_id: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut page = 1;
        let mut expected = None;
        loop {
            let mut url = self
                .endpoint
                .join("comments")
                .map_err(|_| Error::new("wordpress_url_invalid"))?;
            url.query_pairs_mut()
                .append_pair("post", post_id)
                .append_pair("per_page", &self.page_size.to_string())
                .append_pair("page", &page.to_string())
                .append_pair("orderby", "id")
                .append_pair("order", "asc");
            let response = self
                .http
                .send(
                    Method::GET,
                    url,
                    self.authorization.as_deref(),
                    Body::Empty,
                    8 * 1024 * 1024,
                )
                .await?;
            let pages = response
                .headers
                .get("x-wp-totalpages")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or_else(|| Error::new("wordpress_pagination_missing"))?;
            require(
                pages <= 1000 && expected.is_none_or(|p| p == pages),
                "wordpress_pagination_changed",
            )?;
            expected = Some(pages);
            let rows = json_response(response)?;
            let rows = rows
                .as_array()
                .ok_or_else(|| Error::new("wordpress_comments_invalid"))?;
            require(
                rows.len() <= self.page_size && out.len() + rows.len() <= 10000,
                "wordpress_comments_limit",
            )?;
            out.extend(rows.iter().cloned());
            if page >= pages {
                break;
            }
            require(!rows.is_empty(), "wordpress_pagination_invalid")?;
            page += 1;
        }
        let mut ids = BTreeSet::new();
        for row in &out {
            require(
                ids.insert(
                    row["id"]
                        .as_u64()
                        .ok_or_else(|| Error::new("wordpress_comment_invalid"))?,
                ),
                "wordpress_comment_duplicate",
            )?;
        }
        Ok(out)
    }
}
fn rendered<'a>(post: &'a Value, field: &str) -> Result<&'a str> {
    post[field]["rendered"]
        .as_str()
        .ok_or_else(|| Error::new("wordpress_content_invalid"))
}
/// WordPress assigns request-scoped render artifacts: gallery instance classes
/// and, for `[video]`/`[audio]` shortcodes, an IE9 shim on the first player in
/// the request, `id="video-{post}-{n}"` and a `?_={n}` cache-buster on the
/// source URL. They change when the same post is fetched in a different
/// include set, so they are not editorial content and must not affect an
/// approved snapshot or the imported body. Mirrored exactly by
/// scripts/generate_wordpress_manifest.py.
pub fn stable_wordpress_html(value: &str) -> String {
    let value = stable_gallery_html(value);
    let value = strip_media_shims(&value);
    let value = stable_player_ids(&value);
    strip_cache_busters(&value)
}
fn strip_media_shims(value: &str) -> String {
    let mut out = value.to_owned();
    for tag in ["video", "audio"] {
        let shim = format!(
            "<!--[if lt IE 9]><script>document.createElement('{tag}');</script><![endif]-->"
        );
        while let Some(pos) = out.find(&shim) {
            let mut end = pos + shim.len();
            if out[end..].starts_with('\n') {
                end += 1;
            }
            out.replace_range(pos..end, "");
        }
    }
    out
}
fn leading_digits(s: &str) -> usize {
    s.bytes().take_while(u8::is_ascii_digit).count()
}
/// `id="video-123-4"` -> `id="video-123-instance"` (same for audio).
fn stable_player_ids(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    loop {
        let next = ["id=\"video-", "id=\"audio-"]
            .iter()
            .filter_map(|p| rest.find(p).map(|i| (i, p.len())))
            .min();
        let Some((pos, len)) = next else { break };
        out.push_str(&rest[..pos + len]);
        let after = &rest[pos + len..];
        let post = leading_digits(after);
        let tail = &after[post..];
        let n = if post > 0 && tail.starts_with('-') {
            leading_digits(&tail[1..])
        } else {
            0
        };
        if n > 0 && tail[1 + n..].starts_with('"') {
            out.push_str(&after[..post + 1]);
            out.push_str("instance");
            rest = &tail[1 + n..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out
}
/// Drops `?_=N` / `&_=N` / `&#038;_=N` / `&amp;_=N` directly before a quote.
fn strip_cache_busters(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    let bytes = value.as_bytes();
    'scan: while i < value.len() {
        for sep in ["?_=", "&_=", "&#038;_=", "&amp;_="] {
            if value[i..].starts_with(sep) {
                let digits = leading_digits(&value[i + sep.len()..]);
                let end = i + sep.len() + digits;
                if digits > 0 && matches!(bytes.get(end), Some(b'"' | b'\'')) {
                    i = end;
                    continue 'scan;
                }
            }
        }
        let ch = value[i..].chars().next().unwrap_or_default();
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    out
}
fn stable_gallery_html(value: &str) -> String {
    const PREFIX: &str = "wp-block-gallery-";
    const REPLACEMENT: &str = "wp-block-gallery-instance";
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(position) = remaining.find(PREFIX) {
        output.push_str(&remaining[..position]);
        let after = &remaining[position + PREFIX.len()..];
        let digit_bytes = after
            .as_bytes()
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let boundary = after
            .as_bytes()
            .get(digit_bytes)
            .is_none_or(|byte| byte.is_ascii_whitespace() || matches!(*byte, b'"' | b'\''));
        if digit_bytes > 0 && boundary {
            output.push_str(REPLACEMENT);
            remaining = &after[digit_bytes..];
        } else {
            output.push_str(PREFIX);
            remaining = after;
        }
    }
    output.push_str(remaining);
    output
}
pub fn gmt(value: &str) -> Result<String> {
    let raw = if value.ends_with('Z') {
        value.to_owned()
    } else {
        format!("{value}Z")
    };
    Ok(chrono::DateTime::parse_from_rfc3339(&raw)
        .map_err(|_| Error::new("wordpress_date_invalid"))?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}
/// Exact port of the old importer's `ArticleTransformer::sanitize_slug`:
/// characters outside `[A-Za-z0-9-_.~]` become `-`, runs collapse, edges are
/// trimmed, and an empty result falls back to `post-<wpId>`.
pub fn sanitize_slug(raw: &str, wp_id: u64) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;
    for ch in raw.chars() {
        let allowed =
            ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' || ch == '~';
        if allowed {
            out.push(ch);
            last_dash = ch == '-';
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        format!("post-{wp_id}")
    } else {
        trimmed.to_string()
    }
}
/// Maps old CTA blocks onto the DS `registration-cta` component
/// `{enabled, afterParagraph (min 1), label, url}`. The old field was
/// `position_after_paragraph = block index`; index 0 (CTA before any block)
/// cannot be expressed with `min: 1`, so it becomes 1.
pub fn registration_ctas(ctas: &[crate::models::ContentBlock]) -> Value {
    json!(ctas
        .iter()
        .map(
            |b| json!({"enabled":true,"afterParagraph":b.index.max(1),"label":b.text,"url":b.href})
        )
        .collect::<Vec<_>>())
}
pub fn apply_wordpress(record: &mut Record, post: &Value, category_map: &Value) -> Result<()> {
    let approved = record
        .wordpress
        .as_ref()
        .ok_or_else(|| Error::new("wordpress_approval_missing"))?;
    let primary = approved["primaryCategoryId"]
        .as_u64()
        .ok_or_else(|| Error::new("wordpress_primary_category_required"))?;
    require(
        post["categories"]
            .as_array()
            .is_some_and(|a| a.contains(&json!(primary))),
        "wordpress_primary_category_absent",
    )?;
    require(
        category_map[primary.to_string()] == record.data["subCategory"]["sourceKey"]
            && !category_map[primary.to_string()].is_null(),
        "wordpress_taxonomy_unmapped",
    )?;
    require(
        approved["status"] == post["status"] && approved["modifiedGmt"] == post["modified_gmt"],
        "wordpress_snapshot_changed",
    )?;
    let mut stable_content = post["content"].clone();
    let content = stable_content
        .as_object_mut()
        .ok_or_else(|| Error::new("wordpress_content_invalid"))?;
    content.insert(
        "rendered".into(),
        json!(stable_wordpress_html(rendered(post, "content")?)),
    );
    let snapshot = json!({"id":post["id"],"title":post["title"],"content":stable_content,"excerpt":post["excerpt"],"date_gmt":post["date_gmt"],"modified_gmt":post["modified_gmt"],"status":post["status"],"categories":post["categories"],"author":post["author"],"slug":post["slug"],"link":post["link"]});
    require(
        approved["checksum"] == sha256(stringify(&snapshot, false)?.as_bytes()),
        "wordpress_snapshot_checksum_mismatch",
    )?;
    require(
        post["author"].as_u64().is_some_and(|id| {
            record.data["author"]["sourceKey"]
                .as_str()
                .and_then(|key| crate::canonical::parse_key(key).ok())
                .is_some_and(|(_, s)| s == id.to_string())
        }),
        "wordpress_author_unmapped",
    )?;
    record.data["title"] = json!(ExcerptNormalizer::normalize_title(rendered(post, "title")?));
    record.data["excerpt"] = json!(ExcerptNormalizer::normalize(rendered(post, "excerpt")?));
    // Old ArticleTransformer parity: CTA buttons leave the body and become
    // structured registration CTAs (old `registration_cta_block`).
    let (body, ctas) = crate::content_parser::ContentParser::split_ctas(&stable_wordpress_html(
        rendered(post, "content")?,
    ));
    record.data["body"] = json!(body);
    record.data["registrationCtas"] = registration_ctas(&ctas);
    record.data["publishDate"] = json!(gmt(post["date_gmt"]
        .as_str()
        .ok_or_else(|| Error::new("wordpress_date_missing"))?)?);
    let id = post["id"]
        .as_u64()
        .ok_or_else(|| Error::new("wordpress_post_invalid"))?;
    record.data["slug"] = json!(sanitize_slug(
        post["slug"]
            .as_str()
            .ok_or_else(|| Error::new("wordpress_slug_missing"))?,
        id
    ));
    record.source_url = Url::parse(
        post["link"]
            .as_str()
            .ok_or_else(|| Error::new("wordpress_link_missing"))?,
    )
    .map_err(|_| Error::new("wordpress_link_invalid"))?;
    Ok(())
}
pub fn verify_media(bytes: &[u8], definition: &Value) -> Result<()> {
    require(
        definition["size"].as_u64() == Some(bytes.len() as u64),
        "media_size_mismatch",
    )?;
    require(
        definition["checksum"] == sha256(bytes),
        "media_checksum_mismatch",
    )?;
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.len() >= 16
        && &bytes[4..8] == b"ftyp"
        && bytes[8..]
            .chunks(4)
            .take(8)
            .any(|s| s == b"avif" || s == b"avis")
    {
        "image/avif"
    } else {
        return Err(Error::new("media_signature_invalid"));
    };
    require(definition["mimeType"] == mime, "media_mime_mismatch")
}
pub async fn fetch_media(http: &Http, media: &Media) -> Result<Vec<u8>> {
    let response = http
        .send(
            Method::GET,
            media.url.clone(),
            None,
            Body::Empty,
            20 * 1024 * 1024,
        )
        .await?;
    require(
        response
            .headers
            .get(CONTENT_TYPE)
            .and_then(|s| s.to_str().ok())
            .is_some_and(|s| {
                Some(s.split(';').next().unwrap_or("")) == media.definition["mimeType"].as_str()
            }),
        "media_mime_mismatch",
    )?;
    verify_media(&response.bytes, &media.definition)?;
    Ok(response.bytes)
}
/// DOM rewriting retains figures, captions, galleries, order and CTA markup.
/// Any unapproved inline media is an exception, never silently hotlinked.
pub fn rewrite_html(html: &str, mappings: &BTreeMap<String, String>) -> Result<String> {
    rewrite_html_with_frames(html, mappings, &[])
}
pub fn rewrite_html_with_frames(
    html: &str,
    mappings: &BTreeMap<String, String>,
    frames: &[String],
) -> Result<String> {
    let mut doc = scraper::Html::parse_fragment(html);
    let nodes = doc.tree.nodes().map(|n| n.id()).collect::<Vec<_>>();
    require(
        nodes.len() <= 50000 && html.len() <= 2 * 1024 * 1024,
        "html_limit",
    )?;
    for id in nodes {
        let Some(mut node) = doc.tree.get_mut(id) else {
            continue;
        };
        let scraper::Node::Element(el) = node.value() else {
            continue;
        };
        let iframe = el.name.local.as_ref() == "iframe";
        // Video is not rehosted: <video src>/<source src> pass through unchanged
        // (HTTPS only); the CMS enforces its own video-origin allowlist.
        let video = matches!(el.name.local.as_ref(), "video" | "source");
        for (key, value) in &mut el.attrs {
            let name = key.local.as_ref();
            if iframe && name == "src" {
                let url =
                    Url::parse(value.as_ref()).map_err(|_| Error::new("iframe_source_invalid"))?;
                require(
                    url.scheme() == "https"
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.fragment().is_none()
                        && frames.iter().any(|s| s == url.as_str()),
                    "iframe_source_unapproved",
                )?;
                *value = url.as_str().into();
                continue;
            }
            if video && name == "src" {
                let url =
                    Url::parse(value.as_ref()).map_err(|_| Error::new("video_source_invalid"))?;
                require(
                    url.scheme() == "https"
                        && url.username().is_empty()
                        && url.password().is_none(),
                    "video_source_not_https",
                )?;
                continue;
            }
            if name == "srcset" {
                let mut revised = Vec::new();
                for candidate in value.split(',') {
                    let parts = candidate.split_whitespace().collect::<Vec<_>>();
                    require(!parts.is_empty() && parts.len() <= 2, "html_srcset_invalid")?;
                    let url = mappings
                        .get(parts[0])
                        .ok_or_else(|| Error::new("required_inline_media_unmapped"))?;
                    revised.push(if parts.len() == 2 {
                        format!("{url} {}", parts[1])
                    } else {
                        url.clone()
                    });
                }
                *value = revised.join(", ").into();
            } else if matches!(name, "src" | "poster" | "data-src" | "data-lazy-src") {
                *value = mappings
                    .get(value.as_ref())
                    .ok_or_else(|| Error::new("required_inline_media_unmapped"))?
                    .as_str()
                    .into();
            } else if name == "href" {
                if let Some(url) = mappings.get(value.as_ref()) {
                    *value = url.as_str().into();
                }
            }
        }
    }
    Ok(doc.root_element().inner_html())
}
