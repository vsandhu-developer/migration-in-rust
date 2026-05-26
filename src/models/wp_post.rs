use serde::{Deserialize, Serialize};

use super::WpComment;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoverImage {
    pub id: i64,
    pub source_url: String,
    pub alt_text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WpPost {
    pub id: i64,
    pub date: String,
    pub slug: String,
    pub title: String,
    pub content: String,
    pub excerpt: String,
    pub categories: i64,
    pub cover_image: CoverImage,
    pub comments: Vec<WpComment>,
    pub author_id: i64,
    pub tag_ids: Vec<i64>,
    pub comments_enabled: bool,
}
