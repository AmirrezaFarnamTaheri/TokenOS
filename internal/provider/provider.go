// Package provider implements the Provider Adapter Layer: a unified
// interface mapping the kernel's strict payload contract onto each
// platform's native API. Adapters are deliberately dumb translators —
// all intelligence lives in the orchestration layer.
package provider

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"time"

	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/config"
)

// Request is the kernel→adapter execution contract.
type Request struct {
	Route   string // kernel route (DIRECT, PATCH, ...)
	Prompt  string // fully serialized static→dynamic payload
	Model   string // resolved model ID (already filter-approved)
	MaxOut  int    // output token cap
	Timeout time.Duration
}

// Response is the adapter→kernel result.
type Response struct {
	Text      string
	TokensIn  int // provider-reported when available, else 0
	TokensOut int
	Model     string
}

// Adapter is the unified provider interface.
type Adapter interface {
	Name() string
	Execute(ctx context.Context, req Request) (*Response, error)
	// Models lists model IDs the adapter exposes (pre-filter).
	Models() []string
}

// Common sentinel errors so the scheduler can react deterministically.
var (
	ErrRateLimited = errors.New("provider rate limited (429)")
	ErrAuth        = errors.New("provider authentication failed")
	ErrUnavailable = errors.New("provider unavailable")
)

// sharedClient is a pooled HTTP client: keep-alives + HTTP/2 multiplexing
// keep upstream connections warm across consecutive kernel turns.
var sharedClient = &http.Client{
	Transport: &http.Transport{
		Proxy: http.ProxyFromEnvironment,
		DialContext: (&net.Dialer{
			Timeout:   10 * time.Second,
			KeepAlive: 60 * time.Second,
		}).DialContext,
		ForceAttemptHTTP2:   true,
		MaxIdleConns:        64,
		MaxIdleConnsPerHost: 16,
		IdleConnTimeout:     120 * time.Second,
		TLSHandshakeTimeout: 10 * time.Second,
	},
}

// New constructs an adapter from a provider profile.
func New(name string, p config.Provider) (Adapter, error) {
	apiKey := ""
	if p.APIKeyEnv != "" {
		apiKey = os.Getenv(p.APIKeyEnv)
	}
	switch p.Adapter {
	case "mock":
		return NewMock(name), nil
	case "openai":
		return &OpenAI{name: name, endpoint: orDefault(p.Endpoint, "https://api.openai.com/v1"), apiKey: apiKey, model: p.Model}, nil
	case "anthropic":
		return &Anthropic{name: name, endpoint: orDefault(p.Endpoint, "https://api.anthropic.com/v1"), apiKey: apiKey, model: p.Model}, nil
	case "gemini":
		return &Gemini{name: name, endpoint: orDefault(p.Endpoint, "https://generativelanguage.googleapis.com/v1beta"), apiKey: apiKey, model: p.Model}, nil
	case "proxy", "proxy_ide":
		// OpenAI-compatible local bridge (Cursor/Windsurf/ollama/llama.cpp...).
		if p.Endpoint == "" {
			return nil, fmt.Errorf("provider %q: proxy adapter requires endpoint", name)
		}
		return &OpenAI{name: name, endpoint: p.Endpoint, apiKey: apiKey, model: p.Model}, nil
	default:
		return nil, fmt.Errorf("provider %q: unknown adapter %q", name, p.Adapter)
	}
}

func orDefault(v, def string) string {
	if v == "" {
		return def
	}
	return v
}

func classifyHTTP(status int) error {
	switch {
	case status == http.StatusTooManyRequests:
		return ErrRateLimited
	case status == http.StatusUnauthorized || status == http.StatusForbidden:
		return ErrAuth
	case status >= 500:
		return ErrUnavailable
	default:
		return fmt.Errorf("provider returned HTTP %d", status)
	}
}
