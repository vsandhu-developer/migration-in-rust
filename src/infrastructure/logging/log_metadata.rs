use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
            LogLevel::Fatal => "FATAL",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerformanceMetrics {
    pub parse_time_ms: Option<f64>,
    pub cover_image_time_ms: Option<f64>,
    pub body_images_time_ms: Option<f64>,
    pub content_rebuild_time_ms: Option<f64>,
    pub article_post_time_ms: Option<f64>,
    pub comments_time_ms: Option<f64>,
    pub total_time_ms: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetryContext {
    pub attempt_number: i32,
    pub max_attempts: i32,
    pub next_retry_in_ms: i32,
    pub previous_error_type: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ErrorDiagnostics {
    pub response_body: String,
    pub failed_field_name: String,
    pub validation_error: String,
    pub suggestion_or_note: String,
}

#[derive(Debug, Clone, Default)]
pub struct LogMetadata {
    pub timestamp: Option<String>,
    pub unix_time: Option<i64>,
    pub level: Option<LogLevel>,
    pub component: Option<String>,

    pub wp_post_id: Option<i64>,
    pub wp_comment_id: Option<i64>,
    pub strapi_doc_id: Option<String>,
    pub strapi_media_id: Option<i64>,

    pub article_title: Option<String>,
    pub article_summary: Option<String>,
    pub payload_size_bytes: Option<i64>,

    pub operation: Option<String>,
    pub message: Option<String>,
    pub error_type: Option<String>,
    pub http_status: Option<i32>,
    pub duration_ms: Option<f64>,

    pub error_diagnostics: Option<ErrorDiagnostics>,
    pub failed_field_name: Option<String>,
    pub response_snippet: Option<String>,

    pub image_url: Option<String>,
    pub image_block_index: Option<i32>,
    pub total_images: Option<i32>,
    pub successful_images: Option<i32>,
    pub failed_images: Option<i32>,
    pub image_media_id: Option<i64>,

    pub total_comments: Option<i32>,
    pub successful_comments: Option<i32>,
    pub failed_comments: Option<i32>,

    pub retry_context: Option<RetryContext>,
    pub performance_metrics: Option<PerformanceMetrics>,

    pub total_articles: Option<i32>,
    pub successful_articles: Option<i32>,
    pub failed_articles: Option<i32>,
    pub total_duration_ms: Option<f64>,

    pub cache_hit: Option<bool>,
    pub cache_type: Option<String>,
    pub cached_records: Option<i32>,
}

impl LogMetadata {
    pub fn new(level: LogLevel, component: impl Into<String>) -> Self {
        Self {
            level: Some(level),
            component: Some(component.into()),
            ..Default::default()
        }
    }

    pub fn with_message(mut self, msg: impl Into<String>) -> Self {
        self.message = Some(msg.into());
        self
    }
    pub fn with_operation(mut self, op: impl Into<String>) -> Self {
        self.operation = Some(op.into());
        self
    }
    pub fn with_wp_post_id(mut self, id: i64) -> Self {
        self.wp_post_id = Some(id);
        self
    }
    pub fn with_strapi_doc_id(mut self, id: impl Into<String>) -> Self {
        self.strapi_doc_id = Some(id.into());
        self
    }
    pub fn with_duration_ms(mut self, ms: f64) -> Self {
        self.duration_ms = Some(ms);
        self
    }
    pub fn with_http_status(mut self, status: i32) -> Self {
        self.http_status = Some(status);
        self
    }
    pub fn with_error_type(mut self, ty: impl Into<String>) -> Self {
        self.error_type = Some(ty.into());
        self
    }

    pub fn ensure_timestamp(&mut self) {
        if self.timestamp.is_none() {
            self.timestamp = Some(Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        }
        if self.unix_time.is_none() {
            self.unix_time = Some(Utc::now().timestamp());
        }
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        if let Some(v) = &self.timestamp { m.insert("timestamp".into(), Value::String(v.clone())); }
        if let Some(v) = self.unix_time { m.insert("unixTime".into(), Value::from(v)); }
        if let Some(v) = self.level { m.insert("level".into(), Value::String(v.as_str().into())); }
        if let Some(v) = &self.component { m.insert("component".into(), Value::String(v.clone())); }

        if let Some(v) = self.wp_post_id { m.insert("wpPostId".into(), Value::from(v)); }
        if let Some(v) = self.wp_comment_id { m.insert("wpCommentId".into(), Value::from(v)); }
        if let Some(v) = &self.strapi_doc_id { m.insert("strapiDocId".into(), Value::String(v.clone())); }
        if let Some(v) = self.strapi_media_id { m.insert("strapiMediaId".into(), Value::from(v)); }

        if let Some(v) = &self.article_title { m.insert("articleTitle".into(), Value::String(v.clone())); }
        if let Some(v) = &self.article_summary { m.insert("articleSummary".into(), Value::String(v.clone())); }
        if let Some(v) = self.payload_size_bytes { m.insert("payloadSizeBytes".into(), Value::from(v)); }

        if let Some(v) = &self.operation { m.insert("operation".into(), Value::String(v.clone())); }
        if let Some(v) = &self.message { m.insert("message".into(), Value::String(v.clone())); }
        if let Some(v) = &self.error_type { m.insert("errorType".into(), Value::String(v.clone())); }
        if let Some(v) = self.http_status { m.insert("httpStatus".into(), Value::from(v)); }
        if let Some(v) = self.duration_ms { m.insert("durationMs".into(), Value::from(v)); }

        if let Some(ed) = &self.error_diagnostics {
            m.insert("errorDiagnostics".into(), json!({
                "responseBody": ed.response_body,
                "failedFieldName": ed.failed_field_name,
                "validationError": ed.validation_error,
                "suggestionOrNote": ed.suggestion_or_note,
            }));
        }
        if let Some(v) = &self.failed_field_name { m.insert("failedFieldName".into(), Value::String(v.clone())); }
        if let Some(v) = &self.response_snippet { m.insert("responseSnippet".into(), Value::String(v.clone())); }

        if let Some(v) = &self.image_url { m.insert("imageUrl".into(), Value::String(v.clone())); }
        if let Some(v) = self.image_block_index { m.insert("imageBlockIndex".into(), Value::from(v)); }
        if let Some(v) = self.total_images { m.insert("totalImages".into(), Value::from(v)); }
        if let Some(v) = self.successful_images { m.insert("successfulImages".into(), Value::from(v)); }
        if let Some(v) = self.failed_images { m.insert("failedImages".into(), Value::from(v)); }
        if let Some(v) = self.image_media_id { m.insert("imageMediaId".into(), Value::from(v)); }

        if let Some(v) = self.total_comments { m.insert("totalComments".into(), Value::from(v)); }
        if let Some(v) = self.successful_comments { m.insert("successfulComments".into(), Value::from(v)); }
        if let Some(v) = self.failed_comments { m.insert("failedComments".into(), Value::from(v)); }

        if let Some(rc) = &self.retry_context {
            m.insert("retryContext".into(), json!({
                "attemptNumber": rc.attempt_number,
                "maxAttempts": rc.max_attempts,
                "nextRetryInMs": rc.next_retry_in_ms,
                "previousErrorType": rc.previous_error_type,
            }));
        }
        if let Some(pm) = &self.performance_metrics {
            let mut pmap = Map::new();
            if let Some(v) = pm.parse_time_ms { pmap.insert("parseTimeMs".into(), Value::from(v)); }
            if let Some(v) = pm.cover_image_time_ms { pmap.insert("coverImageTimeMs".into(), Value::from(v)); }
            if let Some(v) = pm.body_images_time_ms { pmap.insert("bodyImagesTimeMs".into(), Value::from(v)); }
            if let Some(v) = pm.content_rebuild_time_ms { pmap.insert("contentRebuildTimeMs".into(), Value::from(v)); }
            if let Some(v) = pm.article_post_time_ms { pmap.insert("articlePostTimeMs".into(), Value::from(v)); }
            if let Some(v) = pm.comments_time_ms { pmap.insert("commentsTimeMs".into(), Value::from(v)); }
            if let Some(v) = pm.total_time_ms { pmap.insert("totalTimeMs".into(), Value::from(v)); }
            m.insert("performanceMetrics".into(), Value::Object(pmap));
        }

        if let Some(v) = self.total_articles { m.insert("totalArticles".into(), Value::from(v)); }
        if let Some(v) = self.successful_articles { m.insert("successfulArticles".into(), Value::from(v)); }
        if let Some(v) = self.failed_articles { m.insert("failedArticles".into(), Value::from(v)); }
        if let Some(v) = self.total_duration_ms { m.insert("totalDurationMs".into(), Value::from(v)); }

        if let Some(v) = self.cache_hit { m.insert("cacheHit".into(), Value::Bool(v)); }
        if let Some(v) = &self.cache_type { m.insert("cacheType".into(), Value::String(v.clone())); }
        if let Some(v) = self.cached_records { m.insert("cachedRecords".into(), Value::from(v)); }

        Value::Object(m)
    }
}
