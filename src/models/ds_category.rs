use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DsCategory {
    pub wp_id: i64,
    pub name: String,
    pub slug: String,
    pub access_level: String,
}
