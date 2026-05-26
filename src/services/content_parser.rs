//! Parse WordPress post HTML into an ordered Vec<ContentBlock>.
//!
//! Walks the top-level children of <body> with scraper. For each top-level
//! element, classifies it as IMAGE / CTA / HTML. Avoids the C++ implementation's
//! duplication bug (where descendants were emitted both as their own blocks
//! AND included inside an HTML block).

use scraper::{ElementRef, Html, Node, Selector};

use crate::models::{BlockType, ContentBlock};

/// The four class tokens that the WP block editor applies to every CTA button.
/// All four must be present on a single element.
const CTA_TOKENS: [&str; 4] = [
    "wp-block-button__link",
    "has-white-color",
    "has-text-color",
    "has-background",
];

pub struct ContentParser;

impl ContentParser {
    pub fn parse(html_content: &str) -> Vec<ContentBlock> {
        if html_content.is_empty() {
            return Vec::new();
        }
        let doc = Html::parse_fragment(html_content);
        let mut index: i32 = 0;
        let mut out = Vec::new();

        // walk top-level children of the fragment root
        let root = doc.root_element();
        for child in root.children() {
            if let Some(el) = ElementRef::wrap(child) {
                Self::handle_element(el, &mut index, &mut out);
            } else if let Node::Text(t) = child.value() {
                let s = t.trim();
                if !s.is_empty() {
                    out.push(ContentBlock {
                        block_type: BlockType::Html,
                        index,
                        raw_html: s.to_string(),
                        ..Default::default()
                    });
                    index += 1;
                }
            }
        }
        out
    }

    fn handle_element(el: ElementRef, index: &mut i32, out: &mut Vec<ContentBlock>) {
        let name = el.value().name();
        let class_attr = el.value().attr("class").unwrap_or("");

        // <html> / <body> wrappers — recurse through to top-level children
        if name.eq_ignore_ascii_case("html") || name.eq_ignore_ascii_case("body") {
            for child in el.children() {
                if let Some(c) = ElementRef::wrap(child) {
                    Self::handle_element(c, index, out);
                }
            }
            return;
        }

        // Direct <img>
        if name.eq_ignore_ascii_case("img") {
            out.push(Self::image_block_from_img(el, *index));
            *index += 1;
            return;
        }

        // figure / wp-block-image / wp-image wrapper around an <img>
        let is_image_wrapper = name.eq_ignore_ascii_case("figure")
            || class_contains(class_attr, "wp-block-image")
            || class_contains(class_attr, "wp-image");
        if is_image_wrapper {
            if let Some(img) = find_descendant_by_tag(el, "img") {
                let mut blk = Self::image_block_from_img(img, *index);
                // Pull alt from inner <img> if present
                if blk.alt.is_empty() {
                    if let Some(alt) = img.value().attr("alt") {
                        blk.alt = alt.to_string();
                    }
                }
                out.push(blk);
                *index += 1;
                return;
            }
            // wrapper without an img — fall through to HTML serialization
        }

        // wp-block-button wrapper (find inner CTA link)
        if class_contains(class_attr, "wp-block-button") {
            if let Some(cta) = find_descendant(el, is_cta_element) {
                out.push(Self::cta_block_from(cta, *index));
                *index += 1;
                return;
            }
        }

        // Direct CTA link
        if is_cta_element(el) {
            out.push(Self::cta_block_from(el, *index));
            *index += 1;
            return;
        }

        // Anything else -> HTML block, verbatim.
        let raw = el.html();
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            out.push(ContentBlock {
                block_type: BlockType::Html,
                index: *index,
                raw_html: trimmed.to_string(),
                ..Default::default()
            });
            *index += 1;
        }
    }

    fn image_block_from_img(img: ElementRef, index: i32) -> ContentBlock {
        let src = img.value().attr("src").unwrap_or("").to_string();
        let alt = img.value().attr("alt").unwrap_or("").to_string();
        let srcset_raw = img.value().attr("srcset").unwrap_or("");
        let srcset = split_srcset(srcset_raw);
        ContentBlock {
            block_type: BlockType::Image,
            index,
            raw_html: src.clone(),
            src,
            srcset,
            alt,
            ..Default::default()
        }
    }

    fn cta_block_from(el: ElementRef, index: i32) -> ContentBlock {
        let href = el.value().attr("href").unwrap_or("").to_string();
        let text = collapse_text(&el);
        ContentBlock {
            block_type: BlockType::Cta,
            index,
            href,
            text,
            ..Default::default()
        }
    }
}

fn class_contains(class_attr: &str, token: &str) -> bool {
    class_attr.split_ascii_whitespace().any(|t| t == token)
}

fn is_cta_element(el: ElementRef) -> bool {
    let class_attr = el.value().attr("class").unwrap_or("");
    CTA_TOKENS.iter().all(|t| class_contains(class_attr, t))
}

fn find_descendant_by_tag<'a>(root: ElementRef<'a>, tag: &str) -> Option<ElementRef<'a>> {
    let sel = Selector::parse(tag).ok()?;
    root.select(&sel).next()
}

fn find_descendant<'a, F>(root: ElementRef<'a>, pred: F) -> Option<ElementRef<'a>>
where
    F: Fn(ElementRef<'a>) -> bool,
{
    for descendant in root.descendants() {
        if let Some(el) = ElementRef::wrap(descendant) {
            if pred(el) {
                return Some(el);
            }
        }
    }
    None
}

fn split_srcset(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn collapse_text(el: &ElementRef) -> String {
    let mut s = String::new();
    for t in el.text() {
        s.push_str(t);
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
