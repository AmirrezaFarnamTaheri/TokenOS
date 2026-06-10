package store

import (
	"testing"

	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/kernel"
)

func openTest(t *testing.T) *Store {
	t.Helper()
	s, err := Open(":memory:")
	if err != nil {
		t.Fatalf("open store: %v", err)
	}
	t.Cleanup(func() { s.Close() })
	return s
}

func TestTaskRoundTrip(t *testing.T) {
	s := openTest(t)
	st := &kernel.State{TaskID: "t1", Goal: "do the thing", Status: kernel.StatusPending}
	if err := s.SaveTask(st); err != nil {
		t.Fatal(err)
	}
	got, err := s.GetTask("t1")
	if err != nil {
		t.Fatal(err)
	}
	if got.Goal != "do the thing" || got.Status != kernel.StatusPending {
		t.Errorf("round trip mismatch: %+v", got)
	}
	// Upsert
	st.Status = kernel.StatusDone
	if err := s.SaveTask(st); err != nil {
		t.Fatal(err)
	}
	got, _ = s.GetTask("t1")
	if got.Status != kernel.StatusDone {
		t.Errorf("upsert failed, status %s", got.Status)
	}
}

func TestFailureMemoryCapInDB(t *testing.T) {
	s := openTest(t)
	s.SaveTask(&kernel.State{TaskID: "t2", Goal: "g", Status: kernel.StatusPending})
	for i := 0; i < 10; i++ {
		if err := s.RecordFailure("t2", "act", "reason"); err != nil {
			t.Fatal(err)
		}
	}
	fails, err := s.Failures("t2")
	if err != nil {
		t.Fatal(err)
	}
	if len(fails) != kernel.MaxFailureMemory {
		t.Errorf("DB failure memory should cap at %d, got %d", kernel.MaxFailureMemory, len(fails))
	}
	ok, err := s.HasSimilarFailure("t2", "act")
	if err != nil || !ok {
		t.Errorf("HasSimilarFailure = %v, %v", ok, err)
	}
}

func TestTelemetryAggregates(t *testing.T) {
	s := openTest(t)
	s.SaveTask(&kernel.State{TaskID: "t3", Goal: "g", Status: kernel.StatusDone})
	execs := []Execution{
		{TaskID: "t3", Route: "PATCH", Provider: "mock", TokensIn: 100, TokensOut: 50, LatencyMS: 200, EstCostUSD: 0.002, Success: true},
		{TaskID: "t3", Route: "PATCH", Provider: "mock", TokensIn: 120, TokensOut: 60, LatencyMS: 250, EstCostUSD: 0.003, Success: false},
		{TaskID: "t3", Route: "IMPLEMENT", Provider: "mock", TokensIn: 500, TokensOut: 300, LatencyMS: 900, EstCostUSD: 0.01, Success: true},
	}
	for _, e := range execs {
		if err := s.RecordExecution(e); err != nil {
			t.Fatal(err)
		}
	}
	sum, err := s.GetSummary()
	if err != nil {
		t.Fatal(err)
	}
	if sum.Executions != 3 || sum.Successes != 2 {
		t.Errorf("summary executions=%d successes=%d", sum.Executions, sum.Successes)
	}
	wantCPS := (0.002 + 0.003 + 0.01) / 2
	if sum.CostPerSuccess < wantCPS-1e-9 || sum.CostPerSuccess > wantCPS+1e-9 {
		t.Errorf("cost per success = %f, want %f", sum.CostPerSuccess, wantCPS)
	}

	routes, err := s.StatsByRoute()
	if err != nil {
		t.Fatal(err)
	}
	if len(routes) != 2 {
		t.Fatalf("want 2 route groups, got %d", len(routes))
	}
	provs, err := s.StatsByProvider()
	if err != nil || len(provs) != 1 {
		t.Fatalf("provider stats: %v, %v", provs, err)
	}
	if provs[0].TotalTokens != 100+50+120+60+500+300 {
		t.Errorf("provider tokens = %d", provs[0].TotalTokens)
	}
}
