//! Edge secret-masking / anonymization codec (evolution section 24).
//!
//! Outbound prompts are scanned for secrets (API keys, bearer tokens, AWS
//! credentials, private-key blocks, passwords, emails, IPs) BEFORE any
//! network byte leaves the process. Each secret is replaced with a stable,
//! OPAQUE placeholder (`«SECRET:k1»` — the secret class never crosses the
//! wire) and the reverse mapping is held in an
//! in-process vault that never persists and never crosses the wire. Inbound
//! responses are passed back through the codec so placeholders the model
//! echoed are restored to the original values.
//!
//! Properties:
//!   * deterministic: the same secret maps to the same placeholder within a
//!     codec instance, so multi-mention prompts stay coherent
//!   * lossless: unmask(mask(x)) == x for every detected span
//!   * fail-closed ordering: longer/multi-line patterns run first so a
//!     private-key block is masked whole rather than per-line

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

/// One detected and masked secret span.
#[derive(Debug, Clone)]
pub struct MaskedSpan {
    pub kind: &'static str,
    pub placeholder: String,
}

/// Detection rules, ordered most-specific → least-specific. Each pattern is
/// linear-time (the regex crate compiles to finite automata; no backtracking).
static RULES: Lazy<Vec<(&'static str, Regex)>> = Lazy::new(|| {
    vec![
        (
            "private_key",
            Regex::new(
                r"-----BEGIN [A-Z ]*PRIVATE KEY-----[A-Za-z0-9+/=\s]+-----END [A-Z ]*PRIVATE KEY-----",
            )
            .unwrap(),
        ),
        ("openai_key", Regex::new(r"\bsk-[A-Za-z0-9_-]{20,}\b").unwrap()),
        ("anthropic_key", Regex::new(r"\bsk-ant-[A-Za-z0-9_-]{20,}\b").unwrap()),
        ("github_token", Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b").unwrap()),
        ("aws_access_key", Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap()),
        (
            "aws_secret_key",
            Regex::new(r#"(?i)\baws_secret_access_key\s*[=:]\s*['"]?([A-Za-z0-9/+=]{40})['"]?"#)
                .unwrap(),
        ),
        ("google_key", Regex::new(r"\bAIza[0-9A-Za-z_-]{30,}\b").unwrap()),
        ("slack_token", Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b").unwrap()),
        (
            "jwt",
            Regex::new(r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b")
                .unwrap(),
        ),
        (
            "bearer_header",
            Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/-]{16,}=*").unwrap(),
        ),
        (
            "password_assignment",
            Regex::new(r#"(?i)\b(password|passwd|pwd|secret|api[_-]?key|token)\s*[=:]\s*['"]([^'"\s]{8,})['"]"#)
                .unwrap(),
        ),
        (
            "connection_string",
            Regex::new(r"\b[a-z][a-z0-9+]*://[^/\s:@]+:[^@\s]+@[^\s]+").unwrap(),
        ),
        (
            "email",
            Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b").unwrap(),
        ),
        (
            "ipv4",
            Regex::new(r"\b(?:(?:25[0-5]|2[0-4][0-9]|1?[0-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1?[0-9]?[0-9])\b")
                .unwrap(),
        ),
    ]
});

/// Loopback/documentation IPs that are safe to transmit (reduce false
/// positives). Private-network addresses (10.x, 172.16.x, 192.168.x) are
/// deliberately NOT here — internal topology is exactly what the codec
/// exists to keep off the wire.
fn is_benign_ip(s: &str) -> bool {
    s == "127.0.0.1" || s == "0.0.0.0" || s.starts_with("192.0.2.")
}

/// Rules whose regex captures the secret VALUE in a group: only the value is
/// masked so the model keeps the assignment context (`password = «SECRET:k1»`)
/// instead of losing the whole statement.
fn value_group(kind: &str) -> Option<usize> {
    match kind {
        "aws_secret_key" => Some(1),
        "password_assignment" => Some(2),
        _ => None,
    }
}

/// The codec instance: holds the reverse vault. One codec per outbound
/// request; the vault dies with it. Not serializable by design.
#[derive(Debug, Default)]
pub struct MaskCodec {
    /// placeholder → original
    vault: HashMap<String, String>,
    /// original → placeholder (stable placeholder reuse)
    forward: HashMap<String, String>,
    counter: usize,
}

impl MaskCodec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Masks every detected secret in `text`, returning the redacted text
    /// and the list of spans masked.
    pub fn mask(&mut self, text: &str) -> (String, Vec<MaskedSpan>) {
        let mut out = text.to_string();
        let mut spans = Vec::new();
        for (kind, re) in RULES.iter() {
            // Offset-based scan: `pos` advances past every decision so a
            // benign IP is simply skipped in place (no sentinel rewriting)
            // and a freshly inserted placeholder is never re-examined.
            // Value-group rules leave the assignment context intact, so
            // advancing `pos` past the whole match guarantees termination.
            let mut pos = 0usize;
            while pos <= out.len() {
                let (s, e, whole_end) = {
                    let caps = match re.captures_at(&out, pos) {
                        Some(c) => c,
                        None => break,
                    };
                    let whole = caps.get(0).expect("group 0 always present");
                    match value_group(kind).and_then(|g| caps.get(g)) {
                        Some(m) => (m.start(), m.end(), whole.end()),
                        None => (whole.start(), whole.end(), whole.end()),
                    }
                };
                let matched = out[s..e].to_string();
                if *kind == "ipv4" && is_benign_ip(&matched) {
                    pos = whole_end.max(pos + 1);
                    continue;
                }
                let ph = if let Some(existing) = self.forward.get(&matched) {
                    existing.clone()
                } else {
                    self.counter += 1;
                    // Opaque on the wire: the placeholder carries an index
                    // only — leaking the secret CLASS ("openai_key", …) to
                    // the remote provider would itself be signal. The kind
                    // stays local in the returned spans.
                    let ph = format!("\u{00AB}SECRET:k{}\u{00BB}", self.counter);
                    self.forward.insert(matched.clone(), ph.clone());
                    self.vault.insert(ph.clone(), matched.clone());
                    ph
                };
                spans.push(MaskedSpan {
                    kind,
                    placeholder: ph.clone(),
                });
                out.replace_range(s..e, &ph);
                pos = s + ph.len();
            }
        }
        (out, spans)
    }

    /// Restores placeholders the model echoed back to their original values.
    pub fn unmask(&self, text: &str) -> String {
        if self.vault.is_empty() || !text.contains('\u{00AB}') {
            return text.to_string();
        }

        static PLACEHOLDER_RE: Lazy<Regex> =
            Lazy::new(|| Regex::new(r"\u{00AB}SECRET:k\d+\u{00BB}").unwrap());

        PLACEHOLDER_RE
            .replace_all(text, |caps: &regex::Captures| {
                let ph = caps.get(0).unwrap().as_str();
                if let Some(original) = self.vault.get(ph) {
                    original.clone()
                } else {
                    ph.to_string()
                }
            })
            .into_owned()
    }

    /// Number of distinct secrets currently held in the vault.
    pub fn vault_len(&self) -> usize {
        self.vault.len()
    }
}

/// One-shot convenience used by the engine: mask a prompt, returning the
/// redacted text plus the codec for the response leg.
pub fn mask_prompt(prompt: &str) -> (String, MaskCodec) {
    let mut codec = MaskCodec::new();
    let (masked, _) = codec.mask(prompt);
    (masked, codec)
}

/// True when text still contains an opaque placeholder emitted by this codec.
/// Durable caches use this as a replay guard: a placeholder is safe at rest but
/// not useful as a later user-facing answer because the reverse vault dies at
/// request end.
pub fn contains_placeholder(text: &str) -> bool {
    text.contains("\u{00AB}SECRET:k")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_key_is_masked_and_restored() {
        let mut c = MaskCodec::new();
        let secret = "sk-abcdefghijklmnopqrstuvwxyz123456";
        let (masked, spans) = c.mask(&format!("use key {secret} for auth"));
        assert!(!masked.contains(secret));
        assert_eq!(spans.len(), 1);
        assert_eq!(c.unmask(&masked), format!("use key {secret} for auth"));
    }

    #[test]
    fn same_secret_gets_same_placeholder() {
        let mut c = MaskCodec::new();
        let secret = "ghp_ABCDEFGHIJKLMNOPQRSTuvwx12345678";
        let (masked, _) = c.mask(&format!("{secret} and again {secret}"));
        assert!(!masked.contains(secret));
        assert_eq!(c.vault_len(), 1);
        let ph: Vec<&str> = masked.split(" and again ").collect();
        assert_eq!(ph[0], ph[1]);
    }

    #[test]
    fn private_key_block_masked_whole() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA7\nqrstuvwx\n-----END RSA PRIVATE KEY-----";
        let mut c = MaskCodec::new();
        let (masked, spans) = c.mask(pem);
        assert!(!masked.contains("MIIEpAIBAAKCAQEA7"));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, "private_key");
        assert_eq!(c.unmask(&masked), pem);
    }

    #[test]
    fn aws_and_bearer_masked() {
        let mut c = MaskCodec::new();
        let text = "creds AKIAIOSFODNN7EXAMPLE with Bearer abcdefghijklmnopqrstuvwxyz";
        let (masked, _) = c.mask(text);
        assert!(!masked.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!masked.to_lowercase().contains("bearer abcdefgh"));
        assert_eq!(c.unmask(&masked), text);
    }

    #[test]
    fn password_assignment_masked() {
        let mut c = MaskCodec::new();
        let text = r#"set password = "hunter2hunter2" in the env"#;
        let (masked, _) = c.mask(text);
        assert!(!masked.contains("hunter2hunter2"));
        assert_eq!(c.unmask(&masked), text);
    }

    #[test]
    fn connection_string_masked() {
        let mut c = MaskCodec::new();
        let text = "db at postgres://admin:s3cr3tpass@db.internal:5432/prod";
        let (masked, _) = c.mask(text);
        assert!(!masked.contains("s3cr3tpass"));
        assert_eq!(c.unmask(&masked), text);
    }

    #[test]
    fn email_and_ip_masked_but_loopback_kept() {
        let mut c = MaskCodec::new();
        let text = "mail alice@example.com from 203.0.113.99 via 127.0.0.1";
        let (masked, _) = c.mask(text);
        assert!(!masked.contains("alice@example.com"));
        assert!(!masked.contains("203.0.113.99"));
        assert!(
            masked.contains("127.0.0.1"),
            "loopback must remain: {masked}"
        );
        assert_eq!(c.unmask(&masked), text);
    }

    #[test]
    fn jwt_masked() {
        let mut c = MaskCodec::new();
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJVadQssw5c";
        let (masked, spans) = c.mask(&format!("token: {jwt}"));
        assert!(!masked.contains(jwt));
        assert_eq!(spans[0].kind, "jwt");
    }

    #[test]
    fn clean_text_unchanged() {
        let mut c = MaskCodec::new();
        let text = "rename variable x to y in main.rs";
        let (masked, spans) = c.mask(text);
        assert_eq!(masked, text);
        assert!(spans.is_empty());
        assert_eq!(c.vault_len(), 0);
    }

    #[test]
    fn unmask_without_placeholders_is_identity() {
        let c = MaskCodec::new();
        assert_eq!(c.unmask("plain output"), "plain output");
    }

    #[test]
    fn preexisting_sentinel_char_survives_masking() {
        // Input legitimately containing U+2024 (ONE DOT LEADER) must never
        // be corrupted; the offset-based scan touches nothing it does not
        // mask, and benign IPs are skipped in place.
        let mut c = MaskCodec::new();
        let text = "literal one-dot-leader: a\u{2024}b near 127.0.0.1";
        let (masked, _) = c.mask(text);
        assert!(
            masked.contains("a\u{2024}b"),
            "U+2024 must survive: {masked}"
        );
        assert!(masked.contains("127.0.0.1"), "loopback stays: {masked}");
        assert_eq!(c.unmask(&masked), text);
    }

    #[test]
    fn private_network_ips_are_masked() {
        // 10.x is internal topology, NOT a documentation range — it must
        // be masked like any other address.
        let mut c = MaskCodec::new();
        let text = "db lives at 10.0.0.7 behind the LB";
        let (masked, _) = c.mask(text);
        assert!(
            !masked.contains("10.0.0.7"),
            "private IP must mask: {masked}"
        );
        assert_eq!(c.unmask(&masked), text);
    }

    #[test]
    fn placeholders_are_opaque_no_kind_on_wire() {
        let mut c = MaskCodec::new();
        let (masked, spans) = c.mask("key sk-abcdefghijklmnopqrstuvwxyz123456");
        assert!(
            !masked.contains("openai_key"),
            "secret class must not cross the wire: {masked}"
        );
        // The kind is still available locally for telemetry.
        assert_eq!(spans[0].kind, "openai_key");
    }

    #[test]
    fn password_assignment_masks_value_only() {
        let mut c = MaskCodec::new();
        let text = r#"set password = "hunter2hunter2" in the env"#;
        let (masked, _) = c.mask(text);
        assert!(!masked.contains("hunter2hunter2"));
        // The assignment context survives so the model understands WHAT is
        // being configured — only the value is redacted.
        assert!(
            masked.contains("password"),
            "context must survive: {masked}"
        );
        assert_eq!(c.unmask(&masked), text);
    }

    #[test]
    fn mask_prompt_roundtrip() {
        let secret = "sk-ant-abcdefghijklmnopqrstuvwxyz1234";
        let (masked, codec) = mask_prompt(&format!("call api with {secret}"));
        assert!(!masked.contains(secret));
        let model_echo = format!(
            "Here is your config using {}",
            masked.split_whitespace().last().unwrap()
        );
        let restored = codec.unmask(&model_echo);
        assert!(restored.contains(secret));
    }

    #[test]
    fn multiple_distinct_secrets_distinct_placeholders() {
        let mut c = MaskCodec::new();
        let (masked, _) =
            c.mask("k1 sk-aaaaaaaaaaaaaaaaaaaaaaaaaa1 and k2 sk-bbbbbbbbbbbbbbbbbbbbbbbbbb2");
        assert_eq!(c.vault_len(), 2);
        assert!(!masked.contains("sk-aaaa"));
        assert!(!masked.contains("sk-bbbb"));
    }

    #[test]
    fn cascading_placeholder_unmasks_correctly() {
        let mut c = MaskCodec::new();
        // The value of a secret literally contains the placeholder format of another secret.
        let secret1 = "10.0.0.1"; // masks to «SECRET:k1»
        let secret2 = "password = \"\u{00AB}SECRET:k1\u{00BB}\""; // contains «SECRET:k1», masks value to «SECRET:k2»

        let text = format!("ip is {secret1} and {secret2}");
        let (masked, _) = c.mask(&text);

        // Single-pass replacement prevents double-replacement corruption.
        assert_eq!(
            c.unmask(&masked),
            "ip is 10.0.0.1 and password = \"\u{00AB}SECRET:k1\u{00BB}\""
        );
    }

    #[test]
    fn many_secrets_unmasks_correctly() {
        let mut c = MaskCodec::new();
        let mut text = String::new();
        let mut expected = String::new();
        for i in 1..=20 {
            // Generate 20 distinct openai keys
            let key = format!("sk-abcdefghijklmnopqrstuvwxyz{:06}", i);
            text.push_str(&format!("key{}: {}, ", i, key));
            expected.push_str(&format!("key{}: {}, ", i, key));
        }
        let (masked, _) = c.mask(&text);
        assert_eq!(c.unmask(&masked), expected);
    }
}
