//! HTML -> clean UTF-8 text. Strips tags, decodes numeric entities,
//! collapses whitespace. Mirrors C++ ExcerptNormalizer behavior.

use scraper::{Html, Node};

pub struct ExcerptNormalizer;

const BLOCK_TAGS: &[&str] = &[
    "br", "p", "div", "li", "section", "article", "blockquote", "header", "footer",
    "tr", "td", "th", "ul", "ol", "h1", "h2", "h3", "h4", "h5", "h6",
];

// Strapi ds-article.Excerpt has maxLength: 300. Posts longer than this 400 out.
const EXCERPT_MAX_CHARS: usize = 300;

impl ExcerptNormalizer {
    pub fn normalize(raw: &str) -> String {
        let s = Self::extract_plain_text(raw);
        Self::cap_length(&Self::trim(&s), EXCERPT_MAX_CHARS)
    }

    fn cap_length(s: &str, max: usize) -> String {
        // Strapi validates string length using JS String.length, which counts
        // UTF-16 code units (astral chars like emoji = 2 units). Counting
        // Rust chars (scalar values) under-estimates and lets ~0.3% of
        // articles slip past the cap.
        if s.encode_utf16().count() <= max {
            return s.to_string();
        }
        let budget = max.saturating_sub(1); // leave room for the ellipsis
        let mut accumulated = String::new();
        let mut units: usize = 0;
        for ch in s.chars() {
            let n = ch.len_utf16();
            if units + n > budget { break; }
            accumulated.push(ch);
            units += n;
        }
        let base = match accumulated.rfind(char::is_whitespace) {
            Some(idx) if accumulated[idx..].chars().count() <= 40 => {
                accumulated[..idx].trim_end().to_string()
            }
            _ => accumulated,
        };
        format!("{base}…")
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
    // Walks chars, not bytes — casting raw UTF-8 bytes to `char` splits multi-byte
    // sequences and produces mojibake for any non-ASCII content (smart quotes, …, é, …).
    fn decode_numeric_entities(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            if bytes[i] == b'&' && i + 1 < bytes.len() && bytes[i + 1] == b'#' {
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
                    if let Some(ch) = code_opt.and_then(char::from_u32) {
                        out.push(ch);
                        i = end + 1;
                        continue;
                    }
                }
            }
            // Advance one full UTF-8 char (not one byte).
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
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
