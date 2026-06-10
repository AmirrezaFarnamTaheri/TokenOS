package config

import "path/filepath"

// ModelFilter implements the Two-Tier Filtering Matrix.
//
// Precedence rules (deterministic, short-circuiting):
//  1. Absolute blacklist: any match in Exclude drops the model immediately.
//  2. Explicit whitelist: if Include is non-empty, the model must match at
//     least one pattern; otherwise it is dropped.
//  3. Default fallback: with neither list defined, all models are permitted.
//
// Patterns support shell-style wildcards via filepath.Match (e.g.
// "gemini-2.0-flash-*"), plus exact string equality.
type ModelFilter struct {
	Include []string `yaml:"include,omitempty" json:"include,omitempty"`
	Exclude []string `yaml:"exclude,omitempty" json:"exclude,omitempty"`
}

// IsModelAllowed evaluates the precedence rules for a given model ID.
func (f *ModelFilter) IsModelAllowed(modelID string) bool {
	// Rule 1: absolute blacklist — exclusion always wins.
	for _, pattern := range f.Exclude {
		if patternMatch(pattern, modelID) {
			return false
		}
	}
	// Rule 2: explicit whitelist.
	if len(f.Include) == 0 {
		return true // Rule 3: default allow when not explicitly blocked.
	}
	for _, pattern := range f.Include {
		if patternMatch(pattern, modelID) {
			return true
		}
	}
	return false
}

// Filter returns the subset of candidate model IDs that pass the matrix.
func (f *ModelFilter) Filter(models []string) []string {
	out := make([]string, 0, len(models))
	for _, m := range models {
		if f.IsModelAllowed(m) {
			out = append(out, m)
		}
	}
	return out
}

func patternMatch(pattern, s string) bool {
	if pattern == s {
		return true
	}
	ok, err := filepath.Match(pattern, s)
	return err == nil && ok
}
