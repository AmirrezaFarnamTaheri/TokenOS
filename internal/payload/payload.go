// Package payload serializes kernel state into provider prompts using the
// JIT Prompt Caching Alignment Strategy: a strict static→dynamic ordering so
// provider-side prefix caches stay hot across turns.
//
//	┌──────────────────────────────────────────────┐
//	│ STATIC      kernel contract (never changes)  │ → high cache hit
//	├──────────────────────────────────────────────┤
//	│ SEMI-STATIC constraints, architecture notes  │ → moderate cache hit
//	├──────────────────────────────────────────────┤
//	│ DYNAMIC     state, context, failures, action │ → appended last
//	└──────────────────────────────────────────────┘
package payload

import (
	"fmt"
	"strings"

	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/kernel"
)

// KernelContract is the tiny worker contract. Workers are not smart; the
// orchestration layer is. This block must remain byte-stable to maximize
// provider prefix-cache hits.
const KernelContract = `You are a token-optimal execution worker.
Rules:
1. Output the finished result only. No preamble, no commentary, no apologies.
2. For PATCH routes output a unified diff only.
3. For ASK routes output exactly one question.
4. For PARTIAL routes output completed work, then a line "BLOCKERS:" listing blockers.
5. Stop at acceptance. No optional refactoring, no extra enhancements.
6. Never restate the task or your reasoning.`

// Build produces the final prompt for a given route and state, with static
// content first and volatile content last.
func Build(route kernel.Route, st *kernel.State) string {
	var b strings.Builder

	// --- STATIC BLOCK ---
	b.WriteString(KernelContract)
	b.WriteString("\n\n")

	// --- SEMI-STATIC BLOCK ---
	if len(st.Constraints) > 0 {
		b.WriteString("CONSTRAINTS:\n")
		for _, c := range st.Constraints {
			b.WriteString("- ")
			b.WriteString(c)
			b.WriteByte('\n')
		}
		b.WriteByte('\n')
	}

	// --- DYNAMIC BLOCK (always last; never breaks the prefix above) ---
	fmt.Fprintf(&b, "ROUTE: %s\n", route)
	fmt.Fprintf(&b, "GOAL: %s\n", st.Goal)
	if st.Context != "" {
		b.WriteString("CONTEXT (minimum viable):\n")
		b.WriteString(st.Context)
		if !strings.HasSuffix(st.Context, "\n") {
			b.WriteByte('\n')
		}
	}
	if len(st.Failures) > 0 {
		b.WriteString("FAILURE MEMORY (do not repeat):\n")
		for _, f := range st.Failures {
			fmt.Fprintf(&b, "- failed: %s | reason: %s\n", f.Action, f.Reason)
		}
	}
	if st.NextAction != "" {
		fmt.Fprintf(&b, "NEXT ACTION: %s\n", st.NextAction)
	}
	return b.String()
}

// ExtractSolution applies the strict output contract: strip common
// conversational filler and fences the providers sometimes add despite
// instructions, returning the solution body only.
func ExtractSolution(raw string) string {
	s := strings.TrimSpace(raw)
	// Drop a leading "Sure"/"Here is"-style filler line if present.
	if i := strings.IndexByte(s, '\n'); i > 0 {
		first := strings.ToLower(s[:i])
		for _, filler := range []string{"sure", "here is", "here's", "certainly", "of course"} {
			if strings.HasPrefix(first, filler) {
				s = strings.TrimSpace(s[i+1:])
				break
			}
		}
	}
	// Unwrap a single outer code fence.
	if strings.HasPrefix(s, "```") {
		if end := strings.LastIndex(s, "```"); end > 3 {
			nl := strings.IndexByte(s, '\n')
			if nl >= 0 && nl < end {
				body := s[nl+1 : end]
				return strings.TrimRight(body, "\n")
			}
		}
	}
	return s
}
