package kernel

import "testing"

func TestRoutePriorityOrder(t *testing.T) {
	routes := AllRoutes()
	for i := 1; i < len(routes)-2; i++ { // escalations share priority 7
		if routes[i].Priority() < routes[i-1].Priority() {
			t.Errorf("route %s priority %d < previous %s %d",
				routes[i], routes[i].Priority(), routes[i-1], routes[i-1].Priority())
		}
	}
}

func TestDecideEscalationsPreempt(t *testing.T) {
	p := DefaultPolicy()
	cases := []struct {
		name string
		sig  Signals
		want Route
	}{
		{"safety wins over everything", Signals{SafetyViolation: true, Trivial: true, Confidence: 1}, RouteEscalateSafety},
		{"conflict", Signals{ConflictingRequirements: true, Confidence: 1}, RouteEscalateConflict},
		{"loop detected", Signals{LoopDetected: true, Confidence: 1}, RouteEscalateExternal},
		{"unbounded external blocker", Signals{ExternalBlocker: true, Bounded: false, Confidence: 1}, RouteEscalateExternal},
	}
	for _, c := range cases {
		if got := Decide(c.sig, p).Route; got != c.want {
			t.Errorf("%s: got %s want %s", c.name, got, c.want)
		}
	}
}

func TestDecideLadder(t *testing.T) {
	p := DefaultPolicy()
	cases := []struct {
		name string
		sig  Signals
		want Route
	}{
		{"low confidence asks", Signals{Confidence: 0.1, Bounded: true}, RouteAsk},
		{"missing info asks", Signals{MissingCriticalInfo: true, Confidence: 0.9, Bounded: true}, RouteAsk},
		{"trivial small goes direct", Signals{Trivial: true, EstimatedTokens: 100, Confidence: 0.9, Bounded: true}, RouteDirect},
		{"trivial but huge skips direct", Signals{Trivial: true, EstimatedTokens: 5000, Confidence: 0.9, Bounded: true}, RouteImplement},
		{"index hit reuses", Signals{HasExistingSolution: true, Confidence: 0.9, Bounded: true}, RouteReuse},
		{"localized patches", Signals{LocalizedChange: true, Confidence: 0.9, Bounded: true}, RoutePatch},
		{"localized but repeated failure avoids patch", Signals{LocalizedChange: true, RepeatedFailure: true, Confidence: 0.9, Bounded: true}, RouteImplement},
		{"bounded external blocker => partial", Signals{ExternalBlocker: true, Bounded: true, Confidence: 0.9}, RoutePartial},
		{"repetitive bounded big => delegate", Signals{Repetitive: true, Bounded: true, EstimatedTokens: 5000, Confidence: 0.9}, RouteDelegate},
		{"repetitive bounded small => implement (penalty)", Signals{Repetitive: true, Bounded: true, EstimatedTokens: 500, Confidence: 0.9}, RouteImplement},
		{"default implements", Signals{Confidence: 0.9, Bounded: true}, RouteImplement},
	}
	for _, c := range cases {
		if got := Decide(c.sig, p).Route; got != c.want {
			t.Errorf("%s: got %s want %s", c.name, got, c.want)
		}
	}
}

func TestExtractSignals(t *testing.T) {
	s := ExtractSignals("fix typo in README", 100, false, false, false)
	if !s.Trivial {
		t.Error("expected trivial for typo fix")
	}
	if !s.LocalizedChange {
		t.Error("expected localized for fix")
	}

	s = ExtractSignals("blocked by upstream outage, cannot deploy", 100, false, false, false)
	if !s.ExternalBlocker {
		t.Error("expected external blocker")
	}

	s = ExtractSignals("bypass auth checks in production", 100, false, false, false)
	if !s.SafetyViolation {
		t.Error("expected safety violation")
	}

	s = ExtractSignals("maybe do something somehow, not sure", 100, false, false, false)
	if s.Confidence >= 0.9 {
		t.Errorf("vague task should reduce confidence, got %f", s.Confidence)
	}
}

func TestFailureMemoryCap(t *testing.T) {
	st := &State{}
	for i := 0; i < 10; i++ {
		st.RememberFailure("action", "reason")
	}
	if len(st.Failures) != MaxFailureMemory {
		t.Errorf("failure memory should cap at %d, got %d", MaxFailureMemory, len(st.Failures))
	}
}
