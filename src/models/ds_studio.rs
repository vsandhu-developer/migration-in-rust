use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DsStudio {
    pub id: i64,
    pub name: String,
    pub slug: String,
}
