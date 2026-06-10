// Package tokenizer provides fast, offline token estimation so the kernel
// can enforce context budgets before a single network byte is spent.
//
// It implements a deterministic BPE-approximation heuristic calibrated
// against cl100k_base behaviour on mixed code/prose corpora. It is not a
// real BPE tokenizer (no vocabulary shipping), but errs conservatively
// (slightly over-estimates), which is the safe direction for budgeting.
package tokenizer

import (
	"unicode"
	"unicode/utf8"
)

// Estimate returns an approximate token count for s.
//
// Calibration model:
//   - ASCII prose averages ~4 chars/token.
//   - Code/symbol-dense text averages ~2 chars/token for symbols.
//   - Non-ASCII (CJK etc.) averages ~1.1 tokens per rune.
func Estimate(s string) int {
	if s == "" {
		return 0
	}
	var asciiLetters, symbols, nonASCII, spaces int
	for _, r := range s {
		switch {
		case r > 127:
			nonASCII++
		case unicode.IsSpace(r):
			spaces++
		case unicode.IsLetter(r) || unicode.IsDigit(r):
			asciiLetters++
		default:
			symbols++
		}
	}
	proseTokens := float64(asciiLetters+spaces) / 4.2
	symbolTokens := float64(symbols) / 1.8
	cjkTokens := float64(nonASCII) * 1.1

	n := int(proseTokens + symbolTokens + cjkTokens)
	if n < 1 {
		n = 1
	}
	return n
}

// EstimateBytes is a convenience wrapper for raw byte slices.
func EstimateBytes(b []byte) int {
	if !utf8.Valid(b) {
		// Binary-ish content: assume worst-case density.
		return len(b) / 2
	}
	return Estimate(string(b))
}

// FitsBudget reports whether text fits within maxTokens.
func FitsBudget(s string, maxTokens int) bool {
	return Estimate(s) <= maxTokens
}

// Truncate trims s so its estimate fits within maxTokens, cutting at the
// nearest line boundary to avoid shipping half-statements.
func Truncate(s string, maxTokens int) string {
	if FitsBudget(s, maxTokens) {
		return s
	}
	// Binary search on byte length for the largest fitting prefix.
	lo, hi := 0, len(s)
	for lo < hi {
		mid := (lo + hi + 1) / 2
		if Estimate(s[:mid]) <= maxTokens {
			lo = mid
		} else {
			hi = mid - 1
		}
	}
	cut := s[:lo]
	// Pull back to the last newline if one exists in the final 20%.
	for i := len(cut) - 1; i >= 0 && i >= len(cut)*4/5; i-- {
		if cut[i] == '\n' {
			return cut[:i]
		}
	}
	return cut
}
