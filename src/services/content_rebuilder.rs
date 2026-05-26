//! Re-serialize parsed blocks back to an HTML body string for Strapi.
//! Substitutes Strapi-hosted image URLs in place of WP origin URLs.

use std::collections::HashMap;

use crate::models::{BlockType, ContentBlock};

pub struct ContentRebuilder;

impl ContentRebuilder {
    pub fn rebuild(blocks: &[ContentBlock], image_url_map: &HashMap<i32, String>) -> String {
        let mut sorted: Vec<&ContentBlock> = blocks.iter().collect();
        sorted.sort_by_key(|b| b.index);

        let mut out = String::new();
        for blk in sorted {
            match blk.block_type {
                BlockType::Cta => {
                    // CTAs are carried in the structured payload field — drop from body.
                }
                BlockType::Image => {
                    let new_url = image_url_map
                        .get(&blk.index)
                        .cloned()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| blk.src.clone());
                    if new_url.is_empty() {
                        continue;
                    }
                    // Fixed bug: preserve original alt text instead of hardcoding alt="".
                    let alt = html_escape::encode_quoted_attribute(&blk.alt);
                    let url = html_escape::encode_quoted_attribute(&new_url);
                    out.push_str(&format!(
                        "<p><img src=\"{}\" alt=\"{}\"/></p>",
                        url, alt
                    ));
                }
                BlockType::Html => {
                    out.push_str(&blk.raw_html);
                }
            }
        }
        out
    }
}
