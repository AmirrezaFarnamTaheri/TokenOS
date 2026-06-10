package pricing

import "testing"

func TestQuoteAllPrefersCheaper(t *testing.T) {
	cands := []Candidate{
		{Provider: "expensive", CostPerMTokIn: 10, CostPerMTokOut: 30, MaxContext: 100000, Priority: 1},
		{Provider: "cheap", CostPerMTokIn: 0.1, CostPerMTokOut: 0.4, MaxContext: 100000, Priority: 2},
	}
	quotes := QuoteAll(cands, 0.9, 1000, 1000, DefaultWeights(), nil, nil)
	if len(quotes) != 2 {
		t.Fatalf("want 2 quotes, got %d", len(quotes))
	}
	if quotes[0].Provider != "cheap" {
		t.Errorf("cheap provider should win on utility, got %q", quotes[0].Provider)
	}
}

func TestQuoteAllContextHardConstraint(t *testing.T) {
	cands := []Candidate{
		{Provider: "small", MaxContext: 100, CostPerMTokIn: 0.1},
		{Provider: "big", MaxContext: 1000000, CostPerMTokIn: 5},
	}
	quotes := QuoteAll(cands, 0.9, 50000, 1000, DefaultWeights(), nil, nil)
	if len(quotes) != 1 || quotes[0].Provider != "big" {
		t.Errorf("context overflow must drop small provider: %+v", quotes)
	}
}

func TestQuotaPressureDecaysUtility(t *testing.T) {
	tr := NewTracker()
	for i := 0; i < 10; i++ {
		tr.Record("saturated", 100, true)
	}
	cands := []Candidate{
		{Provider: "saturated", CostPerMTokIn: 0.1, MaxContext: 100000},
		{Provider: "idle", CostPerMTokIn: 0.1, MaxContext: 100000},
	}
	quotes := QuoteAll(cands, 0.9, 1000, 1000, DefaultWeights(), tr,
		map[string]int{"saturated": 10, "idle": 10})
	if quotes[0].Provider != "idle" {
		t.Errorf("quota-saturated provider should lose, got %q first", quotes[0].Provider)
	}
	for _, q := range quotes {
		if q.Provider == "saturated" && q.QuotaPressure < 0.99 {
			t.Errorf("saturated pressure = %f, want ~1", q.QuotaPressure)
		}
	}
}

func TestFailureEWMADecaysUtility(t *testing.T) {
	tr := NewTracker()
	for i := 0; i < 5; i++ {
		tr.Record("flaky", 100, false)
		tr.Record("solid", 100, true)
	}
	cands := []Candidate{
		{Provider: "flaky", CostPerMTokIn: 0.1, MaxContext: 100000},
		{Provider: "solid", CostPerMTokIn: 0.1, MaxContext: 100000},
	}
	quotes := QuoteAll(cands, 0.9, 1000, 1000, DefaultWeights(), tr, nil)
	if quotes[0].Provider != "solid" {
		t.Errorf("failing provider should rank below healthy one")
	}
}
