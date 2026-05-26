use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CategoryMapping {
    pub name: String,
    pub slug: String,
    pub access_level: String,
    pub document_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubCategoryItem {
    pub wp_id: i64,
    pub name: String,
    pub slug: String,
    pub access_level: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParentCategoryGroup {
    pub name: String,
    pub slug: String,
    pub access_level: String,
    pub sub_categories: Vec<SubCategoryItem>,
    pub document_id: String,
}
