// Package pricing implements Dynamic Shadow Pricing: provider selection as a
// live constraint optimization rather than a static priority queue.
//
//	U = confidence / (alpha * tokenCost + beta * historicalLatency)
//
// Higher utility wins. Quota depletion and recent failures depress utility,
// so the scheduler automatically drains toward healthy, cheap, fast routes.
package pricing

import (
	"math"
	"sort"
	"sync"
	"time"
)

// Candidate is one provider option under evaluation.
type Candidate struct {
	Provider       string
	Model          string
	CostPerMTokIn  float64 // $/1M input tokens
	CostPerMTokOut float64 // $/1M output tokens
	MaxContext     int
	Priority       int // static tiebreaker (lower preferred)
}

// Quote is the scored result for a candidate.
type Quote struct {
	Candidate
	Utility       float64 `json:"utility"`
	EstCostUSD    float64 `json:"est_cost_usd"`
	AvgLatencyMS  float64 `json:"avg_latency_ms"`
	RecentFailPct float64 `json:"recent_fail_pct"`
	QuotaPressure float64 `json:"quota_pressure"`
}

// Weights tunes the utility function.
type Weights struct {
	Alpha float64 // weight on token cost
	Beta  float64 // weight on latency (per ms)
}

// DefaultWeights returns sane production defaults.
func DefaultWeights() Weights { return Weights{Alpha: 1.0, Beta: 0.002} }

// ---------------------------------------------------------------------------
// Rolling health tracker (EWMA latency + failure rate + quota window).
// ---------------------------------------------------------------------------

type health struct {
	ewmaLatencyMS float64
	failEWMA      float64 // 0..1
	calls         []time.Time
}

// Tracker accumulates live per-provider health metrics.
type Tracker struct {
	mu     sync.Mutex
	state  map[string]*health
	window time.Duration
}

// NewTracker creates a Tracker with a 1-minute quota window.
func NewTracker() *Tracker {
	return &Tracker{state: map[string]*health{}, window: time.Minute}
}

const ewmaAlpha = 0.3

// Record registers an execution outcome for a provider.
func (t *Tracker) Record(provider string, latencyMS float64, success bool) {
	t.mu.Lock()
	defer t.mu.Unlock()
	h := t.state[provider]
	if h == nil {
		h = &health{ewmaLatencyMS: latencyMS}
		t.state[provider] = h
	}
	h.ewmaLatencyMS = ewmaAlpha*latencyMS + (1-ewmaAlpha)*h.ewmaLatencyMS
	f := 0.0
	if !success {
		f = 1.0
	}
	h.failEWMA = ewmaAlpha*f + (1-ewmaAlpha)*h.failEWMA
	now := time.Now()
	h.calls = append(h.calls, now)
	// Prune calls outside the quota window.
	cutoff := now.Add(-t.window)
	i := 0
	for i < len(h.calls) && h.calls[i].Before(cutoff) {
		i++
	}
	h.calls = h.calls[i:]
}

// Snapshot returns (avgLatencyMS, failRate, callsInWindow) for a provider.
func (t *Tracker) Snapshot(provider string) (float64, float64, int) {
	t.mu.Lock()
	defer t.mu.Unlock()
	h := t.state[provider]
	if h == nil {
		return 0, 0, 0
	}
	return h.ewmaLatencyMS, h.failEWMA, len(h.calls)
}

// ---------------------------------------------------------------------------
// Shadow pricing
// ---------------------------------------------------------------------------

// QuoteAll scores all candidates for a task and returns them sorted by
// utility (best first). estIn/estOut are token estimates; quotaPerMin maps
// provider -> per-minute call quota (0 = unlimited).
func QuoteAll(
	cands []Candidate,
	confidence float64,
	estIn, estOut int,
	w Weights,
	tracker *Tracker,
	quotaPerMin map[string]int,
) []Quote {
	quotes := make([]Quote, 0, len(cands))
	for _, c := range cands {
		// Hard constraint: context must fit.
		if c.MaxContext > 0 && estIn > c.MaxContext {
			continue
		}
		cost := (float64(estIn)*c.CostPerMTokIn + float64(estOut)*c.CostPerMTokOut) / 1e6

		var lat, fail float64
		var calls int
		if tracker != nil {
			lat, fail, calls = tracker.Snapshot(c.Provider)
		}
		// Quota pressure: 0 (idle) .. 1 (saturated). Saturated providers are
		// shadow-priced toward zero utility instead of hard-dropped, so a
		// fully exhausted fleet still produces a deterministic ordering.
		pressure := 0.0
		if q := quotaPerMin[c.Provider]; q > 0 {
			pressure = math.Min(1, float64(calls)/float64(q))
		}

		denom := w.Alpha*cost*1000 + w.Beta*lat + 1e-9 // scale cost to comparable magnitude
		u := confidence / denom
		u *= (1 - fail)         // failure-prone providers decay
		u *= (1 - 0.9*pressure) // quota saturation decays utility by up to 90%
		if u < 0 {
			u = 0
		}
		quotes = append(quotes, Quote{
			Candidate:     c,
			Utility:       u,
			EstCostUSD:    cost,
			AvgLatencyMS:  lat,
			RecentFailPct: fail,
			QuotaPressure: pressure,
		})
	}
	sort.Slice(quotes, func(i, j int) bool {
		if quotes[i].Utility != quotes[j].Utility {
			return quotes[i].Utility > quotes[j].Utility
		}
		if quotes[i].Priority != quotes[j].Priority {
			return quotes[i].Priority < quotes[j].Priority
		}
		return quotes[i].Provider < quotes[j].Provider
	})
	return quotes
}
