//! Edge secret-masking / anonymization codec (evolution section 24).
//!
//! Outbound prompts are scanned for secrets (API keys, bearer tokens, AWS
//! credentials, private-key blocks, passwords, emails, IPs) BEFORE any
//! network byte leaves the process. Each secret is replaced with a stable
//! placeholder (`«SECRET:k1»`) and the reverse mapping is held in an
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

/// Loopback/example IPs that are safe to transmit (reduce false positives).
fn is_benign_ip(s: &str) -> bool {
    s == "127.0.0.1" || s == "0.0.0.0" || s.starts_with("192.0.2.") || s.starts_with("10.0.0.")
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
            // Collect matches against the CURRENT text so successive rules
            // see prior placeholders (placeholders never re-match: they use
            // guillemets outside every pattern's alphabet).
            loop {
                let found = re.find(&out).map(|m| (m.start(), m.end()));
                let Some((s, e)) = found else { break };
                let matched = out[s..e].to_string();
                if *kind == "ipv4" && is_benign_ip(&matched) {
                    // Skip benign IPs by temporarily replacing and restoring
                    // after the loop is impossible with `find`; instead scan
                    // past this occurrence by masking nothing and using a
                    // bounded search window.
                    // Simple approach: replace with a sentinel that cannot
                    // re-match, then restore at the end.
                    out.replace_range(s..e, &matched.replace('.', "\u{2024}"));
                    continue;
                }
                let ph = if let Some(existing) = self.forward.get(&matched) {
                    existing.clone()
                } else {
                    self.counter += 1;
                    let ph = format!("\u{00AB}SECRET:{}:k{}\u{00BB}", kind, self.counter);
                    self.forward.insert(matched.clone(), ph.clone());
                    self.vault.insert(ph.clone(), matched.clone());
                    ph
                };
                spans.push(MaskedSpan { kind, placeholder: ph.clone() });
                out.replace_range(s..e, &ph);
            }
        }
        // Restore benign-IP sentinels.
        if out.contains('\u{2024}') {
            out = out.replace('\u{2024}', ".");
        }
        (out, spans)
    }

    /// Restores placeholders the model echoed back to their original values.
    pub fn unmask(&self, text: &str) -> String {
        if self.vault.is_empty() || !text.contains('\u{00AB}') {
            return text.to_string();
        }
        let mut out = text.to_string();
        for (ph, original) in &self.vault {
            if out.contains(ph.as_str()) {
                out = out.replace(ph.as_str(), original);
            }
        }
        out
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
        assert!(masked.contains("127.0.0.1"), "loopback must remain: {masked}");
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
    fn mask_prompt_roundtrip() {
        let secret = "sk-ant-abcdefghijklmnopqrstuvwxyz1234";
        let (masked, codec) = mask_prompt(&format!("call api with {secret}"));
        assert!(!masked.contains(secret));
        let model_echo = format!("Here is your config using {}", masked.split_whitespace().last().unwrap());
        let restored = codec.unmask(&model_echo);
        assert!(restored.contains(secret));
    }

    #[test]
    fn multiple_distinct_secrets_distinct_placeholders() {
        let mut c = MaskCodec::new();
        let (masked, _) = c.mask(
            "k1 sk-aaaaaaaaaaaaaaaaaaaaaaaaaa1 and k2 sk-bbbbbbbbbbbbbbbbbbbbbbbbbb2",
        );
        assert_eq!(c.vault_len(), 2);
        assert!(!masked.contains("sk-aaaa"));
        assert!(!masked.contains("sk-bbbb"));
    }
}
