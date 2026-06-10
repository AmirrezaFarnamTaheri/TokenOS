// Package webui serves the embedded TokenOS control panel: a zero-dependency
// single-page dashboard plus a JSON API over the engine. The GUI is local
// telemetry/observability tooling — it never adds tokens to any execution.
package webui

import (
	"context"
	"embed"
	"encoding/json"
	"io/fs"
	"net/http"
	"sync"
	"time"

	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/engine"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/payload"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/tokenizer"
)

//go:embed static
var staticFS embed.FS

// Server is the HTTP control panel.
type Server struct {
	eng *engine.Engine
	mu  sync.Mutex // engine adapters/SQLite guarded for concurrent API calls
}

// NewServer wraps an engine.
func NewServer(eng *engine.Engine) *Server { return &Server{eng: eng} }

// Handler returns the full HTTP handler (static UI + JSON API).
func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()

	static, _ := fs.Sub(staticFS, "static")
	mux.Handle("/", http.FileServer(http.FS(static)))

	mux.HandleFunc("GET /api/summary", s.handleSummary)
	mux.HandleFunc("GET /api/stats/routes", s.handleRouteStats)
	mux.HandleFunc("GET /api/stats/providers", s.handleProviderStats)
	mux.HandleFunc("GET /api/executions", s.handleExecutions)
	mux.HandleFunc("GET /api/tasks", s.handleTasks)
	mux.HandleFunc("GET /api/config", s.handleConfig)
	mux.HandleFunc("GET /api/traces/{taskID}", s.handleTraces)
	mux.HandleFunc("POST /api/route", s.handleRoutePreview)
	mux.HandleFunc("POST /api/run", s.handleRun)

	return logRequests(mux)
}

func logRequests(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		next.ServeHTTP(w, r)
	})
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(v)
}

func writeErr(w http.ResponseWriter, status int, msg string) {
	writeJSON(w, status, map[string]string{"error": msg})
}

func (s *Server) handleSummary(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	defer s.mu.Unlock()
	sum, err := s.eng.Store.GetSummary()
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, sum)
}

func (s *Server) handleRouteStats(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	defer s.mu.Unlock()
	stats, err := s.eng.Store.StatsByRoute()
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, stats)
}

func (s *Server) handleProviderStats(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	defer s.mu.Unlock()
	stats, err := s.eng.Store.StatsByProvider()
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, stats)
}

func (s *Server) handleExecutions(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	defer s.mu.Unlock()
	execs, err := s.eng.Store.ListExecutions(200)
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, execs)
}

func (s *Server) handleTasks(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	defer s.mu.Unlock()
	tasks, err := s.eng.Store.ListTasks(100)
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, tasks)
}

func (s *Server) handleConfig(w http.ResponseWriter, r *http.Request) {
	// Config is exposed read-only; API keys live in env vars, never here.
	writeJSON(w, 200, s.eng.Cfg)
}

func (s *Server) handleTraces(w http.ResponseWriter, r *http.Request) {
	taskID := r.PathValue("taskID")
	events, err := s.eng.Recorder.Events(taskID)
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, events)
}

type taskRequest struct {
	Task        string   `json:"task"`
	Constraints []string `json:"constraints,omitempty"`
}

func (s *Server) handleRoutePreview(w http.ResponseWriter, r *http.Request) {
	var req taskRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil || req.Task == "" {
		writeErr(w, 400, "body must be {\"task\": \"...\"}")
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	dec, ctxBlock := s.eng.RouteOnly(req.Task)
	chain := s.eng.Cfg.ProviderChain(string(dec.Route))
	writeJSON(w, 200, map[string]any{
		"decision":       dec,
		"provider_chain": chain,
		"context_tokens": tokenizer.Estimate(ctxBlock),
		"prompt_tokens":  tokenizer.Estimate(payload.KernelContract) + tokenizer.Estimate(req.Task) + tokenizer.Estimate(ctxBlock),
	})
}

func (s *Server) handleRun(w http.ResponseWriter, r *http.Request) {
	var req taskRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil || req.Task == "" {
		writeErr(w, 400, "body must be {\"task\": \"...\"}")
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	ctx, cancel := context.WithTimeout(r.Context(), 5*time.Minute)
	defer cancel()
	res, err := s.eng.Run(ctx, req.Task, req.Constraints)
	if err != nil && res == nil {
		writeErr(w, 500, err.Error())
		return
	}
	out := map[string]any{"result": res}
	if err != nil {
		out["error"] = err.Error()
	}
	writeJSON(w, 200, out)
}
