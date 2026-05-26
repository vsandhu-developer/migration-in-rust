//! Full per-article pipeline:
//! parse → cover image → body images (strict) → rebuild → transform → POST → comments.
//!
//! Emits one structured JSONL event per phase via Logger (parity with the C++
//! implementation at ArticleMigrationOrchestrator.cpp). Panics are caught by
//! `MigrationRunner::run` via `FutureExt::catch_unwind` so a single bad article
//! cannot crash the whole runtime.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::clients::StrapiClient;
use crate::infrastructure::cache::{FailedCommentsCache, MappingCache, PartialArticleCache};
use crate::infrastructure::error::ErrorClassifier;
use crate::infrastructure::http::HttpClient;
use crate::infrastructure::logging::{
    ErrorDiagnostics, LogLevel, LogMetadata, Logger, PerformanceMetrics, RetryContext,
};
use crate::models::{CommentFailureRecord, ContentBlock, PartialArticleRecord, WpPost};
use crate::services::article_transformer::ArticleTransformer;
use crate::services::content_parser::ContentParser;
use crate::services::content_rebuilder::ContentRebuilder;
use crate::services::excerpt_normalizer::ExcerptNormalizer;
use crate::services::image_service::ImageService;

const COMPONENT: &str = "ArticleMigrationOrchestrator";
const MAX_ARTICLE_RETRIES: u32 = 3;

pub struct ArticleMigrationOrchestrator {
    strapi: StrapiClient,
    cache: Arc<MappingCache>,
    logger: Arc<Logger>,
    image_service: ImageService,
    partial_path: PathBuf,
    failed_comments_path: PathBuf,
}

impl ArticleMigrationOrchestrator {
    pub fn new(
        strapi: StrapiClient,
        http: HttpClient,
        cache: Arc<MappingCache>,
        logger: Arc<Logger>,
        partial_path: impl Into<PathBuf>,
        failed_comments_path: impl Into<PathBuf>,
    ) -> Self {
        let image_service = ImageService::new(strapi.clone(), http);
        Self {
            strapi,
            cache,
            logger,
            image_service,
            partial_path: partial_path.into(),
            failed_comments_path: failed_comments_path.into(),
        }
    }

    fn log(&self, level: LogLevel, op: &str, post_id: i64, message: &str) {
        self.logger.log(
            LogMetadata::new(level, COMPONENT)
                .with_operation(op)
                .with_wp_post_id(post_id)
                .with_message(message),
        );
    }

    /// Returns true if the article POST succeeded (comments may still have failed).
    pub async fn migrate_post(&self, post: &WpPost) -> bool {
        let article_start = Instant::now();
        let mut perf = PerformanceMetrics::default();

        self.log(LogLevel::Info, "migrate_start", post.id, "Begin article migration");

        // ---------------- 1. Parse content ----------------
        let t = Instant::now();
        let blocks = ContentParser::parse(&post.content);
        perf.parse_time_ms = Some(t.elapsed().as_secs_f64() * 1000.0);
        self.logger.log(LogMetadata {
            level: Some(LogLevel::Debug),
            component: Some(COMPONENT.into()),
            operation: Some("parse_success".into()),
            wp_post_id: Some(post.id),
            duration_ms: perf.parse_time_ms,
            message: Some(format!("Parsed {} blocks", blocks.len())),
            ..Default::default()
        });

        let mut working_post = post.clone();

        // ---------------- 2. Cover image (non-fatal) ----------------
        if !working_post.cover_image.source_url.is_empty() {
            let t = Instant::now();
            match self.image_service.process_cover_image(&working_post.cover_image.source_url).await {
                Ok(cover) => {
                    working_post.cover_image.id = cover.strapi_media_id;
                    perf.cover_image_time_ms = Some(t.elapsed().as_secs_f64() * 1000.0);
                    self.logger.log(LogMetadata {
                        level: Some(LogLevel::Debug),
                        component: Some(COMPONENT.into()),
                        operation: Some("cover_image_upload".into()),
                        wp_post_id: Some(post.id),
                        strapi_media_id: Some(cover.strapi_media_id),
                        image_url: Some(working_post.cover_image.source_url.clone()),
                        duration_ms: perf.cover_image_time_ms,
                        ..Default::default()
                    });
                }
                Err(e) => {
                    working_post.cover_image.id = -1;
                    self.logger.log(LogMetadata {
                        level: Some(LogLevel::Warn),
                        component: Some(COMPONENT.into()),
                        operation: Some("cover_image_failed".into()),
                        wp_post_id: Some(post.id),
                        image_url: Some(working_post.cover_image.source_url.clone()),
                        message: Some("Cover image failed; continuing without cover".into()),
                        error_diagnostics: Some(ErrorDiagnostics {
                            response_body: e.to_string(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                }
            }
        }

        if blocks.is_empty() {
            self.log(LogLevel::Warn, "empty_blocks", post.id, "No content blocks parsed");
            return false;
        }

        // ---------------- 3. Body images (STRICT) ----------------
        let image_blocks: Vec<&ContentBlock> = blocks.iter().filter(|b| b.is_image()).collect();
        let t = Instant::now();
        let image_result = self.image_service.process_images_strict(&image_blocks).await;
        perf.body_images_time_ms = Some(t.elapsed().as_secs_f64() * 1000.0);

        self.logger.log(LogMetadata {
            level: Some(if image_result.success { LogLevel::Info } else { LogLevel::Warn }),
            component: Some(COMPONENT.into()),
            operation: Some("image_processing".into()),
            wp_post_id: Some(post.id),
            total_images: Some(image_result.total_image_blocks_required),
            successful_images: Some(image_result.successfully_processed_count),
            failed_images: Some(image_result.failed_image_urls.len() as i32),
            duration_ms: perf.body_images_time_ms,
            ..Default::default()
        });

        if !image_result.success {
            let mut record = PartialArticleRecord::new_now();
            record.wp_post_id = post.id;
            record.title = post.title.clone();
            record.cover_image_strapi_id = working_post.cover_image.id;
            for (idx, url) in &image_result.url_mapping {
                record.uploaded_images.insert(idx.to_string(), url.clone());
            }
            record.failed_image_urls = image_result.failed_image_urls.clone();
            record.failure_reason = image_result.failure_reason.clone();
            record.upload_attempts = 1;

            match PartialArticleCache::save(&self.partial_path, &record) {
                Ok(()) => self.log(LogLevel::Info, "partial_save_success", post.id, "Partial article saved for retry"),
                Err(e) => self.logger.log(LogMetadata {
                    level: Some(LogLevel::Warn),
                    component: Some(COMPONENT.into()),
                    operation: Some("partial_save_failed".into()),
                    wp_post_id: Some(post.id),
                    message: Some(format!("Failed to save partial article: {e}")),
                    ..Default::default()
                }),
            }

            self.logger.log(LogMetadata {
                level: Some(LogLevel::Error),
                component: Some(COMPONENT.into()),
                operation: Some("image_processing_failed".into()),
                wp_post_id: Some(post.id),
                error_type: Some("IMAGE_DOWNLOAD_ERROR".into()),
                message: Some(image_result.failure_reason.clone()),
                ..Default::default()
            });
            return false;
        }

        // ---------------- 4. Rebuild HTML ----------------
        let t = Instant::now();
        let rebuilt = ContentRebuilder::rebuild(&blocks, &image_result.url_mapping);
        working_post.content = rebuilt.clone();
        perf.content_rebuild_time_ms = Some(t.elapsed().as_secs_f64() * 1000.0);
        self.logger.log(LogMetadata {
            level: Some(LogLevel::Debug),
            component: Some(COMPONENT.into()),
            operation: Some("content_rebuild".into()),
            wp_post_id: Some(post.id),
            duration_ms: perf.content_rebuild_time_ms,
            ..Default::default()
        });

        // ---------------- 5. Transform ----------------
        let payload = ArticleTransformer::transform(&working_post, &rebuilt, &blocks, &self.cache);
        if payload.ds_author.is_empty() {
            self.logger.log(LogMetadata {
                level: Some(LogLevel::Fatal),
                component: Some(COMPONENT.into()),
                operation: Some("validate_payload".into()),
                wp_post_id: Some(post.id),
                error_type: Some("VALIDATION_ERROR".into()),
                failed_field_name: Some("ds_author".into()),
                message: Some("ds_author resolved to empty — cannot post article".into()),
                error_diagnostics: Some(ErrorDiagnostics {
                    validation_error: "Author mapping missing".into(),
                    failed_field_name: "ds_author".into(),
                    suggestion_or_note: "Run author migration first or check MappingCache".into(),
                    ..Default::default()
                }),
                ..Default::default()
            });
            return false;
        }

        // ---------------- 6. Build payload JSON ----------------
        let mut data = serde_json::Map::new();
        data.insert("Site".into(), json!(payload.site));
        data.insert("Title".into(), json!(payload.title));
        data.insert("Slug".into(), json!(payload.slug));
        data.insert("Excerpt".into(), json!(payload.excerpt));
        data.insert("Publish_Date".into(), json!(payload.publish_date));
        data.insert("Body".into(), json!(payload.body));
        data.insert("Allow_Comment".into(), json!(payload.allow_comment));

        if !payload.ds_performers.is_empty() {
            data.insert("ds_performers".into(), json!({ "connect": payload.ds_performers }));
        }
        if !payload.ds_studios.is_empty() {
            data.insert("ds_studios".into(), json!({ "connect": payload.ds_studios }));
        }
        if !payload.ds_author.is_empty() {
            data.insert("ds_author".into(), json!({ "connect": [payload.ds_author] }));
        }
        if !payload.ds_sub_category.is_empty() {
            data.insert("ds_sub_category".into(), json!({ "connect": [payload.ds_sub_category] }));
        }

        let mut cover_obj = serde_json::Map::new();
        cover_obj.insert("AltTag".into(), json!(payload.cover_image.alt_tag));
        match payload.cover_image.public {
            Some(id) if id > 0 => { cover_obj.insert("Public".into(), json!(id)); }
            _ => { cover_obj.insert("Public".into(), Value::Null); }
        }
        data.insert("Cover_Image".into(), Value::Object(cover_obj));

        if !payload.registration_cta_block.is_empty() {
            let arr: Vec<Value> = payload.registration_cta_block.iter().map(|b| json!({
                "Enabled": b.enabled,
                "position_after_paragraph": b.position_after_paragraph,
                "label": b.label,
                "url": b.url,
            })).collect();
            data.insert("registration_cta_block".into(), Value::Array(arr));
        }

        let payload_json = json!({ "data": Value::Object(data) });
        let payload_str = payload_json.to_string();
        self.logger.log(LogMetadata {
            level: Some(LogLevel::Debug),
            component: Some(COMPONENT.into()),
            operation: Some("payload_construction".into()),
            wp_post_id: Some(post.id),
            article_title: Some(payload.title.clone()),
            payload_size_bytes: Some(payload_str.len() as i64),
            ..Default::default()
        });

        // ---------------- 7. POST article with retries ----------------
        let t = Instant::now();
        let mut posted = false;
        let mut last_status: u16 = 0;
        let mut last_body = String::new();
        let mut last_error: Option<String> = None;
        let mut document_id = String::new();
        let mut attempts_used = 0u32;

        for attempt in 1..=MAX_ARTICLE_RETRIES {
            attempts_used = attempt;
            match self.strapi.post_article(&payload_str).await {
                Ok(resp) => {
                    last_status = resp.status_code;
                    last_body = resp.body.clone();
                    if resp.is_success() {
                        if let Ok(v) = serde_json::from_str::<Value>(&resp.body) {
                            document_id = v
                                .pointer("/data/documentId")
                                .and_then(|x| x.as_str())
                                .map(|s| s.to_string())
                                .or_else(|| v.pointer("/data/id").and_then(|x| x.as_i64()).map(|i| i.to_string()))
                                .unwrap_or_default();
                        }
                        posted = true;
                        self.logger.log(LogMetadata {
                            level: Some(LogLevel::Info),
                            component: Some(COMPONENT.into()),
                            operation: Some("article_post_success".into()),
                            wp_post_id: Some(post.id),
                            strapi_doc_id: Some(document_id.clone()),
                            http_status: Some(resp.status_code as i32),
                            duration_ms: Some(t.elapsed().as_secs_f64() * 1000.0),
                            ..Default::default()
                        });
                        break;
                    }
                    let et = ErrorClassifier::classify_http(resp.status_code);
                    let retryable = ErrorClassifier::is_retryable(et);
                    let backoff = backoff_ms(attempt);
                    self.logger.log(LogMetadata {
                        level: Some(LogLevel::Warn),
                        component: Some(COMPONENT.into()),
                        operation: Some("article_post_retry".into()),
                        wp_post_id: Some(post.id),
                        http_status: Some(resp.status_code as i32),
                        error_type: Some(et.name().into()),
                        response_snippet: Some(truncate(&resp.body, 200)),
                        retry_context: Some(RetryContext {
                            attempt_number: attempt as i32,
                            max_attempts: MAX_ARTICLE_RETRIES as i32,
                            next_retry_in_ms: if retryable && attempt < MAX_ARTICLE_RETRIES { backoff as i32 } else { 0 },
                            previous_error_type: et.name().into(),
                        }),
                        ..Default::default()
                    });
                    if !retryable || attempt == MAX_ARTICLE_RETRIES { break; }
                    tokio::time::sleep(Duration::from_millis(backoff)).await;
                }
                Err(e) => {
                    let et = ErrorClassifier::classify_anyhow(&e);
                    let retryable = ErrorClassifier::is_retryable(et);
                    let backoff = backoff_ms(attempt);
                    last_error = Some(e.to_string());
                    self.logger.log(LogMetadata {
                        level: Some(LogLevel::Warn),
                        component: Some(COMPONENT.into()),
                        operation: Some("article_post_retry".into()),
                        wp_post_id: Some(post.id),
                        error_type: Some(et.name().into()),
                        message: Some(e.to_string()),
                        retry_context: Some(RetryContext {
                            attempt_number: attempt as i32,
                            max_attempts: MAX_ARTICLE_RETRIES as i32,
                            next_retry_in_ms: if retryable && attempt < MAX_ARTICLE_RETRIES { backoff as i32 } else { 0 },
                            previous_error_type: et.name().into(),
                        }),
                        ..Default::default()
                    });
                    if !retryable || attempt == MAX_ARTICLE_RETRIES { break; }
                    tokio::time::sleep(Duration::from_millis(backoff)).await;
                }
            }
        }
        perf.article_post_time_ms = Some(t.elapsed().as_secs_f64() * 1000.0);

        if !posted {
            let failure_reason = format!(
                "Article POST failed with HTTP {}. {}",
                last_status,
                last_error.unwrap_or_else(|| truncate(&last_body, 200))
            );
            self.logger.log(LogMetadata {
                level: Some(LogLevel::Error),
                component: Some(COMPONENT.into()),
                operation: Some("article_post_final".into()),
                wp_post_id: Some(post.id),
                http_status: Some(last_status as i32),
                error_type: Some(ErrorClassifier::classify_http(last_status).name().into()),
                response_snippet: Some(truncate(&last_body, 200)),
                message: Some(failure_reason.clone()),
                ..Default::default()
            });

            if !image_result.url_mapping.is_empty() {
                let mut rec = PartialArticleRecord::new_now();
                rec.wp_post_id = post.id;
                rec.title = post.title.clone();
                rec.cover_image_strapi_id = working_post.cover_image.id;
                for (idx, url) in &image_result.url_mapping {
                    rec.uploaded_images.insert(idx.to_string(), url.clone());
                }
                rec.failure_reason = failure_reason;
                rec.upload_attempts = 1;
                rec.article_creation_attempts = attempts_used as i32;
                match PartialArticleCache::save(&self.partial_path, &rec) {
                    Ok(()) => self.log(LogLevel::Info, "partial_save_article_failed", post.id, "Saved partial after article POST failure"),
                    Err(e) => self.logger.log(LogMetadata {
                        level: Some(LogLevel::Warn),
                        component: Some(COMPONENT.into()),
                        operation: Some("partial_save_failed".into()),
                        wp_post_id: Some(post.id),
                        message: Some(format!("Failed to save partial: {e}")),
                        ..Default::default()
                    }),
                }
            }
            return false;
        }

        // ---------------- 8. Comments ----------------
        let t = Instant::now();
        let mut succeeded_comments = 0;
        let mut failed_comments: Vec<CommentFailureRecord> = Vec::new();
        self.logger.log(LogMetadata {
            level: Some(LogLevel::Debug),
            component: Some(COMPONENT.into()),
            operation: Some("comments_start".into()),
            wp_post_id: Some(post.id),
            total_comments: Some(post.comments.len() as i32),
            ..Default::default()
        });

        for c in &post.comments {
            let text = ExcerptNormalizer::normalize_comment(&c.content);
            let user_id = self.cache.get_user_strapi_id(c.author_id);
            let body = json!({
                "data": {
                    "Comment": text,
                    "articleId": { "connect": [document_id.clone()] },
                    "userId": user_id,
                    "commentedAt": c.date,
                }
            });

            match self.strapi.post_migration_comment(&body.to_string()).await {
                Ok(resp) if resp.is_success() => {
                    succeeded_comments += 1;
                    self.logger.log(LogMetadata {
                        level: Some(LogLevel::Debug),
                        component: Some(COMPONENT.into()),
                        operation: Some("post_comment_success".into()),
                        wp_post_id: Some(post.id),
                        wp_comment_id: Some(c.id),
                        http_status: Some(resp.status_code as i32),
                        ..Default::default()
                    });
                }
                Ok(resp) => {
                    let reason = format!("HTTP {}: {}", resp.status_code, truncate(&resp.body, 200));
                    let mut rec = CommentFailureRecord::new_now();
                    rec.wp_comment_id = c.id;
                    rec.wp_post_id = post.id;
                    rec.strapi_article_doc_id = document_id.clone();
                    rec.comment_content = text;
                    rec.author_id = c.author_id;
                    rec.original_date = c.date.clone();
                    rec.failure_reason = reason.clone();
                    rec.posting_attempts = 1;
                    self.logger.log(LogMetadata {
                        level: Some(LogLevel::Warn),
                        component: Some(COMPONENT.into()),
                        operation: Some("post_comment_failed".into()),
                        wp_post_id: Some(post.id),
                        wp_comment_id: Some(c.id),
                        http_status: Some(resp.status_code as i32),
                        error_type: Some(ErrorClassifier::classify_http(resp.status_code).name().into()),
                        response_snippet: Some(truncate(&resp.body, 200)),
                        message: Some(reason),
                        ..Default::default()
                    });
                    failed_comments.push(rec);
                }
                Err(e) => {
                    let reason = e.to_string();
                    let mut rec = CommentFailureRecord::new_now();
                    rec.wp_comment_id = c.id;
                    rec.wp_post_id = post.id;
                    rec.strapi_article_doc_id = document_id.clone();
                    rec.comment_content = text;
                    rec.author_id = c.author_id;
                    rec.original_date = c.date.clone();
                    rec.failure_reason = reason.clone();
                    rec.posting_attempts = 1;
                    self.logger.log(LogMetadata {
                        level: Some(LogLevel::Warn),
                        component: Some(COMPONENT.into()),
                        operation: Some("post_comment_error".into()),
                        wp_post_id: Some(post.id),
                        wp_comment_id: Some(c.id),
                        error_type: Some(ErrorClassifier::classify_anyhow(&e).name().into()),
                        message: Some(reason),
                        ..Default::default()
                    });
                    failed_comments.push(rec);
                }
            }
        }
        perf.comments_time_ms = Some(t.elapsed().as_secs_f64() * 1000.0);

        if !failed_comments.is_empty() {
            let mut cached_ok = 0;
            let mut cached_err = 0;
            for rec in &failed_comments {
                match FailedCommentsCache::save(&self.failed_comments_path, rec) {
                    Ok(()) => cached_ok += 1,
                    Err(_) => cached_err += 1,
                }
            }
            self.logger.log(LogMetadata {
                level: Some(if cached_err == 0 { LogLevel::Info } else { LogLevel::Warn }),
                component: Some(COMPONENT.into()),
                operation: Some("comments_cached".into()),
                wp_post_id: Some(post.id),
                failed_comments: Some(failed_comments.len() as i32),
                message: Some(format!("Cached {cached_ok} failed comments, {cached_err} cache errors")),
                ..Default::default()
            });
        }

        // ---------------- 9. Final summary ----------------
        perf.total_time_ms = Some(article_start.elapsed().as_secs_f64() * 1000.0);
        self.logger.log(LogMetadata {
            level: Some(LogLevel::Info),
            component: Some(COMPONENT.into()),
            operation: Some("article_complete".into()),
            wp_post_id: Some(post.id),
            strapi_doc_id: Some(document_id),
            total_comments: Some(post.comments.len() as i32),
            successful_comments: Some(succeeded_comments),
            failed_comments: Some(failed_comments.len() as i32),
            performance_metrics: Some(perf),
            ..Default::default()
        });

        true
    }
}

fn backoff_ms(attempt: u32) -> u64 {
    let m = 1.5f64.powi((attempt as i32).saturating_sub(1));
    (1000.0 * m).round() as u64
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { s.chars().take(n).collect() }
}
