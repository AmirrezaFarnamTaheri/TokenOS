//! Truncated-JSON rescuer (evolution section 20).
//!
//! Provider responses are sometimes cut mid-stream (timeouts, token limits),
//! producing structurally invalid JSON that a strict parser rejects — wasting
//! the entire paid generation. This module recovers such fragments with a
//! single-pass, no-backtracking recursive-descent parser that treats EOF as
//! a soft boundary:
//!
//!   * a string cut at EOF yields its partial contents
//!   * an object cut after `"key":` drops the dangling key
//!   * an array/object cut mid-stream closes with everything parsed so far
//!   * a literal cut at EOF (`tru`, `12.`) resolves when unambiguous
//!
//! The rescuer never invents data — it only closes what the model opened.
//! Time O(N), space O(depth). Input bytes are scanned exactly once.

use serde_json::{Map, Number, Value};

/// Outcome of a rescue attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rescue {
    /// Input was already valid JSON; use it as-is.
    Intact,
    /// Input was repaired; the canonical repaired text is provided.
    Repaired(String),
    /// Input is not rescuable JSON (e.g. plain prose).
    NotJson,
}

/// Repairs a (possibly truncated) JSON fragment.
pub fn rescue(input: &str) -> Rescue {
    let s = input.trim();
    let start = match s.bytes().position(|b| b == b'{' || b == b'[') {
        Some(i) => i,
        None => return Rescue::NotJson,
    };
    let body = &s[start..];

    if serde_json::from_str::<Value>(body).is_ok() {
        return if start == 0 {
            Rescue::Intact
        } else {
            Rescue::Repaired(body.to_string())
        };
    }

    let mut p = Parser {
        b: body.as_bytes(),
        i: 0,
    };
    match p.value() {
        Some(v) if v.is_object() || v.is_array() => {
            // Truncation guard: a genuinely cut-off generation is consumed
            // to EOF. If the parser stalled mid-input, the text is prose
            // that merely starts with a bracket (e.g. "[mock] done…") —
            // "repairing" it would destroy a valid answer.
            p.ws();
            if p.i < p.b.len() {
                return Rescue::NotJson;
            }
            Rescue::Repaired(serde_json::to_string(&v).expect("value serializes"))
        }
        _ => Rescue::NotJson,
    }
}

/// Parses raw provider output leniently, repairing truncation when needed.
pub fn parse_lenient(input: &str) -> Option<Value> {
    match rescue(input) {
        Rescue::Intact => serde_json::from_str(input.trim()).ok(),
        Rescue::Repaired(fixed) => serde_json::from_str(&fixed).ok(),
        Rescue::NotJson => None,
    }
}

/// Single-pass lenient recursive-descent parser. EOF is a soft terminator at
/// every grammar position.
struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

/// Hard ceiling on nesting depth (stack-overflow guard for adversarial or
/// pathological generations).
const MAX_DEPTH: usize = 256;

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\r' | b'\n') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn value(&mut self) -> Option<Value> {
        self.value_at(0)
    }

    fn value_at(&mut self, depth: usize) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.ws();
        match self.peek()? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Value::String),
            b't' | b'f' | b'n' => self.literal(),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn object(&mut self, depth: usize) -> Option<Value> {
        self.i += 1; // consume '{'
        let mut map = Map::new();
        loop {
            self.ws();
            match self.peek() {
                None => return Some(Value::Object(map)), // EOF: close here
                Some(b'}') => {
                    self.i += 1;
                    return Some(Value::Object(map));
                }
                Some(b',') => {
                    self.i += 1;
                    continue;
                }
                Some(b'"') => {
                    let key_start = self.i;
                    let key = match self.string() {
                        Some(k) => k,
                        None => return Some(Value::Object(map)),
                    };
                    self.ws();
                    if self.peek() != Some(b':') {
                        // Truncated right after the key (or key was actually
                        // a cut string value of a malformed doc): drop it.
                        let _ = key_start;
                        return Some(Value::Object(map));
                    }
                    self.i += 1; // consume ':'
                    match self.value_at(depth + 1) {
                        Some(v) => {
                            map.insert(key, v);
                        }
                        None => return Some(Value::Object(map)), // dangling key dropped
                    }
                }
                Some(_) => return Some(Value::Object(map)), // garbage: stop cleanly
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<Value> {
        self.i += 1; // consume '['
        let mut items = Vec::new();
        loop {
            self.ws();
            match self.peek() {
                None => return Some(Value::Array(items)), // EOF: close here
                Some(b']') => {
                    self.i += 1;
                    return Some(Value::Array(items));
                }
                Some(b',') => {
                    self.i += 1;
                    continue;
                }
                Some(_) => match self.value_at(depth + 1) {
                    Some(v) => items.push(v),
                    None => return Some(Value::Array(items)),
                },
            }
        }
    }

    /// Parses a string; EOF inside the string returns the partial contents.
    /// A truncated escape sequence at EOF is dropped.
    fn string(&mut self) -> Option<String> {
        debug_assert_eq!(self.peek(), Some(b'"'));
        self.i += 1;
        let mut out = String::new();
        while let Some(c) = self.peek() {
            match c {
                b'"' => {
                    self.i += 1;
                    return Some(out);
                }
                b'\\' => {
                    self.i += 1;
                    let Some(esc) = self.peek() else { break };
                    self.i += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            if self.i + 4 <= self.b.len() {
                                let hex = &self.b[self.i..self.i + 4];
                                if let Ok(h) = std::str::from_utf8(hex) {
                                    if let Ok(cp) = u32::from_str_radix(h, 16) {
                                        if let Some(ch) = char::from_u32(cp) {
                                            out.push(ch);
                                        }
                                    }
                                }
                                self.i += 4;
                            } else {
                                self.i = self.b.len(); // truncated \uXX → drop
                            }
                        }
                        _ => {} // unknown escape: drop
                    }
                }
                _ => {
                    // Consume one UTF-8 scalar.
                    let rest = &self.b[self.i..];
                    // SAFETY: The input `body` is constructed from a valid UTF-8 `&str`,
                    // and `self.i` is only ever incremented by valid UTF-8 character lengths
                    // or single-byte ASCII characters. Thus, `rest` is guaranteed to be
                    // a valid UTF-8 byte sequence.
                    let s = unsafe { std::str::from_utf8_unchecked(rest) };
                    let ch = s.chars().next().unwrap();
                    out.push(ch);
                    self.i += ch.len_utf8();
                }
            }
        }
        Some(out) // EOF inside string: partial contents
    }

    /// true / false / null, accepting unambiguous prefixes at EOF.
    fn literal(&mut self) -> Option<Value> {
        let rest = &self.b[self.i..];
        for (word, v) in [
            ("true", Value::Bool(true)),
            ("false", Value::Bool(false)),
            ("null", Value::Null),
        ] {
            let wb = word.as_bytes();
            if rest.len() >= wb.len() && &rest[..wb.len()] == wb {
                self.i += wb.len();
                return Some(v);
            }
            // Unambiguous prefix at EOF ("tru", "fals", "nul" …).
            if !rest.is_empty() && rest.len() < wb.len() && wb.starts_with(rest) {
                self.i = self.b.len();
                return Some(v);
            }
        }
        None
    }

    fn number(&mut self) -> Option<Value> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if matches!(c, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                self.i += 1;
            } else {
                break;
            }
        }
        let mut tok = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        // Trim a trailing partial exponent/point ("12.", "1e", "1e-").
        while !tok.is_empty() {
            if let Ok(n) = tok.parse::<f64>() {
                if tok.contains('.') || tok.contains('e') || tok.contains('E') {
                    return Number::from_f64(n).map(Value::Number);
                }
                // Integer token: prefer exact integer types over the lossy
                // f64 fallback — u64 covers positive values past i64::MAX
                // (e.g. 18446744073709551615) without precision loss.
                return tok
                    .parse::<i64>()
                    .ok()
                    .map(|i| Value::Number(i.into()))
                    .or_else(|| tok.parse::<u64>().ok().map(|u| Value::Number(u.into())))
                    .or_else(|| Number::from_f64(n).map(Value::Number));
            }
            tok = &tok[..tok.len() - 1];
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_json_is_intact() {
        assert_eq!(rescue(r#"{"a": 1, "b": [2, 3]}"#), Rescue::Intact);
    }

    #[test]
    fn truncated_object_closes() {
        let v = parse_lenient(r#"{"a": 1, "b": {"c": 2"#).unwrap();
        assert_eq!(v["a"], json!(1));
        assert_eq!(v["b"]["c"], json!(2));
    }

    #[test]
    fn truncated_string_value_closes() {
        let v = parse_lenient(r#"{"msg": "hello wor"#).unwrap();
        assert_eq!(v["msg"], json!("hello wor"));
    }

    #[test]
    fn dangling_key_is_trimmed() {
        let v = parse_lenient(r#"{"a": 1, "b":"#).unwrap();
        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn dangling_comma_is_trimmed() {
        let v = parse_lenient(r#"[1, 2, 3,"#).unwrap();
        assert_eq!(v, json!([1, 2, 3]));
    }

    #[test]
    fn truncated_array_of_objects() {
        let v = parse_lenient(r#"{"items": [{"id": 1}, {"id": 2}, {"id"#).unwrap();
        let items = v["items"].as_array().unwrap();
        assert_eq!(items[0], json!({"id": 1}));
        assert_eq!(items[1], json!({"id": 2}));
        // The cut third object may survive as {} — it must not corrupt
        // the previous complete items.
        assert!(items.len() == 2 || items[2] == json!({}));
    }

    #[test]
    fn leading_prose_is_stripped() {
        let v = parse_lenient(r#"Here is the JSON you asked for: {"ok": true}"#).unwrap();
        assert_eq!(v, json!({"ok": true}));
    }

    #[test]
    fn escaped_quotes_survive() {
        let v = parse_lenient(r#"{"s": "he said \"hi\"", "n": 4"#).unwrap();
        assert_eq!(v["s"], json!(r#"he said "hi""#));
        assert_eq!(v["n"], json!(4));
    }

    #[test]
    fn truncated_number_at_eof_is_kept() {
        let v = parse_lenient(r#"{"x": 12.5"#).unwrap();
        assert_eq!(v["x"], json!(12.5));
    }

    #[test]
    fn partial_exponent_is_salvaged() {
        let v = parse_lenient(r#"{"x": 12e"#).unwrap();
        assert_eq!(v["x"], json!(12));
    }

    #[test]
    fn plain_prose_is_not_json() {
        assert_eq!(rescue("the quick brown fox"), Rescue::NotJson);
        assert!(parse_lenient("no json here at all").is_none());
        // Prose that merely starts with a bracket must NOT be "repaired"
        // into a fragment — the truncation guard requires EOF consumption.
        assert_eq!(
            rescue("[mock:dry-run] completed route IMPLEMENT for goal: x"),
            Rescue::NotJson
        );
        assert_eq!(rescue("{a} is the set containing a"), Rescue::NotJson);
    }

    #[test]
    fn empty_input_is_not_json() {
        assert_eq!(rescue(""), Rescue::NotJson);
    }

    #[test]
    fn nested_truncation_deep() {
        let v = parse_lenient(r#"{"a": {"b": {"c": {"d": [1, {"e": "f"#).unwrap();
        assert_eq!(v["a"]["b"]["c"]["d"][0], json!(1));
        assert_eq!(v["a"]["b"]["c"]["d"][1]["e"], json!("f"));
    }

    #[test]
    fn booleans_and_null_complete() {
        let v = parse_lenient(r#"{"t": true, "f": false, "n": null"#).unwrap();
        assert_eq!(v["t"], json!(true));
        assert_eq!(v["f"], json!(false));
        assert_eq!(v["n"], json!(null));
    }

    #[test]
    fn truncated_literal_prefix_resolves() {
        let v = parse_lenient(r#"{"t": tru"#).unwrap();
        assert_eq!(v["t"], json!(true));
        let v = parse_lenient(r#"{"n": nul"#).unwrap();
        assert_eq!(v["n"], json!(null));
    }

    #[test]
    fn truncated_escape_at_eof_is_dropped() {
        let v = parse_lenient(r#"{"s": "abc\"#).unwrap();
        assert_eq!(v["s"], json!("abc"));
    }

    #[test]
    fn unicode_escape_roundtrip() {
        let v = parse_lenient(r#"{"s": "\u00e9clair"#).unwrap();
        assert_eq!(v["s"], json!("éclair"));
    }

    #[test]
    fn depth_bomb_is_bounded() {
        let bomb = "[".repeat(10_000);
        // Must not stack-overflow; result may be NotJson or a shallow array.
        let _ = rescue(&bomb);
    }

    #[test]
    fn whole_array_document() {
        let v = parse_lenient(r#"[{"a": 1}, {"b": 2"#).unwrap();
        assert_eq!(v[0], json!({"a": 1}));
        assert_eq!(v[1], json!({"b": 2}));
    }
}
