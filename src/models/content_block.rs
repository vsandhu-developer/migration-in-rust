use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockType {
    Html,
    Image,
    Cta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlock {
    pub block_type: BlockType,
    pub index: i32,
    pub raw_html: String,
    pub src: String,
    pub srcset: Vec<String>,
    pub alt: String,
    pub href: String,
    pub text: String,
}

impl Default for ContentBlock {
    fn default() -> Self {
        Self {
            block_type: BlockType::Html,
            index: 0,
            raw_html: String::new(),
            src: String::new(),
            srcset: Vec::new(),
            alt: String::new(),
            href: String::new(),
            text: String::new(),
        }
    }
}

impl ContentBlock {
    pub fn is_image(&self) -> bool {
        self.block_type == BlockType::Image
    }
    pub fn is_cta(&self) -> bool {
        self.block_type == BlockType::Cta
    }
    pub fn is_html(&self) -> bool {
        self.block_type == BlockType::Html
    }
}
