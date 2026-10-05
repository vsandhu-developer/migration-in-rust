use crate::error::{require, Error, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use unicode_normalization::UnicodeNormalization;

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn hash_valid(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|x| x.is_ascii_digit() || (b'a'..=b'f').contains(&x))
}
pub fn source_key(system: &str, id: &str) -> Result<String> {
    require(
        !system.is_empty()
            && system.len() <= 64
            && system.as_bytes()[0].is_ascii_lowercase()
            && system
                .bytes()
                .all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || b"._-".contains(&x)),
        "source_system_invalid",
    )?;
    require(
        !id.is_empty()
            && id.encode_utf16().count() <= 255
            && id.trim() == id
            && !id.chars().any(|x| x <= '\u{1f}' || x == '\u{7f}'),
        "source_id_invalid",
    )?;
    let key = serde_json::to_string(&[system, id]).map_err(|_| Error::new("source_key_invalid"))?;
    require(key.encode_utf16().count() <= 512, "source_key_invalid")?;
    Ok(key)
}
pub fn parse_key(key: &str) -> Result<(String, String)> {
    let pair: [String; 2] =
        serde_json::from_str(key).map_err(|_| Error::new("source_key_invalid"))?;
    require(source_key(&pair[0], &pair[1])? == key, "source_key_invalid")?;
    Ok((pair[0].clone(), pair[1].clone()))
}

/// Matches ds-source.js (including ECMAScript numeric formatting and UTF-16 key ordering).
pub fn stringify(value: &Value, normalize: bool) -> Result<String> {
    fn walk(v: &Value, n: bool, depth: usize, count: &mut usize) -> Result<String> {
        *count += 1;
        require(
            depth <= 32 && *count <= 100000,
            "source_payload_too_complex",
        )?;
        Ok(match v {
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Number(x) => {
                let f = x.as_f64().ok_or_else(|| Error::new("number_invalid"))?;
                require(
                    f.is_finite() && f.abs() <= 9007199254740991.0,
                    "number_unsafe",
                )?;
                if f == 0.0 {
                    "0".into()
                } else {
                    ryu_js::Buffer::new().format(f).to_owned()
                }
            }
            Value::String(s) => serde_json::to_string(&if n {
                s.nfc()
                    .collect::<String>()
                    .replace("\r\n", "\n")
                    .replace('\r', "\n")
            } else {
                s.clone()
            })
            .map_err(|_| Error::new("json_invalid"))?,
            Value::Array(a) => format!(
                "[{}]",
                a.iter()
                    .map(|x| walk(x, n, depth + 1, count))
                    .collect::<Result<Vec<_>>>()?
                    .join(",")
            ),
            Value::Object(o) => {
                let mut keys = o.keys().collect::<Vec<_>>();
                keys.sort_by_key(|s| s.encode_utf16().collect::<Vec<_>>());
                // JS enumerates integer-index object keys first, even after insertion in lexical order.
                keys.sort_by_key(|s| {
                    s.parse::<u32>()
                        .ok()
                        .filter(|i| *i < u32::MAX && i.to_string() == **s)
                        .map(|i| (0, i))
                        .unwrap_or((1, 0))
                });
                let mut fields = Vec::new();
                for k in keys {
                    require(
                        !["__proto__", "constructor", "prototype"].contains(&k.as_str()),
                        "source_key_forbidden",
                    )?;
                    if n && [
                        "id",
                        "documentId",
                        "createdAt",
                        "updatedAt",
                        "publishedAt",
                        "createdBy",
                        "updatedBy",
                        "migrationRunId",
                        "sourceChecksum",
                    ]
                    .contains(&k.as_str())
                    {
                        continue;
                    }
                    fields.push(format!(
                        "{}:{}",
                        serde_json::to_string(k).map_err(|_| Error::new("json_invalid"))?,
                        walk(&o[k], n, depth + 1, count)?
                    ));
                }
                format!("{{{}}}", fields.join(","))
            }
        })
    }
    let result = walk(value, normalize, 0, &mut 0)?;
    require(result.len() <= 2 * 1024 * 1024, "source_payload_too_large")?;
    Ok(result)
}
pub fn manifest_hash(value: &Value) -> Result<String> {
    let mut v = value.clone();
    v.as_object_mut()
        .ok_or_else(|| Error::new("manifest_invalid"))?
        .remove("manifestHash");
    Ok(sha256(stringify(&v, false)?.as_bytes()))
}
pub fn checksum(
    data: &Value,
    relations: BTreeMap<String, Vec<String>>,
    mut media: Vec<Value>,
) -> Result<String> {
    let relations: BTreeMap<_, _> = relations
        .into_iter()
        .map(|(k, mut v)| {
            v.sort_by_key(|s| s.encode_utf16().collect::<Vec<_>>());
            v.dedup();
            (k, v)
        })
        .collect();
    media.sort_by_key(|v| {
        v["sourceKey"]
            .as_str()
            .unwrap_or("")
            .encode_utf16()
            .collect::<Vec<_>>()
    });
    let raw = stringify(
        &json!({"version":1,"payload":data,"relations":relations,"media":media}),
        true,
    )?;
    Ok(sha256(format!("daily-squirt-source-v1\n{raw}").as_bytes()))
}
