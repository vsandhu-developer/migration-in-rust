use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CtaBlock {
    pub url: String,
    pub content: String,
    pub index: i32,
}
