use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WpComment {
    pub id: i64,
    pub date: String,
    pub content: String,
    pub avatar_url: String,
    pub author_id: i64,
}
