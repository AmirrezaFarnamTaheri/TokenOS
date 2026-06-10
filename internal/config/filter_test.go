package config

import "testing"

func TestFilterMatrixPrecedence(t *testing.T) {
	f := ModelFilter{
		Include: []string{"gemini-1.5-flash", "gemini-2.0-flash-*"},
		Exclude: []string{"gemini-2.0-flash-thinking-*", "gemini-1.0-*"},
	}
	cases := []struct {
		model string
		want  bool
	}{
		{"gemini-1.5-flash", true},                  // exact include
		{"gemini-2.0-flash-exp", true},              // wildcard include
		{"gemini-2.0-flash-thinking-exp", false},    // exclude wins over include wildcard
		{"gemini-1.0-pro", false},                   // excluded family
		{"gemini-1.5-pro", false},                   // not in whitelist
		{"claude-sonnet", false},                    // not in whitelist
	}
	for _, c := range cases {
		if got := f.IsModelAllowed(c.model); got != c.want {
			t.Errorf("IsModelAllowed(%q) = %v, want %v", c.model, got, c.want)
		}
	}
}

func TestFilterEmptyIncludeAllowsUnlessExcluded(t *testing.T) {
	f := ModelFilter{Exclude: []string{"bad-*"}}
	if !f.IsModelAllowed("anything") {
		t.Error("empty include should allow non-excluded models")
	}
	if f.IsModelAllowed("bad-model") {
		t.Error("exclude must always win")
	}
}

func TestFilterDefaultAllowsAll(t *testing.T) {
	f := ModelFilter{}
	if !f.IsModelAllowed("any-model-at-all") {
		t.Error("no lists defined => allow all")
	}
}

func TestProviderChain(t *testing.T) {
	cfg := Default()
	// Enable everything so the chain is deterministic.
	for name, p := range cfg.Providers {
		p.Disabled = false
		cfg.Providers[name] = p
	}
	chain := cfg.ProviderChain("IMPLEMENT")
	if len(chain) == 0 {
		t.Fatal("chain must not be empty")
	}
	if chain[0] != "anthropic" {
		t.Errorf("IMPLEMENT primary should be anthropic, got %q", chain[0])
	}
	if chain[1] != "openai" {
		t.Errorf("IMPLEMENT fallback should be openai, got %q", chain[1])
	}
	// All enabled providers eventually appear.
	if len(chain) != len(cfg.Providers) {
		t.Errorf("chain should include all enabled providers: got %v", chain)
	}
}

func TestProviderChainSkipsDisabled(t *testing.T) {
	cfg := Default() // only mock is enabled by default
	chain := cfg.ProviderChain("IMPLEMENT")
	for _, name := range chain {
		if cfg.Providers[name].Disabled {
			t.Errorf("disabled provider %q in chain", name)
		}
	}
	if len(chain) != 1 || chain[0] != "mock" {
		t.Errorf("default chain should be [mock], got %v", chain)
	}
}

func TestValidate(t *testing.T) {
	cfg := Default()
	if err := cfg.Validate(); err != nil {
		t.Fatalf("default config must validate: %v", err)
	}
	bad := Default()
	bad.Routing = append(bad.Routing, RoutingRule{Provider: "ghost", RouteTypes: []string{"*"}})
	if err := bad.Validate(); err == nil {
		t.Error("unknown provider in routing must fail validation")
	}
}
