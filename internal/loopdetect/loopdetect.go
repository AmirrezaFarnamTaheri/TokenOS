// Package loopdetect implements semantic loop detection via edit-distance
// ceilings (Levenshtein). Agents oscillating between near-identical failed
// attempts are caught deterministically and escalated, bypassing model
// confidence entirely.
package loopdetect

// DefaultCeiling is the normalized edit-distance below which two failed
// attempts are considered "the same attempt" (3%).
const DefaultCeiling = 0.03

// Detector keeps a bounded window of prior failed attempt payloads.
type Detector struct {
	Ceiling float64 // normalized distance threshold (0..1)
	Window  int     // how many prior attempts to compare against
	history []string
}

// New creates a Detector with sane defaults.
func New() *Detector {
	return &Detector{Ceiling: DefaultCeiling, Window: 5}
}

// Observe records a failed attempt and reports whether it forms a semantic
// loop with any previously observed attempt.
func (d *Detector) Observe(attempt string) bool {
	loop := false
	for _, prev := range d.history {
		if NormalizedDistance(prev, attempt) < d.Ceiling {
			loop = true
			break
		}
	}
	d.history = append(d.history, attempt)
	if d.Window > 0 && len(d.history) > d.Window {
		d.history = d.history[len(d.history)-d.Window:]
	}
	return loop
}

// Reset clears the attempt history (e.g., after a successful verification).
func (d *Detector) Reset() { d.history = nil }

// NormalizedDistance returns Levenshtein(a,b) / max(len(a),len(b)).
func NormalizedDistance(a, b string) float64 {
	if a == b {
		return 0
	}
	ra, rb := []rune(a), []rune(b)
	la, lb := len(ra), len(rb)
	if la == 0 || lb == 0 {
		return 1
	}
	maxLen := la
	if lb > maxLen {
		maxLen = lb
	}
	d := levenshtein(ra, rb)
	return float64(d) / float64(maxLen)
}

// levenshtein is a memory-efficient two-row dynamic program.
func levenshtein(a, b []rune) int {
	la, lb := len(a), len(b)
	if la == 0 {
		return lb
	}
	if lb == 0 {
		return la
	}
	prev := make([]int, lb+1)
	curr := make([]int, lb+1)
	for j := 0; j <= lb; j++ {
		prev[j] = j
	}
	for i := 1; i <= la; i++ {
		curr[0] = i
		for j := 1; j <= lb; j++ {
			cost := 1
			if a[i-1] == b[j-1] {
				cost = 0
			}
			del := prev[j] + 1
			ins := curr[j-1] + 1
			sub := prev[j-1] + cost
			m := del
			if ins < m {
				m = ins
			}
			if sub < m {
				m = sub
			}
			curr[j] = m
		}
		prev, curr = curr, prev
	}
	return prev[lb]
}
