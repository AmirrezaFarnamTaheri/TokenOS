package provider

import (
	"context"
	"fmt"
	"strings"
	"sync/atomic"
	"time"

	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/tokenizer"
)

// Mock is the offline short-circuit adapter: deterministic responses for
// smoke-testing routing, failover, quota tracking and telemetry without
// burning a single live token. Supports scripted fault injection.
type Mock struct {
	name  string
	calls atomic.Int64

	// Fault injection (used by tests and `--dry-run` flows).
	FailEveryN int           // every Nth call returns ErrRateLimited
	Latency    time.Duration // artificial latency
	Canned     string        // fixed response body (otherwise synthesized)
}

// NewMock creates a mock adapter.
func NewMock(name string) *Mock { return &Mock{name: name} }

// Name implements Adapter.
func (m *Mock) Name() string { return m.name }

// Models implements Adapter.
func (m *Mock) Models() []string { return []string{"mock-1", "mock-large"} }

// Execute implements Adapter with deterministic synthesized output.
func (m *Mock) Execute(ctx context.Context, req Request) (*Response, error) {
	n := m.calls.Add(1)
	if m.Latency > 0 {
		select {
		case <-time.After(m.Latency):
		case <-ctx.Done():
			return nil, ctx.Err()
		}
	}
	if m.FailEveryN > 0 && n%int64(m.FailEveryN) == 0 {
		return nil, ErrRateLimited
	}

	body := m.Canned
	if body == "" {
		goal := extractLine(req.Prompt, "GOAL: ")
		switch req.Route {
		case "ASK":
			body = fmt.Sprintf("What is the single most critical unspecified detail required to complete: %q?", goal)
		case "PATCH":
			body = "--- a/target\n+++ b/target\n@@ -1,1 +1,1 @@\n-// before\n+// after (mock patch for: " + goal + ")"
		case "VERIFY":
			body = "VERIFICATION: PASS (mock static checks + targeted tests)"
		default:
			body = fmt.Sprintf("[mock:%s] completed route %s for goal: %s", m.name, req.Route, goal)
		}
	}
	return &Response{
		Text:      body,
		TokensIn:  tokenizer.Estimate(req.Prompt),
		TokensOut: tokenizer.Estimate(body),
		Model:     "mock-1",
	}, nil
}

func extractLine(s, prefix string) string {
	for _, line := range strings.Split(s, "\n") {
		if strings.HasPrefix(line, prefix) {
			return strings.TrimSpace(strings.TrimPrefix(line, prefix))
		}
	}
	return "(unspecified)"
}
