package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/config"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/contextidx"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/provider"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/webui"
)

func cmdIndex(args []string) error {
	fs := flag.NewFlagSet("index", flag.ExitOnError)
	out := fs.String("out", "", "index database path (default: in-memory test run)")
	query := fs.String("query", "", "optional test query against the fresh index")
	pos := parseAnywhere(fs, args)
	root := "."
	if len(pos) > 0 {
		root = pos[0]
	}
	ix, err := contextidx.Open(*out)
	if err != nil {
		return err
	}
	defer ix.Close()
	n, err := ix.IndexWorkspace(root)
	if err != nil {
		return err
	}
	fmt.Printf("indexed %d symbols from %s\n", n, root)
	if *query != "" {
		syms, err := ix.Search(*query, 5)
		if err != nil {
			return err
		}
		for _, s := range syms {
			fmt.Printf("  %s:%d-%d  [%s] %s\n", s.File, s.StartLine, s.EndLine, s.Kind, s.Name)
		}
	}
	return nil
}

func cmdProviders(args []string) error {
	fs := flag.NewFlagSet("providers", flag.ExitOnError)
	cfgPath := fs.String("config", "", "config file path")
	fs.Parse(args)
	cfg, err := config.Load(*cfgPath)
	if err != nil {
		return err
	}
	fmt.Printf("%-12s %-10s %-9s %-28s %s\n", "PROVIDER", "ADAPTER", "ENABLED", "MODEL", "FILTER VERDICT")
	for name, p := range cfg.Providers {
		enabled := "yes"
		if p.Disabled {
			enabled = "no"
		}
		verdict := "-"
		if p.Model != "" {
			if p.Models.IsModelAllowed(p.Model) {
				verdict = "ALLOWED"
			} else {
				verdict = "BLOCKED by filter matrix"
			}
		}
		fmt.Printf("%-12s %-10s %-9s %-28s %s\n", name, p.Adapter, enabled, p.Model, verdict)
		if a, err := provider.New(name, p); err == nil {
			allowed := p.Models.Filter(a.Models())
			if len(allowed) > 0 {
				fmt.Printf("%-12s   manifest: %s\n", "", strings.Join(allowed, ", "))
			}
		}
	}
	return nil
}

func cmdTelemetry(args []string) error {
	fs := flag.NewFlagSet("telemetry", flag.ExitOnError)
	ef := addEngineFlags(fs)
	fs.Parse(args)
	eng, err := buildEngine(ef)
	if err != nil {
		return err
	}
	defer eng.Close()

	sum, err := eng.Store.GetSummary()
	if err != nil {
		return err
	}
	fmt.Printf("tasks=%d  executions=%d  successes=%d  success_rate=%.1f%%\n",
		sum.Tasks, sum.Executions, sum.Successes, sum.OverallSuccessPct*100)
	fmt.Printf("total_tokens=%d  total_cost=$%.6f  avg_latency=%.0fms\n",
		sum.TotalTokens, sum.TotalCostUSD, sum.AvgLatencyMS)
	fmt.Printf("EFFECTIVE COST PER SUCCESSFUL TASK: $%.6f\n\n", sum.CostPerSuccess)

	routes, err := eng.Store.StatsByRoute()
	if err != nil {
		return err
	}
	if len(routes) > 0 {
		fmt.Printf("%-20s %6s %9s %10s %10s %12s %14s\n",
			"ROUTE", "RUNS", "SUCCESS", "AVG_IN", "AVG_OUT", "AVG_LAT_MS", "COST/SUCCESS")
		for _, r := range routes {
			fmt.Printf("%-20s %6d %8.1f%% %10.0f %10.0f %12.0f %14.6f\n",
				r.Route, r.Runs, r.SuccessRate*100, r.AvgTokensIn, r.AvgTokensOut,
				r.AvgLatencyMS, r.CostPerSuccess)
		}
	}
	return nil
}

func cmdTasks(args []string) error {
	fs := flag.NewFlagSet("tasks", flag.ExitOnError)
	ef := addEngineFlags(fs)
	limit := fs.Int("limit", 20, "max tasks to show")
	fs.Parse(args)
	eng, err := buildEngine(ef)
	if err != nil {
		return err
	}
	defer eng.Close()

	tasks, err := eng.Store.ListTasks(*limit)
	if err != nil {
		return err
	}
	if len(tasks) == 0 {
		fmt.Println("no tasks recorded")
		return nil
	}
	for _, t := range tasks {
		blocked := ""
		if t.Blocked {
			blocked = "  [BLOCKED]"
		}
		goal := t.Goal
		if len(goal) > 70 {
			goal = goal[:67] + "..."
		}
		fmt.Printf("%-18s %-12s %s%s\n", t.TaskID, t.Status, goal, blocked)
	}
	return nil
}

func cmdTrace(args []string) error {
	fs := flag.NewFlagSet("trace", flag.ExitOnError)
	ef := addEngineFlags(fs)
	showBlobs := fs.Bool("blobs", false, "print full payload blobs")
	pos := parseAnywhere(fs, args)
	if len(pos) == 0 {
		return fmt.Errorf("usage: tokenos trace <task-id>")
	}
	taskID := pos[0]
	eng, err := buildEngine(ef)
	if err != nil {
		return err
	}
	defer eng.Close()

	events, err := eng.Recorder.Events(taskID)
	if err != nil {
		return err
	}
	if len(events) == 0 {
		fmt.Println("no flight-recorder events for task", taskID)
		return nil
	}
	for _, ev := range events {
		fmt.Printf("%s  %-9s %s\n", ev.Timestamp.Format(time.TimeOnly), ev.Kind, ev.Summary)
		if *showBlobs && ev.BlobSHA != "" {
			if blob, err := eng.Recorder.Blob(ev.BlobSHA); err == nil {
				fmt.Println("  +- blob", ev.BlobSHA[:12])
				for _, line := range strings.Split(string(blob), "\n") {
					fmt.Println("  |", line)
				}
				fmt.Println("  +-")
			}
		}
	}
	return nil
}

func cmdConfig(args []string) error {
	fs := flag.NewFlagSet("config", flag.ExitOnError)
	cfgPath := fs.String("config", "", "config file path")
	pos := parseAnywhere(fs, args)

	if len(pos) > 0 && pos[0] == "init" {
		path := *cfgPath
		if path == "" {
			path = config.DefaultPath()
		}
		if _, err := os.Stat(path); err == nil {
			return fmt.Errorf("config already exists at %s", path)
		}
		if err := config.Default().Save(path); err != nil {
			return err
		}
		fmt.Println("wrote default config to", path)
		return nil
	}

	cfg, err := config.Load(*cfgPath)
	if err != nil {
		return err
	}
	enc := json.NewEncoder(os.Stdout)
	enc.SetIndent("", "  ")
	return enc.Encode(cfg)
}

func cmdServe(args []string) error {
	fs := flag.NewFlagSet("serve", flag.ExitOnError)
	ef := addEngineFlags(fs)
	port := fs.Int("port", 8080, "listen port")
	host := fs.String("host", "0.0.0.0", "listen host")
	fs.Parse(args)

	eng, err := buildEngine(ef)
	if err != nil {
		return err
	}
	defer eng.Close()

	srv := &http.Server{
		Addr:              fmt.Sprintf("%s:%d", *host, *port),
		Handler:           webui.NewServer(eng).Handler(),
		ReadHeaderTimeout: 10 * time.Second,
	}
	fmt.Printf("TokenOS control panel listening on http://%s:%d (dry-run=%v)\n", *host, *port, ef.dryRun)

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	go func() {
		<-ctx.Done()
		shutCtx, c2 := context.WithTimeout(context.Background(), 5*time.Second)
		defer c2()
		srv.Shutdown(shutCtx)
	}()
	if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
		return err
	}
	return nil
}
