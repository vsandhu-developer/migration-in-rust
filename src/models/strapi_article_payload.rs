use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoverImagePayload {
    pub public: Option<i64>,
    pub alt_tag: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegistrationCtaBlock {
    pub enabled: bool,
    pub position_after_paragraph: i32,
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrapiArticlePayload {
    pub site: String,
    pub title: String,
    pub slug: String,
    pub excerpt: String,
    pub publish_date: String,
    pub body: String,
    pub ds_performers: Vec<String>,
    pub ds_studios: Vec<String>,
    pub cover_image: CoverImagePayload,
    pub ds_author: String,
    pub ds_sub_category: String,
    pub allow_comment: bool,
    pub registration_cta_block: Vec<RegistrationCtaBlock>,
}

impl Default for StrapiArticlePayload {
    fn default() -> Self {
        Self {
            site: "Daily_Squirt".to_string(),
            title: String::new(),
            slug: String::new(),
            excerpt: String::new(),
            publish_date: String::new(),
            body: String::new(),
            ds_performers: Vec::new(),
            ds_studios: Vec::new(),
            cover_image: CoverImagePayload::default(),
            ds_author: String::new(),
            ds_sub_category: String::new(),
            allow_comment: false,
            registration_cta_block: Vec::new(),
        }
    }
}
