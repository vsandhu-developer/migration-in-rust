use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DsSubCategory {
    pub wp_id: i64,
    pub parent_wp_id: i64,
    pub name: String,
    pub slug: String,
    pub access_level: String,
    pub parent_name: String,
}
