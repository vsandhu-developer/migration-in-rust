use std::collections::HashMap;

use chrono::Utc;
use serde::{Deserialize, Serialize};

fn now_unix() -> i64 {
    Utc::now().timestamp()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommentFailureRecord {
    #[serde(rename = "wpCommentId", default)]
    pub wp_comment_id: i64,
    #[serde(rename = "wpPostId", default)]
    pub wp_post_id: i64,
    #[serde(rename = "strapiArticleDocId", default)]
    pub strapi_article_doc_id: String,
    #[serde(rename = "commentContent", default)]
    pub comment_content: String,
    #[serde(rename = "authorId", default)]
    pub author_id: i64,
    #[serde(rename = "originalDate", default)]
    pub original_date: String,
    #[serde(rename = "failureReason", default)]
    pub failure_reason: String,
    #[serde(rename = "postingAttempts", default)]
    pub posting_attempts: i32,
    #[serde(rename = "createdAtUnix", default)]
    pub created_at_unix: i64,
    #[serde(rename = "updatedAtUnix", default)]
    pub updated_at_unix: i64,
}

impl CommentFailureRecord {
    pub fn new_now() -> Self {
        let ts = now_unix();
        Self {
            created_at_unix: ts,
            updated_at_unix: ts,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PartialArticleRecord {
    #[serde(rename = "wpPostId", default)]
    pub wp_post_id: i64,
    #[serde(rename = "strapiArticleDocId", default)]
    pub strapi_article_doc_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(rename = "coverImageStrapiId", default)]
    pub cover_image_strapi_id: i64,
    /// blockIndex (as string for JSON-compat) -> uploaded Strapi URL
    #[serde(rename = "uploadedImages", default)]
    pub uploaded_images: HashMap<String, String>,
    #[serde(rename = "failedImageUrls", default)]
    pub failed_image_urls: Vec<String>,
    #[serde(rename = "failureReason", default)]
    pub failure_reason: String,
    #[serde(rename = "uploadAttempts", default)]
    pub upload_attempts: i32,
    #[serde(rename = "articleCreationAttempts", default)]
    pub article_creation_attempts: i32,
    #[serde(rename = "createdAtUnix", default)]
    pub created_at_unix: i64,
    #[serde(rename = "updatedAtUnix", default)]
    pub updated_at_unix: i64,
}

impl PartialArticleRecord {
    pub fn new_now() -> Self {
        let ts = now_unix();
        Self {
            created_at_unix: ts,
            updated_at_unix: ts,
            ..Default::default()
        }
    }

    pub fn touch(&mut self) {
        self.updated_at_unix = now_unix();
    }
}

#[derive(Debug, Clone, Default)]
pub struct ImageProcessingResult {
    pub success: bool,
    pub total_image_blocks_required: i32,
    pub successfully_processed_count: i32,
    pub url_mapping: HashMap<i32, String>,
    pub failed_image_urls: Vec<String>,
    pub failure_reason: String,
}
