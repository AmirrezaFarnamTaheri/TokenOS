//! Tiered Verification Budget:
//!
//!   static checks → first   (free, local)
//!   targeted tests → second (cheap, local)
//!   LLM verifier   → last resort (expensive, upstream)
//!
//! Verification cost stays proportional to expected failure cost: only the
//! most likely failure mode is checked, never the whole world.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Result of a verification pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResult {
    pub pass: bool,
    pub tier: String, // static | tests | llm
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<String>,
    /// tokens spent on verification (0 for local tiers)
    pub cost_tokens: usize,
}

/// Free, local AST-lite differential pass: bracket/paren/brace balance,
/// suspicious diff structure for PATCH routes, single-question contract for
/// ASK routes, and obviously truncated output.
pub fn static_check(route: &str, output: &str) -> VerifyResult {
    let mut issues = Vec::new();

    if output.trim().is_empty() {
        issues.push("empty output".to_string());
        return VerifyResult {
            pass: false,
            tier: "static".into(),
            issues,
            cost_tokens: 0,
        };
    }

    if route == "PATCH" && !looks_like_diff(output) {
        issues.push("PATCH route output is not a unified diff".to_string());
    }

    if route == "ASK" {
        let qs = output.matches('?').count();
        if qs == 0 {
            issues.push("ASK route output contains no question".to_string());
        } else if questions_count(output) > 1 {
            issues.push("ASK route must contain exactly one question".to_string());
        }
    }

    let d = brace_balance(output);
    if d != 0 && looks_like_code(output) {
        issues.push(format!(
            "unbalanced braces (delta {d:+}) — possible truncation"
        ));
    }

    if (route == "IMPLEMENT" || route == "PATCH") && has_placeholders(output) {
        issues.push("output contains placeholder text".to_string());
    }

    if output.trim_end().ends_with("...") {
        issues.push("output appears truncated (trailing ellipsis)".to_string());
    }

    VerifyResult {
        pass: issues.is_empty(),
        tier: "static".into(),
        issues,
        cost_tokens: 0,
    }
}

/// Tiered verification entry point (F-12). Checks static rules first, then runs
/// a configured local verification command on code outputs if provided.
pub fn verify_output(
    route: &str,
    output: &str,
    test_command: &str,
    route_commands: &std::collections::HashMap<String, String>,
) -> VerifyResult {
    let mut res = static_check(route, output);
    if !res.pass {
        return res;
    }

    let cmd = route_commands
        .get(route)
        .map(|s| s.as_str())
        .unwrap_or(test_command);

    if !cmd.is_empty() {
        let cmd_res = if cfg!(target_os = "windows") {
            std::process::Command::new("powershell")
                .args(["-Command", cmd])
                .output()
        } else {
            std::process::Command::new("sh").args(["-c", cmd]).output()
        };

        match cmd_res {
            Ok(output_cmd) => {
                if output_cmd.status.success() {
                    res.tier = "tests".into();
                } else {
                    res.pass = false;
                    res.tier = "tests".into();
                    let err_msg = String::from_utf8_lossy(&output_cmd.stderr)
                        .trim()
                        .to_string();
                    let out_msg = String::from_utf8_lossy(&output_cmd.stdout)
                        .trim()
                        .to_string();
                    res.issues.push(format!(
                        "Verification command failed. stdout: {}, stderr: {}",
                        out_msg, err_msg
                    ));
                }
            }
            Err(e) => {
                res.pass = false;
                res.tier = "tests".into();
                res.issues
                    .push(format!("Failed to execute verification command: {}", e));
            }
        }
    }

    res
}

fn looks_like_diff(s: &str) -> bool {
    let t = s.trim();
    t.starts_with("--- ") || t.starts_with("diff ") || t.contains("\n--- ") || t.starts_with("@@")
}

static RE_QUESTION_LINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)\?\s*$").unwrap());

fn questions_count(s: &str) -> usize {
    RE_QUESTION_LINE.find_iter(s).count()
}

fn has_placeholders(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower.contains("insert code here")
        || lower.contains("your code here")
        || lower.contains("rest of the code")
        || lower.contains("rest of code")
        || lower.contains("remains unchanged")
        || lower.contains("implementation goes here")
        || lower.contains("implement remaining")
        || lower.contains("// todo: implement")
        || lower.contains("# todo: implement")
        || lower.contains("/* todo: implement")
}

/// Net {}/()/[] depth, ignoring string literals and line comments (a cheap
/// approximation of an AST balance check).
fn brace_balance(s: &str) -> i64 {
    let bytes = s.as_bytes();
    let mut depth: i64 = 0;
    let mut in_str: u8 = 0;
    let mut esc = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str != 0 {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == in_str {
                in_str = 0;
            }
            i += 1;
            continue;
        }
        if esc {
            esc = false;
            i += 1;
            continue;
        }
        if c == b'\\' {
            esc = true;
            i += 1;
            continue;
        }
        if c == b'"' || c == b'\'' || c == b'`' {
            in_str = c;
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        match c {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    depth
}

fn looks_like_code(s: &str) -> bool {
    s.contains("fn ")
        || s.contains("let ")
        || s.contains("import ")
        || s.contains("const ")
        || s.contains("class ")
        || s.contains("def ")
        || s.contains("struct ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_fails() {
        assert!(!static_check("IMPLEMENT", "   ").pass);
    }

    #[test]
    fn patch_requires_diff() {
        assert!(!static_check("PATCH", "just some code").pass);
        assert!(static_check("PATCH", "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-x\n+y").pass);
    }

    #[test]
    fn ask_single_question() {
        assert!(static_check("ASK", "Which database should be used?").pass);
        assert!(!static_check("ASK", "Which db?\nAnd which port?").pass);
        assert!(!static_check("ASK", "no question here").pass);
    }

    #[test]
    fn unbalanced_braces_in_code_fail() {
        let code = "fn main() {\n  let x = 1;\n"; // missing }
        assert!(!static_check("IMPLEMENT", code).pass);
    }

    #[test]
    fn braces_in_strings_ignored() {
        let code = "fn main() {\n  let s = \"{{{\";\n}";
        assert!(static_check("IMPLEMENT", code).pass);
    }

    #[test]
    fn braces_in_line_comments_ignored() {
        let code = "fn main() {\n  // unbalanced { in comment\n}";
        assert!(static_check("IMPLEMENT", code).pass);
    }

    #[test]
    fn trailing_ellipsis_fails() {
        assert!(!static_check("IMPLEMENT", "result body ...").pass);
    }

    #[test]
    fn placeholders_fail_implement_and_patch() {
        assert!(
            !static_check(
                "IMPLEMENT",
                "fn main() {\n  // TODO: implement remaining functions\n}"
            )
            .pass
        );
        assert!(
            !static_check(
                "PATCH",
                "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-x\n+// your code here"
            )
            .pass
        );
        assert!(static_check("DIRECT", "TODO: implement this later").pass);
    }

    #[test]
    fn prose_with_parens_passes() {
        assert!(static_check("DIRECT", "Done (see notes).").pass);
    }
}
