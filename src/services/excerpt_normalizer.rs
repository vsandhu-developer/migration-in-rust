//! HTML -> clean UTF-8 text. Strips tags, decodes numeric entities,
//! collapses whitespace. Mirrors C++ ExcerptNormalizer behavior.

use scraper::{Html, Node};

pub struct ExcerptNormalizer;

const BLOCK_TAGS: &[&str] = &[
    "br", "p", "div", "li", "section", "article", "blockquote", "header", "footer",
    "tr", "td", "th", "ul", "ol", "h1", "h2", "h3", "h4", "h5", "h6",
];

impl ExcerptNormalizer {
    pub fn normalize(raw: &str) -> String {
        let s = Self::extract_plain_text(raw);
        Self::trim(&s)
    }

    pub fn normalize_title(raw: &str) -> String {
        Self::trim(&Self::extract_plain_text(raw))
    }

    pub fn normalize_comment(raw: &str) -> String {
        Self::trim(&Self::extract_plain_text(raw))
    }

    fn trim(s: &str) -> String {
        s.trim().to_string()
    }

    /// Decode numeric character references (e.g. `&#8217;`, `&#x2019;`) into UTF-8.
    /// Named entities (`&amp;`, `&quot;`) are left for the HTML parser to handle.
    fn decode_numeric_entities(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'&' && i + 1 < bytes.len() && bytes[i + 1] == b'#' {
                // find ';'
                let mut end = i + 2;
                while end < bytes.len() && bytes[end] != b';' && (end - i) < 10 {
                    end += 1;
                }
                if end < bytes.len() && bytes[end] == b';' {
                    let raw = &s[i + 2..end];
                    let code_opt = if raw.starts_with('x') || raw.starts_with('X') {
                        u32::from_str_radix(&raw[1..], 16).ok()
                    } else {
                        raw.parse::<u32>().ok()
                    };
                    if let Some(cp) = code_opt {
                        if let Some(ch) = char::from_u32(cp) {
                            out.push(ch);
                            i = end + 1;
                            continue;
                        }
                    }
                }
            }
            // safe to push raw byte since we only matched ASCII '&' and '#'
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }

    fn is_block(tag: &str) -> bool {
        BLOCK_TAGS.iter().any(|t| t.eq_ignore_ascii_case(tag))
    }

    fn extract_plain_text(html_in: &str) -> String {
        if html_in.is_empty() { return String::new(); }
        let decoded = Self::decode_numeric_entities(html_in);
        // Wrap so scraper has a stable root.
        let doc = Html::parse_fragment(&decoded);
        let root_id = doc.root_element().id();
        let mut buf = String::new();
        Self::collect(&doc, root_id, &mut buf);
        Self::collapse_whitespace(&buf)
    }

    fn collect(doc: &Html, node_id: ego_tree::NodeId, out: &mut String) {
        let node_ref = doc.tree.get(node_id).unwrap();
        for child in node_ref.children() {
            match child.value() {
                Node::Text(t) => out.push_str(t),
                Node::Element(el) => {
                    let tag = el.name();
                    if tag.eq_ignore_ascii_case("br") {
                        out.push(' ');
                    } else {
                        Self::collect(doc, child.id(), out);
                        if Self::is_block(tag) {
                            out.push(' ');
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn collapse_whitespace(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut prev_space = false;
        for ch in s.chars() {
            if ch == '\r' { continue; }
            if ch.is_whitespace() {
                if !prev_space && !out.is_empty() {
                    out.push(' ');
                    prev_space = true;
                }
            } else {
                out.push(ch);
                prev_space = false;
            }
        }
        out.trim().to_string()
    }
}
