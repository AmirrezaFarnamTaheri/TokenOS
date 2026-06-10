// Package verify implements the tiered Verification Budget:
//
//	static checks → first   (free, local)
//	targeted tests → second (cheap, local)
//	LLM verifier   → last resort (expensive, upstream)
//
// Verification cost stays proportional to expected failure cost: only the
// most likely failure mode is checked, never the whole world.
package verify

import (
	"fmt"
	"regexp"
	"strings"
)

// Result of a verification pass.
type Result struct {
	Pass    bool     `json:"pass"`
	Tier    string   `json:"tier"` // static | tests | llm
	Issues  []string `json:"issues,omitempty"`
	CostTok int      `json:"cost_tokens"` // tokens spent on verification (0 for local tiers)
}

// StaticCheck performs the free, local AST-lite differential pass:
// bracket/paren/brace balance, unterminated strings, suspicious diff
// structure for PATCH routes, and obviously truncated output.
func StaticCheck(route, output string) Result {
	var issues []string

	if strings.TrimSpace(output) == "" {
		issues = append(issues, "empty output")
		return Result{Pass: false, Tier: "static", Issues: issues}
	}

	if route == "PATCH" {
		if !looksLikeDiff(output) {
			issues = append(issues, "PATCH route output is not a unified diff")
		}
	}

	if route == "ASK" {
		if qs := strings.Count(output, "?"); qs == 0 {
			issues = append(issues, "ASK route output contains no question")
		} else if questionsCount(output) > 1 {
			issues = append(issues, "ASK route must contain exactly one question")
		}
	}

	if d := braceBalance(output); d != 0 && looksLikeCode(output) {
		issues = append(issues, fmt.Sprintf("unbalanced braces (delta %+d) — possible truncation", d))
	}

	if strings.HasSuffix(strings.TrimSpace(output), "...") {
		issues = append(issues, "output appears truncated (trailing ellipsis)")
	}

	return Result{Pass: len(issues) == 0, Tier: "static", Issues: issues}
}

func looksLikeDiff(s string) bool {
	t := strings.TrimSpace(s)
	return strings.HasPrefix(t, "--- ") || strings.HasPrefix(t, "diff ") ||
		strings.Contains(t, "\n--- ") || strings.HasPrefix(t, "@@")
}

var reQuestionLine = regexp.MustCompile(`(?m)\?\s*$`)

func questionsCount(s string) int {
	return len(reQuestionLine.FindAllString(s, -1))
}

// braceBalance returns the net {}/()/[] depth, ignoring string literals and
// line comments (a cheap approximation of an AST balance check).
func braceBalance(s string) int {
	depth := 0
	inStr := byte(0)
	esc := false
	for i := 0; i < len(s); i++ {
		c := s[i]
		if inStr != 0 {
			if esc {
				esc = false
				continue
			}
			switch c {
			case '\\':
				esc = true
			case inStr:
				inStr = 0
			}
			continue
		}
		switch c {
		case '"', '\'', '`':
			inStr = c
		case '/':
			if i+1 < len(s) && s[i+1] == '/' {
				for i < len(s) && s[i] != '\n' {
					i++
				}
			}
		case '{', '(', '[':
			depth++
		case '}', ')', ']':
			depth--
		}
	}
	return depth
}

var reCodeHints = regexp.MustCompile(`(?m)^\s*(func|def|class|fn|public|private|import|package|const|let|var)\b`)

func looksLikeCode(s string) bool {
	return reCodeHints.MatchString(s)
}
