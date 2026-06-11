//! Serializes kernel state into provider prompts using the JIT Prompt
//! Caching Alignment Strategy: a strict static→dynamic ordering so
//! provider-side prefix caches stay hot across turns.
//!
//! ```text
//! ┌──────────────────────────────────────────────┐
//! │ STATIC      kernel contract (never changes)  │ → high cache hit
//! ├──────────────────────────────────────────────┤
//! │ SEMI-STATIC constraints, architecture notes  │ → moderate cache hit
//! ├──────────────────────────────────────────────┤
//! │ DYNAMIC     state, context, failures, action │ → appended last
//! └──────────────────────────────────────────────┘
//! ```

use crate::kernel::{DelegationPacket, Route, State};
use std::fmt::Write as _;

/// The tiny worker contract. Workers are not smart; the orchestration layer
/// is. This block must remain byte-stable to maximize provider prefix-cache
/// hits.
pub const KERNEL_CONTRACT: &str = "You are a token-optimal execution worker.
Rules:
1. Output the finished result only. No preamble, no commentary, no apologies.
2. For PATCH routes output a unified diff only.
3. For ASK routes output exactly one question.
4. For PARTIAL routes output completed work, then a line \"BLOCKERS:\" listing blockers.
5. Stop at acceptance. No optional refactoring, no extra enhancements.
6. Never restate the task or your reasoning.";

/// Produces the final prompt for a given route and state, with static content
/// first and volatile content last.
pub fn build(route: Route, st: &State) -> String {
    // DELEGATE routes transmit the minimal contract — conclusions only, no
    // history, no reasoning — serialized as a compact DelegationPacket.
    if route == Route::Delegate {
        return build_delegation(st);
    }

    let mut b = String::with_capacity(1024);

    // --- STATIC BLOCK ---
    b.push_str(KERNEL_CONTRACT);
    b.push_str("\n\n");

    // --- SEMI-STATIC BLOCK ---
    if !st.constraints.is_empty() {
        b.push_str("CONSTRAINTS:\n");
        for c in &st.constraints {
            b.push_str("- ");
            b.push_str(c);
            b.push('\n');
        }
        b.push('\n');
    }

    // --- DYNAMIC BLOCK (always last; never breaks the prefix above) ---
    let _ = writeln!(b, "ROUTE: {route}");
    let _ = writeln!(b, "GOAL: {}", st.goal);
    if !st.context.is_empty() {
        b.push_str("CONTEXT (minimum viable):\n");
        b.push_str(&st.context);
        if !st.context.ends_with('\n') {
            b.push('\n');
        }
    }
    if !st.failures.is_empty() {
        b.push_str("FAILURE MEMORY (do not repeat):\n");
        for f in &st.failures {
            let _ = writeln!(b, "- failed: {} | reason: {}", f.action, f.reason);
        }
    }
    if !st.next_action.is_empty() {
        let _ = writeln!(b, "NEXT ACTION: {}", st.next_action);
    }
    b
}

/// Serializes the DELEGATE prompt around a `DelegationPacket`: the static
/// kernel contract leads (cache-aligned), then the packet as compact JSON.
/// State is preferred over summaries; the packet carries conclusions only.
fn build_delegation(st: &State) -> String {
    let packet = DelegationPacket {
        task: st.goal.clone(),
        scope: if st.context.is_empty() {
            "self-contained; no external context required".to_string()
        } else {
            st.context.clone()
        },
        constraints: st.constraints.clone(),
        acceptance: "output satisfies the task exactly; no scope expansion".to_string(),
        next_step: if st.next_action.is_empty() {
            "complete the delegated work and stop".to_string()
        } else {
            st.next_action.clone()
        },
    };
    let mut b = String::with_capacity(1024);
    b.push_str(KERNEL_CONTRACT);
    b.push_str("\n\nROUTE: DELEGATE\nDELEGATION PACKET (complete contract; no further context will follow):\n");
    b.push_str(&serde_json::to_string(&packet).unwrap_or_else(|_| st.goal.clone()));
    b.push('\n');
    if !st.failures.is_empty() {
        b.push_str("FAILURE MEMORY (do not repeat):\n");
        for f in &st.failures {
            let _ = writeln!(b, "- failed: {} | reason: {}", f.action, f.reason);
        }
    }
    b
}

/// Applies the strict output contract: prioritize Markdown fence extraction
/// first (structural, lossless); only fall back to conversational-filler
/// stripping when no fence is present. This ordering prevents destructive
/// truncation of valid code that legitimately starts with a filler-looking
/// word (audit finding 12.4).
pub fn extract_solution(raw: &str) -> String {
    let s = raw.trim();

    // 1. Structural pass: unwrap a single outer code fence if present.
    if let Some(body) = unwrap_fence(s) {
        return body;
    }

    // 2. Heuristic fallback: drop a leading "Sure"/"Here is"-style filler
    //    line, then retry fence extraction on the remainder.
    if let Some(i) = s.find('\n') {
        let first = s[..i].to_lowercase();
        const FILLERS: [&str; 5] = ["sure", "here is", "here's", "certainly", "of course"];
        if FILLERS.iter().any(|f| first.starts_with(f)) {
            let rest = s[i + 1..].trim();
            if let Some(body) = unwrap_fence(rest) {
                return body;
            }
            return rest.to_string();
        }
    }
    s.to_string()
}

/// Unwraps a single outer ``` fence; returns None when the text is not a
/// fenced block.
fn unwrap_fence(s: &str) -> Option<String> {
    if !s.starts_with("```") {
        return None;
    }
    let end = s.rfind("```")?;
    if end <= 3 {
        return None;
    }
    let nl = s.find('\n')?;
    if nl < end {
        Some(s[nl + 1..end].trim_end_matches('\n').to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::State;

    #[test]
    fn static_block_leads() {
        let st = State::new("t", "do the thing");
        let p = build(Route::Implement, &st);
        assert!(p.starts_with(KERNEL_CONTRACT));
        assert!(p.contains("GOAL: do the thing"));
    }

    #[test]
    fn dynamic_after_static() {
        let mut st = State::new("t", "g");
        st.constraints.push("no API changes".into());
        st.remember_failure("x", "y");
        let p = build(Route::Patch, &st);
        let contract_end = KERNEL_CONTRACT.len();
        let con = p.find("CONSTRAINTS:").unwrap();
        let goal = p.find("GOAL:").unwrap();
        let fail = p.find("FAILURE MEMORY").unwrap();
        assert!(contract_end < con && con < goal && goal < fail);
    }

    #[test]
    fn extract_unwraps_fence() {
        let raw = "```go\nfunc main() {}\n```";
        assert_eq!(extract_solution(raw), "func main() {}");
    }

    #[test]
    fn extract_strips_filler_then_fence() {
        let raw = "Sure, here you go:\n```\ncode body\n```";
        assert_eq!(extract_solution(raw), "code body");
    }

    #[test]
    fn extract_preserves_code_starting_with_filler_word() {
        // Finding 12.4: fenced code beginning with "Sure" must survive intact.
        let raw = "```sql\nSure_table_name := 'x';\nSELECT 1;\n```";
        let out = extract_solution(raw);
        assert!(out.starts_with("Sure_table_name"));
    }

    #[test]
    fn extract_plain_passthrough() {
        assert_eq!(extract_solution("  plain answer  "), "plain answer");
    }

    #[test]
    fn delegate_route_emits_delegation_packet() {
        let mut st = State::new("t", "migrate all 200 call sites to the new API");
        st.constraints.push("no behavior changes".into());
        let p = build(Route::Delegate, &st);
        assert!(p.starts_with(KERNEL_CONTRACT), "static block must lead");
        assert!(p.contains("DELEGATION PACKET"));
        // The packet must be valid JSON carrying the goal and constraints.
        let json_start = p.find('{').unwrap();
        let json_end = p.rfind('}').unwrap();
        let packet: crate::kernel::DelegationPacket =
            serde_json::from_str(&p[json_start..=json_end]).unwrap();
        assert_eq!(packet.task, "migrate all 200 call sites to the new API");
        assert_eq!(packet.constraints, vec!["no behavior changes".to_string()]);
        assert!(!packet.acceptance.is_empty());
        assert!(!packet.next_step.is_empty());
    }

    #[test]
    fn delegate_packet_carries_failure_memory() {
        let mut st = State::new("t", "bulk rename");
        st.remember_failure("approach A", "broke tests");
        let p = build(Route::Delegate, &st);
        assert!(p.contains("FAILURE MEMORY"));
        assert!(p.contains("approach A"));
    }
}
