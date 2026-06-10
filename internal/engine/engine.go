// Package engine is the orchestration layer: deterministic routing, shadow
// pricing, failover, verification, telemetry and flight recording around
// dumb worker adapters. The workers are not smart — this layer is.
package engine

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"time"

	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/config"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/contextidx"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/kernel"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/payload"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/pricing"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/provider"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/recorder"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/store"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/tokenizer"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/verify"
)

// Engine wires every subsystem together.
type Engine struct {
	Cfg      *config.Config
	Store    *store.Store
	Recorder *recorder.Recorder
	Tracker  *pricing.Tracker
	Indexer  *contextidx.Indexer // optional; nil when no workspace indexed
	DryRun   bool                // force mock adapter regardless of config

	adapters map[string]provider.Adapter
}

// Options configures engine construction.
type Options struct {
	ConfigPath string
	DBPath     string
	TraceDir   string
	DryRun     bool
}

// New builds an Engine with all subsystems initialized.
func New(opt Options) (*Engine, error) {
	cfg, err := config.Load(opt.ConfigPath)
	if err != nil {
		return nil, err
	}
	st, err := store.Open(opt.DBPath)
	if err != nil {
		return nil, err
	}
	rec, err := recorder.New(opt.TraceDir)
	if err != nil {
		st.Close()
		return nil, err
	}
	return &Engine{
		Cfg:      cfg,
		Store:    st,
		Recorder: rec,
		Tracker:  pricing.NewTracker(),
		DryRun:   opt.DryRun,
		adapters: map[string]provider.Adapter{},
	}, nil
}

// Close releases resources.
func (e *Engine) Close() {
	if e.Store != nil {
		e.Store.Close()
	}
	if e.Indexer != nil {
		e.Indexer.Close()
	}
}

// adapter lazily constructs and caches a provider adapter.
func (e *Engine) adapter(name string) (provider.Adapter, error) {
	if e.DryRun {
		name = "__dryrun__"
	}
	if a, ok := e.adapters[name]; ok {
		return a, nil
	}
	if e.DryRun {
		a := provider.NewMock("dry-run")
		e.adapters[name] = a
		return a, nil
	}
	p, ok := e.Cfg.Providers[name]
	if !ok {
		return nil, fmt.Errorf("unknown provider %q", name)
	}
	a, err := provider.New(name, p)
	if err != nil {
		return nil, err
	}
	e.adapters[name] = a
	return a, nil
}

// RunResult is the complete outcome of one kernel execution.
type RunResult struct {
	TaskID    string          `json:"task_id"`
	Route     kernel.Route    `json:"route"`
	Reason    string          `json:"reason"`
	Provider  string          `json:"provider,omitempty"`
	Model     string          `json:"model,omitempty"`
	Output    string          `json:"output"`
	TokensIn  int             `json:"tokens_in"`
	TokensOut int             `json:"tokens_out"`
	LatencyMS int64           `json:"latency_ms"`
	CostUSD   float64         `json:"cost_usd"`
	Retries   int             `json:"retries"`
	Verified  *verify.Result  `json:"verified,omitempty"`
	Signals   kernel.Signals  `json:"signals"`
	Quotes    []pricing.Quote `json:"quotes,omitempty"`
	Success   bool            `json:"success"`
}

// RouteOnly performs deterministic routing without executing (zero cost).
func (e *Engine) RouteOnly(task string) (kernel.Decision, string) {
	ctxBlock := e.minimumViableContext(task)
	est := tokenizer.Estimate(task) + tokenizer.Estimate(ctxBlock) + tokenizer.Estimate(payload.KernelContract)
	indexHit := ctxBlock != ""
	sig := kernel.ExtractSignals(task, est, indexHit, false, false)
	return kernel.Decide(sig, e.Cfg.Policy), ctxBlock
}

// minimumViableContext queries the surgical index when available.
func (e *Engine) minimumViableContext(task string) string {
	if e.Indexer == nil {
		return ""
	}
	ctxBlock, err := e.Indexer.MinimumViableContext(task, 6)
	if err != nil {
		return ""
	}
	// Context budget: surgical context is capped hard.
	return tokenizer.Truncate(ctxBlock, 2000)
}

func newID() string {
	b := make([]byte, 8)
	if _, err := rand.Read(b); err != nil {
		return fmt.Sprintf("t%d", time.Now().UnixNano())
	}
	return hex.EncodeToString(b)
}

// Run executes a task end-to-end through the kernel.
func (e *Engine) Run(ctx context.Context, task string, constraints []string) (*RunResult, error) {
	taskID := newID()

	// Step 1-2: local state init + context budget enforcement (zero tokens).
	st := &kernel.State{
		TaskID:      taskID,
		Goal:        task,
		Constraints: constraints,
		Status:      kernel.StatusPending,
		CreatedAt:   time.Now().UTC(),
	}
	st.Context = e.minimumViableContext(task)

	// Step 3: failure memory check (local SQLite).
	repeated, _ := e.Store.HasSimilarFailure(taskID, task)

	// Step 4: deterministic routing (zero token cost).
	est := tokenizer.Estimate(task) + tokenizer.Estimate(st.Context) + tokenizer.Estimate(payload.KernelContract)
	sig := kernel.ExtractSignals(task, est, st.Context != "", repeated, false)
	dec := kernel.Decide(sig, e.Cfg.Policy)

	decBlob, _ := json.Marshal(dec)
	e.Recorder.Record(taskID, "decision", string(dec.Route)+": "+dec.Reason, decBlob)

	res := &RunResult{TaskID: taskID, Route: dec.Route, Reason: dec.Reason, Signals: sig}

	st.Status = kernel.StatusRouted
	e.Store.SaveTask(st)

	// Escalations resolve locally with zero network cost.
	if dec.Route.IsEscalation() {
		st.Status = kernel.StatusEscalated
		st.Blocked = true
		st.NextAction = dec.Reason
		e.Store.SaveTask(st)
		res.Output = fmt.Sprintf("%s: %s", dec.Route, dec.Reason)
		res.Success = true // escalating correctly IS the success condition
		e.record(res, 0)
		return res, nil
	}

	// Step 5: payload serialization (static→dynamic, conclusions only).
	prompt := payload.Build(dec.Route, st)

	// Step 6: shadow pricing across the provider chain, then execute with
	// deterministic failover.
	chain := e.Cfg.ProviderChain(string(dec.Route))
	if len(chain) == 0 {
		return nil, errors.New("no enabled providers for route " + string(dec.Route))
	}
	quotes := e.quote(chain, sig.Confidence, est)
	res.Quotes = quotes

	st.Status = kernel.StatusInProgress
	e.Store.SaveTask(st)

	var lastErr error
	for _, provName := range orderedProviders(quotes, chain) {
		a, err := e.adapter(provName)
		if err != nil {
			lastErr = err
			continue
		}
		pCfg := e.Cfg.Providers[provName]
		model := e.resolveModel(provName, a)
		if model == "" {
			lastErr = fmt.Errorf("provider %q: no model passes the filter matrix", provName)
			continue
		}

		e.Recorder.Record(taskID, "prompt", "→ "+provName+"/"+model, []byte(prompt))

		start := time.Now()
		callCtx, cancel := context.WithTimeout(ctx, e.timeoutFor(string(dec.Route)))
		resp, err := a.Execute(callCtx, provider.Request{
			Route:  string(dec.Route),
			Prompt: prompt,
			Model:  model,
			MaxOut: 4096,
		})
		cancel()
		lat := time.Since(start).Milliseconds()
		e.Tracker.Record(provName, float64(lat), err == nil)

		if err != nil {
			lastErr = err
			res.Retries++
			e.Recorder.Record(taskID, "error", provName+": "+err.Error(), nil)
			e.Store.RecordFailure(taskID, "execute via "+provName, err.Error())
			continue // deterministic failover to next quote
		}

		out := payload.ExtractSolution(resp.Text)
		e.Recorder.Record(taskID, "response", "← "+provName, []byte(resp.Text))

		// Step 7: tiered verification — static first, zero token cost.
		v := verify.StaticCheck(string(dec.Route), out)
		res.Verified = &v
		if !v.Pass {
			// Fast local loopback: remember failure, try next provider.
			reason := "static verification failed: " + fmt.Sprint(v.Issues)
			st.RememberFailure("output from "+provName, reason)
			e.Store.RecordFailure(taskID, "output from "+provName, reason)
			e.Recorder.Record(taskID, "verify", reason, []byte(out))
			res.Retries++
			lastErr = errors.New(reason)
			continue
		}

		tokensIn := resp.TokensIn
		if tokensIn == 0 {
			tokensIn = tokenizer.Estimate(prompt)
		}
		tokensOut := resp.TokensOut
		if tokensOut == 0 {
			tokensOut = tokenizer.Estimate(out)
		}

		res.Provider = provName
		res.Model = resp.Model
		res.Output = out
		res.TokensIn = tokensIn
		res.TokensOut = tokensOut
		res.LatencyMS = lat
		res.CostUSD = (float64(tokensIn)*pCfg.CostPerMTokIn + float64(tokensOut)*pCfg.CostPerMTokOut) / 1e6
		res.Success = true

		// Stop rule: acceptance satisfied, no known blocker => stop now.
		if dec.Route == kernel.RouteAsk {
			st.Status = kernel.StatusBlocked
			st.Blocked = true
			st.NextAction = "answer the question: " + out
		} else {
			st.Status = kernel.StatusDone
			st.NextAction = ""
		}
		e.Store.SaveTask(st)
		e.record(res, lat)
		return res, nil
	}

	st.Status = kernel.StatusFailed
	e.Store.SaveTask(st)
	e.record(res, res.LatencyMS)
	if lastErr == nil {
		lastErr = errors.New("all providers exhausted")
	}
	return res, fmt.Errorf("execution failed after %d attempt(s): %w", res.Retries+1, lastErr)
}

// quote runs shadow pricing over the provider chain.
func (e *Engine) quote(chain []string, confidence float64, estIn int) []pricing.Quote {
	cands := make([]pricing.Candidate, 0, len(chain))
	quota := map[string]int{}
	for _, name := range chain {
		p := e.Cfg.Providers[name]
		cands = append(cands, pricing.Candidate{
			Provider:       name,
			Model:          p.Model,
			CostPerMTokIn:  p.CostPerMTokIn,
			CostPerMTokOut: p.CostPerMTokOut,
			MaxContext:     p.MaxContext,
			Priority:       p.Priority,
		})
		quota[name] = p.QuotaPerMin
	}
	w := pricing.Weights{Alpha: e.Cfg.Pricing.Alpha, Beta: e.Cfg.Pricing.Beta}
	if w.Alpha == 0 && w.Beta == 0 {
		w = pricing.DefaultWeights()
	}
	return pricing.QuoteAll(cands, confidence, estIn, 1024, w, e.Tracker, quota)
}

// orderedProviders prefers shadow-priced order, falling back to chain order
// for providers the pricer filtered out (context overflow).
func orderedProviders(quotes []pricing.Quote, chain []string) []string {
	seen := map[string]bool{}
	var out []string
	for _, q := range quotes {
		if !seen[q.Provider] {
			seen[q.Provider] = true
			out = append(out, q.Provider)
		}
	}
	for _, name := range chain {
		if !seen[name] {
			seen[name] = true
			out = append(out, name)
		}
	}
	return out
}

// resolveModel applies the two-tier filter matrix to the adapter's manifest.
func (e *Engine) resolveModel(provName string, a provider.Adapter) string {
	if e.DryRun {
		return "mock-1"
	}
	p := e.Cfg.Providers[provName]
	models := a.Models()
	if p.Model != "" {
		models = append([]string{p.Model}, models...)
	}
	for _, m := range models {
		if p.Models.IsModelAllowed(m) {
			return m
		}
	}
	return ""
}

func (e *Engine) timeoutFor(route string) time.Duration {
	for _, r := range e.Cfg.Routing {
		for _, t := range r.RouteTypes {
			if (t == "*" || t == route) && r.TimeoutMS > 0 {
				return time.Duration(r.TimeoutMS) * time.Millisecond
			}
		}
	}
	return 2 * time.Minute
}

func (e *Engine) record(r *RunResult, latencyMS int64) {
	e.Store.RecordExecution(store.Execution{
		TaskID:     r.TaskID,
		Route:      string(r.Route),
		Provider:   r.Provider,
		Model:      r.Model,
		TokensIn:   r.TokensIn,
		TokensOut:  r.TokensOut,
		LatencyMS:  latencyMS,
		Retries:    r.Retries,
		EstCostUSD: r.CostUSD,
		Success:    r.Success,
	})
}
