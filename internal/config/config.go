// Package config loads and validates the TokenOS configuration: provider
// profiles, the two-tier model filter matrix, routing policy thresholds and
// fallback chains.
package config

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"

	"gopkg.in/yaml.v3"

	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/kernel"
)

// Config is the root configuration document (~/.config/tokenos/config.yaml).
type Config struct {
	CurrentProfile string              `yaml:"current_profile" json:"current_profile"`
	Policy         kernel.RouterPolicy `yaml:"policy" json:"policy"`
	Providers      map[string]Provider `yaml:"providers" json:"providers"`
	Routing        []RoutingRule       `yaml:"execution_routing" json:"execution_routing"`
	Pricing        PricingWeights      `yaml:"pricing" json:"pricing"`
}

// Provider describes one upstream platform profile.
type Provider struct {
	Adapter        string      `yaml:"adapter" json:"adapter"`     // mock | openai | anthropic | gemini | proxy
	AuthType       string      `yaml:"auth_type" json:"auth_type"` // api_key | oauth2 | none
	APIKeyEnv      string      `yaml:"api_key_env,omitempty" json:"api_key_env,omitempty"`
	Endpoint       string      `yaml:"endpoint,omitempty" json:"endpoint,omitempty"`
	Model          string      `yaml:"model,omitempty" json:"model,omitempty"`
	Priority       int         `yaml:"priority" json:"priority"` // lower = preferred
	QuotaPerMin    int         `yaml:"quota_limit_per_min,omitempty" json:"quota_limit_per_min,omitempty"`
	MaxContext     int         `yaml:"max_context_tokens,omitempty" json:"max_context_tokens,omitempty"`
	CostPerMTokIn  float64     `yaml:"cost_per_mtok_in,omitempty" json:"cost_per_mtok_in,omitempty"`
	CostPerMTokOut float64     `yaml:"cost_per_mtok_out,omitempty" json:"cost_per_mtok_out,omitempty"`
	Models         ModelFilter `yaml:"models,omitempty" json:"models,omitempty"`
	Disabled       bool        `yaml:"disabled,omitempty" json:"disabled,omitempty"`
}

// RoutingRule binds kernel routes to a provider with a fallback chain.
type RoutingRule struct {
	Provider   string   `yaml:"provider" json:"provider"`
	RouteTypes []string `yaml:"route_types" json:"route_types"`
	MaxContext int      `yaml:"max_context_tokens,omitempty" json:"max_context_tokens,omitempty"`
	Fallback   string   `yaml:"fallback,omitempty" json:"fallback,omitempty"`
	TimeoutMS  int      `yaml:"timeout_ms,omitempty" json:"timeout_ms,omitempty"`
}

// PricingWeights tunes the shadow-pricing utility function:
//
//	U = confidence / (alpha*tokenCost + beta*latency)
type PricingWeights struct {
	Alpha float64 `yaml:"alpha" json:"alpha"` // weight on token cost
	Beta  float64 `yaml:"beta" json:"beta"`   // weight on historical latency
}

// Default returns a complete, working default configuration with a mock
// provider so the system is testable offline out of the box.
func Default() *Config {
	return &Config{
		CurrentProfile: "default",
		Policy:         kernel.DefaultPolicy(),
		Pricing:        PricingWeights{Alpha: 1.0, Beta: 0.002},
		Providers: map[string]Provider{
			"mock": {
				Adapter:    "mock",
				AuthType:   "none",
				Model:      "mock-1",
				Priority:   100,
				MaxContext: 128000,
			},
			"openai": {
				Adapter:        "openai",
				AuthType:       "api_key",
				APIKeyEnv:      "OPENAI_API_KEY",
				Endpoint:       "https://api.openai.com/v1",
				Model:          "gpt-4o-mini",
				Priority:       2,
				MaxContext:     128000,
				CostPerMTokIn:  0.15,
				CostPerMTokOut: 0.60,
				Models:         ModelFilter{Include: []string{"gpt-4o*", "gpt-4.1*", "o4*"}},
				Disabled:       true,
			},
			"anthropic": {
				Adapter:        "anthropic",
				AuthType:       "api_key",
				APIKeyEnv:      "ANTHROPIC_API_KEY",
				Endpoint:       "https://api.anthropic.com/v1",
				Model:          "claude-sonnet-4-20250514",
				Priority:       1,
				MaxContext:     200000,
				CostPerMTokIn:  3.0,
				CostPerMTokOut: 15.0,
				Models:         ModelFilter{Include: []string{"claude-*"}, Exclude: []string{"claude-2*"}},
				Disabled:       true,
			},
			"gemini": {
				Adapter:        "gemini",
				AuthType:       "api_key",
				APIKeyEnv:      "GEMINI_API_KEY",
				Endpoint:       "https://generativelanguage.googleapis.com/v1beta",
				Model:          "gemini-2.0-flash",
				Priority:       3,
				MaxContext:     1048576,
				CostPerMTokIn:  0.10,
				CostPerMTokOut: 0.40,
				Models: ModelFilter{
					Include: []string{"gemini-2.0-flash-*", "gemini-1.5-*", "gemini-2.0-flash"},
					Exclude: []string{"gemini-2.0-flash-thinking-*", "gemini-1.0-*"},
				},
				Disabled: true,
			},
		},
		Routing: []RoutingRule{
			{Provider: "anthropic", RouteTypes: []string{"IMPLEMENT", "PATCH"}, Fallback: "openai", TimeoutMS: 120000},
			{Provider: "openai", RouteTypes: []string{"DIRECT", "REUSE", "DELEGATE", "PARTIAL"}, Fallback: "gemini", TimeoutMS: 60000},
			{Provider: "gemini", RouteTypes: []string{"VERIFY"}, Fallback: "mock", TimeoutMS: 30000},
			{Provider: "mock", RouteTypes: []string{"*"}, TimeoutMS: 10000},
		},
	}
}

// DefaultPath returns the canonical config file location.
func DefaultPath() string {
	if p := os.Getenv("TOKENOS_CONFIG"); p != "" {
		return p
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "tokenos.yaml"
	}
	return filepath.Join(home, ".config", "tokenos", "config.yaml")
}

// Load reads a config file, falling back to defaults if it does not exist.
func Load(path string) (*Config, error) {
	if path == "" {
		path = DefaultPath()
	}
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return Default(), nil
	}
	if err != nil {
		return nil, fmt.Errorf("read config: %w", err)
	}
	cfg := Default()
	if err := yaml.Unmarshal(data, cfg); err != nil {
		return nil, fmt.Errorf("parse config: %w", err)
	}
	if err := cfg.Validate(); err != nil {
		return nil, err
	}
	return cfg, nil
}

// Save writes the configuration to disk, creating parent directories.
func (c *Config) Save(path string) error {
	if path == "" {
		path = DefaultPath()
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	data, err := yaml.Marshal(c)
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0o600)
}

// Validate enforces structural invariants.
func (c *Config) Validate() error {
	if len(c.Providers) == 0 {
		return fmt.Errorf("config: at least one provider required")
	}
	for name, p := range c.Providers {
		if p.Adapter == "" {
			return fmt.Errorf("config: provider %q missing adapter", name)
		}
	}
	for i, r := range c.Routing {
		if _, ok := c.Providers[r.Provider]; !ok {
			return fmt.Errorf("config: routing rule %d references unknown provider %q", i, r.Provider)
		}
		if r.Fallback != "" {
			if _, ok := c.Providers[r.Fallback]; !ok {
				return fmt.Errorf("config: routing rule %d fallback references unknown provider %q", i, r.Fallback)
			}
		}
	}
	if c.Policy.AskThreshold <= 0 {
		c.Policy = kernel.DefaultPolicy()
	}
	return nil
}

// ProviderChain resolves the ordered provider chain for a route: the first
// matching routing rule plus its fallback chain, then remaining enabled
// providers by priority. Cycles are guarded.
func (c *Config) ProviderChain(route string) []string {
	seen := map[string]bool{}
	var chain []string
	add := func(name string) {
		if name == "" || seen[name] {
			return
		}
		p, ok := c.Providers[name]
		if !ok {
			return
		}
		seen[name] = true
		if !p.Disabled {
			chain = append(chain, name)
		}
	}

	for _, rule := range c.Routing {
		if !matchesRoute(rule.RouteTypes, route) {
			continue
		}
		add(rule.Provider)
		fb := rule.Fallback
		for fb != "" && !seen[fb] {
			cur := fb
			add(cur)
			// follow chained fallbacks declared by rules for the fallback provider
			next := ""
			for _, r2 := range c.Routing {
				if r2.Provider == cur {
					next = r2.Fallback
					break
				}
			}
			fb = next
		}
		break
	}

	// Append remaining enabled providers ordered by priority as last resort.
	type pp struct {
		name string
		prio int
	}
	var rest []pp
	for name, p := range c.Providers {
		if !seen[name] && !p.Disabled {
			rest = append(rest, pp{name, p.Priority})
		}
	}
	sort.Slice(rest, func(i, j int) bool {
		if rest[i].prio != rest[j].prio {
			return rest[i].prio < rest[j].prio
		}
		return rest[i].name < rest[j].name
	})
	for _, r := range rest {
		chain = append(chain, r.name)
	}
	return chain
}

func matchesRoute(types []string, route string) bool {
	for _, t := range types {
		if t == "*" || t == route {
			return true
		}
	}
	return false
}
