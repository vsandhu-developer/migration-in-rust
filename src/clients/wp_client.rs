//! WordPress REST reader. Fetches posts with `_embed=wp:featuredmedia,replies`.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use tracing::info;

use crate::infrastructure::config::Config;
use crate::infrastructure::http::HttpClient;
use crate::models::{CoverImage, WpComment, WpPost};
use crate::services::excerpt_normalizer::ExcerptNormalizer;

// WP REST API rejects per_page above this with HTTP 400.
const WP_MAX_PER_PAGE: i32 = 100;

pub struct WpClient {
    cfg: Config,
    http: HttpClient,
}

impl WpClient {
    pub fn new(cfg: Config, http: HttpClient) -> Self {
        Self { cfg, http }
    }

    fn build_posts_url(&self, per_page: i32, page: i32, embed: bool) -> String {
        let mut base = self.cfg.wp_base_url.trim_end_matches('/').to_string();
        base.push_str(&format!("/wp-json/wp/v2/posts?per_page={}&page={}", per_page, page));
        if embed {
            base.push_str("&_embed=wp:featuredmedia,replies");
        }
        base
    }

    pub async fn get_posts(&self, per_page: i32, embed: bool) -> Result<Vec<WpPost>> {
        self.fetch_page(per_page.min(WP_MAX_PER_PAGE), 1, embed).await
    }

    /// Paginated fetch. Loops `/wp-json/wp/v2/posts?page=N&per_page=100` until
    /// `target` posts collected or WP returns fewer than `per_page` (last page).
    /// `target = 0` falls back to a single page.
    pub async fn get_posts_n(&self, target: usize, embed: bool) -> Result<Vec<WpPost>> {
        if target == 0 {
            return self.get_posts(WP_MAX_PER_PAGE, embed).await;
        }
        let per_page = WP_MAX_PER_PAGE;
        let mut out: Vec<WpPost> = Vec::with_capacity(target);
        let mut page: i32 = 1;
        loop {
            let batch = self.fetch_page(per_page, page, embed).await?;
            let got = batch.len();
            info!(page, got, accumulated = out.len() + got, target, "WP page fetched");
            out.extend(batch);
            if out.len() >= target {
                out.truncate(target);
                break;
            }
            if got < per_page as usize {
                break; // last page
            }
            page += 1;
        }
        Ok(out)
    }

    /// Fetch specific posts by WP id using `?include=id1,id2,...`.
    /// Chunks ids in groups of 100 (WP per_page cap) and concatenates results.
    pub async fn get_posts_by_ids(&self, ids: &[i64], embed: bool) -> Result<Vec<WpPost>> {
        if ids.is_empty() { return Ok(Vec::new()); }
        let mut out: Vec<WpPost> = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(WP_MAX_PER_PAGE as usize) {
            let include = chunk.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            let mut url = self.cfg.wp_base_url.trim_end_matches('/').to_string();
            url.push_str(&format!("/wp-json/wp/v2/posts?include={}&per_page={}", include, WP_MAX_PER_PAGE));
            if embed { url.push_str("&_embed=wp:featuredmedia,replies"); }
            let resp = self.http.get(&url, &[]).await?;
            if !resp.is_success() {
                return Err(anyhow!("WP get_posts_by_ids: HTTP {}: {}", resp.status_code, resp.body));
            }
            let v: Value = serde_json::from_str(&resp.body).context("WP get_posts_by_ids: parse JSON")?;
            let arr = v.as_array().ok_or_else(|| anyhow!("WP get_posts_by_ids: not an array"))?;
            info!(chunk_size = chunk.len(), returned = arr.len(), "WP include-batch fetched");
            for item in arr {
                if let Some(post) = Self::parse_post(item) { out.push(post); }
            }
        }
        Ok(out)
    }

    async fn fetch_page(&self, per_page: i32, page: i32, embed: bool) -> Result<Vec<WpPost>> {
        let url = self.build_posts_url(per_page, page, embed);
        let resp = self.http.get(&url, &[]).await?;
        if !resp.is_success() {
            return Err(anyhow!("WP getPosts: HTTP {}: {}", resp.status_code, resp.body));
        }
        let v: Value = serde_json::from_str(&resp.body).context("WP getPosts: parse JSON")?;
        let arr = v.as_array().ok_or_else(|| anyhow!("WP getPosts: not an array"))?;

        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            if let Some(post) = Self::parse_post(item) {
                out.push(post);
            }
        }
        Ok(out)
    }

    fn parse_post(item: &Value) -> Option<WpPost> {
        let id = item.get("id").and_then(|x| x.as_i64())?;
        let date = item.get("modified").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let slug = item.get("slug").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let title_raw = item.pointer("/title/rendered").and_then(|x| x.as_str()).unwrap_or("");
        let content_raw = item.pointer("/content/rendered").and_then(|x| x.as_str()).unwrap_or("");
        let excerpt_raw = item.pointer("/excerpt/rendered").and_then(|x| x.as_str()).unwrap_or("");

        let categories = item.get("categories")
            .and_then(|x| x.as_array())
            .and_then(|a| a.first())
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let author_id = item.get("author").and_then(|x| x.as_i64()).unwrap_or(0);
        let comments_enabled = item.get("comment_status").and_then(|x| x.as_str())
            .map(|s| s == "open").unwrap_or(false);
        let tag_ids = item.get("tags").and_then(|x| x.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect::<Vec<_>>())
            .unwrap_or_default();

        // cover image from _embedded["wp:featuredmedia"][0]
        let mut cover = CoverImage::default();
        if let Some(arr) = item.pointer("/_embedded/wp:featuredmedia").and_then(|x| x.as_array()) {
            if let Some(first) = arr.first() {
                if let Some(src) = first.get("source_url").and_then(|x| x.as_str()) {
                    cover.source_url = src.to_string();
                }
                if let Some(alt) = first.get("alt_text").and_then(|x| x.as_str()) {
                    cover.alt_text = alt.to_string();
                }
                if cover.alt_text.is_empty() {
                    if let Some(slug) = first.get("slug").and_then(|x| x.as_str()) {
                        cover.alt_text = slug.to_string();
                    }
                }
            }
        }

        // comments from _embedded.replies[0][]
        let mut comments = Vec::new();
        if let Some(replies) = item.pointer("/_embedded/replies").and_then(|x| x.as_array()) {
            if let Some(first) = replies.first() {
                if let Some(arr) = first.as_array() {
                    for c in arr {
                        let cid = c.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
                        let cdate = c.get("date").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let ccontent = c.pointer("/content/rendered").and_then(|x| x.as_str()).unwrap_or("");
                        let cavatar = c.pointer("/author_avatar_urls/96").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let cauthor = c.get("author").and_then(|x| x.as_i64()).unwrap_or(0);
                        comments.push(WpComment {
                            id: cid,
                            date: cdate,
                            content: ExcerptNormalizer::normalize_comment(ccontent),
                            avatar_url: cavatar,
                            author_id: cauthor,
                        });
                    }
                }
            }
        }

        Some(WpPost {
            id,
            date,
            slug,
            title: ExcerptNormalizer::normalize_title(title_raw),
            content: content_raw.to_string(),
            excerpt: ExcerptNormalizer::normalize(excerpt_raw),
            categories,
            cover_image: cover,
            comments,
            author_id,
            tag_ids,
            comments_enabled,
        })
    }
}
