//! Go-compatible digest and point-id derivation.
//!
//! The Qdrant index is written by the Go engine. To read it, this module must
//! reproduce, byte-for-byte, the JSON that Go's `encoding/json` would produce
//! for the same value, then hash it the same way:
//!
//!   digest(v)  = lowercase_hex(sha256(go_json_marshal(v)))
//!   pointID(v) = digest[0:8]-[8:12]-[12:16]-[16:20]-[20:32]  (UUID-shaped)
//!
//! The subtle part is string escaping. Go's encoder is HTML-safe by default:
//!   * `<` `>` `&`          -> \u003c \u003e \u0026
//!   * U+2028 U+2029        -> \u2028 \u2029
//!   * `"` `\`              -> \" \\
//!   * \n \r \t \b \f       -> short escapes
//!   * other control chars  -> \u00xx (lowercase hex)
//!   * `/` and non-ASCII    -> emitted verbatim (UTF-8)
//!
//! serde_json does NOT HTML-escape, so we implement a minimal Go-compatible
//! encoder for exactly the value shapes we hash (strings, structs rendered as
//! ordered maps, arrays, integers, and the fixed-shape payloads below).

use sha2::{Digest, Sha256};

/// A JSON value restricted to what we need to hash in a Go-compatible way.
/// Object key order is preserved (Go emits struct fields in declaration
/// order), so callers build objects in the exact Go field order.
pub enum GoJson {
    Str(String),
    Int(i64),
    Bool(bool),
    Array(Vec<GoJson>),
    /// Ordered key/value pairs. Order matters and must match Go field order.
    Object(Vec<(&'static str, GoJson)>),
}

impl GoJson {
    pub fn s(v: impl Into<String>) -> GoJson {
        GoJson::Str(v.into())
    }
}

/// Serialize into a byte buffer exactly as Go's `json.Marshal` would.
fn encode(value: &GoJson, out: &mut String) {
    match value {
        GoJson::Str(s) => encode_string(s, out),
        GoJson::Int(n) => out.push_str(&n.to_string()),
        GoJson::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        GoJson::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode(item, out);
            }
            out.push(']');
        }
        GoJson::Object(fields) => {
            out.push('{');
            for (i, (k, v)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_string(k, out);
                out.push(':');
                encode(v, out);
            }
            out.push('}');
        }
    }
}

/// Encode a Go string literal with HTML-safe escaping identical to
/// `encoding/json`'s default behavior.
fn encode_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            // HTML-safe escapes applied by Go's default encoder.
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => {
                // Remaining control characters: \u00xx lowercase hex.
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn marshal(value: &GoJson) -> String {
    let mut out = String::new();
    encode(value, &mut out);
    out
}

/// lowercase_hex(sha256(go_json_marshal(value)))
pub fn digest(value: &GoJson) -> String {
    let bytes = marshal(value);
    let mut hasher = Sha256::new();
    hasher.update(bytes.as_bytes());
    hex::encode(hasher.finalize())
}

/// digest of an already-marshalled string (used for chunk keys where the input
/// is itself a Go-formatted string literal, hashed as a JSON string).
pub fn digest_string(s: &str) -> String {
    digest(&GoJson::Str(s.to_string()))
}

/// UUID-shaped point id, matching Go's `pointID`.
pub fn point_id(value: &GoJson) -> String {
    let h = digest(value);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_safe_escaping_matches_go() {
        // Go: json.Marshal("a<b>&c") => "a\u003cb\u003e\u0026c"
        let mut out = String::new();
        encode_string("a<b>&c", &mut out);
        assert_eq!(out, "\"a\\u003cb\\u003e\\u0026c\"");
    }

    #[test]
    fn digest_of_string_is_sha256_of_quoted_form() {
        // digest("x") = sha256(`"x"`)
        let d = digest_string("x");
        let mut h = Sha256::new();
        h.update(b"\"x\"");
        assert_eq!(d, hex::encode(h.finalize()));
    }

    #[test]
    fn point_id_is_uuid_shaped() {
        let id = point_id(&GoJson::s("x"));
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[4].len(), 12);
    }
}
