// Command tokenos is the Token-Optimal Agent Execution Kernel CLI.
//
// Subcommands:
//
//	run       execute a task through the kernel
//	route     preview the deterministic routing decision (zero cost)
//	index     build the surgical context index for a workspace
//	providers list provider profiles and filter-matrix results
//	telemetry show route/provider effectiveness and cost-per-success
//	tasks     list compressed task states
//	trace     replay the flight recorder for a task
//	config    print or initialize configuration
//	serve     launch the web control panel
package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"strings"
	"syscall"

	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/contextidx"
	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/engine"
)

const version = "1.0.0"

func main() {
	if len(os.Args) < 2 {
		usage()
		os.Exit(2)
	}
	cmd, args := os.Args[1], os.Args[2:]
	var err error
	switch cmd {
	case "run":
		err = cmdRun(args)
	case "route":
		err = cmdRoute(args)
	case "index":
		err = cmdIndex(args)
	case "providers":
		err = cmdProviders(args)
	case "telemetry":
		err = cmdTelemetry(args)
	case "tasks":
		err = cmdTasks(args)
	case "trace":
		err = cmdTrace(args)
	case "config":
		err = cmdConfig(args)
	case "serve":
		err = cmdServe(args)
	case "version", "--version", "-v":
		fmt.Println("tokenos", version)
	case "help", "--help", "-h":
		usage()
	default:
		fmt.Fprintf(os.Stderr, "unknown command %q\n\n", cmd)
		usage()
		os.Exit(2)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(1)
	}
}

func usage() {
	fmt.Print(`tokenos — Token-Optimal Agent Execution Kernel

Usage:
  tokenos <command> [flags]

Commands:
  run        Execute a task through the kernel
  route      Preview the routing decision (deterministic, zero tokens)
  index      Build the surgical context index for a workspace
  providers  List provider profiles and model filter results
  telemetry  Route/provider effectiveness; cost per successful task
  tasks      List compressed task states
  trace      Replay the flight recorder for a task
  config     Print effective config or write defaults (config init)
  serve      Launch the web control panel
  version    Print version

Common flags (run/route/serve):
  --config PATH     config file (default ~/.config/tokenos/config.yaml)
  --db PATH         state database (default ~/.local/share/tokenos/tokenos.db)
  --workspace DIR   index this workspace for surgical context
  --dry-run         force the offline mock adapter (zero live tokens)

Examples:
  tokenos run "Fix the auth timeout bug" --workspace . --dry-run
  tokenos route "rename variable foo to bar"
  tokenos serve --port 8080 --dry-run
`)
}

// engineFlags defines flags shared by run/route/serve.
type engineFlags struct {
	config    string
	db        string
	traces    string
	workspace string
	dryRun    bool
}

func addEngineFlags(fs *flag.FlagSet) *engineFlags {
	ef := &engineFlags{}
	fs.StringVar(&ef.config, "config", "", "config file path")
	fs.StringVar(&ef.db, "db", "", "state database path")
	fs.StringVar(&ef.traces, "traces", "", "flight recorder directory")
	fs.StringVar(&ef.workspace, "workspace", "", "workspace to index for surgical context")
	fs.BoolVar(&ef.dryRun, "dry-run", false, "use offline mock adapter")
	return ef
}

// parseAnywhere parses flags regardless of position: Go's flag package stops
// at the first positional argument, so `tokenos run "goal" --dry-run` would
// otherwise swallow trailing flags into the goal text. This loops Parse until
// all flag-looking tokens are consumed, collecting positionals in order.
func parseAnywhere(fs *flag.FlagSet, args []string) []string {
	var positional []string
	for len(args) > 0 {
		fs.Parse(args)
		args = fs.Args()
		if len(args) == 0 {
			break
		}
		positional = append(positional, args[0])
		args = args[1:]
	}
	return positional
}

func buildEngine(ef *engineFlags) (*engine.Engine, error) {
	eng, err := engine.New(engine.Options{
		ConfigPath: ef.config,
		DBPath:     ef.db,
		TraceDir:   ef.traces,
		DryRun:     ef.dryRun,
	})
	if err != nil {
		return nil, err
	}
	if ef.workspace != "" {
		ix, err := contextidx.Open(":memory:")
		if err != nil {
			eng.Close()
			return nil, err
		}
		n, err := ix.IndexWorkspace(ef.workspace)
		if err != nil {
			ix.Close()
			eng.Close()
			return nil, err
		}
		fmt.Fprintf(os.Stderr, "indexed %d symbols from %s\n", n, ef.workspace)
		eng.Indexer = ix
	}
	return eng, nil
}

func cmdRun(args []string) error {
	fs := flag.NewFlagSet("run", flag.ExitOnError)
	ef := addEngineFlags(fs)
	constraints := fs.String("constraints", "", "semicolon-separated constraints")
	asJSON := fs.Bool("json", false, "emit full result as JSON")
	task := strings.TrimSpace(strings.Join(parseAnywhere(fs, args), " "))
	if task == "" {
		return fmt.Errorf("usage: tokenos run \"task description\" [flags]")
	}

	eng, err := buildEngine(ef)
	if err != nil {
		return err
	}
	defer eng.Close()

	var cons []string
	for _, c := range strings.Split(*constraints, ";") {
		if c = strings.TrimSpace(c); c != "" {
			cons = append(cons, c)
		}
	}

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	res, runErr := eng.Run(ctx, task, cons)
	if *asJSON {
		enc := json.NewEncoder(os.Stdout)
		enc.SetIndent("", "  ")
		enc.Encode(res)
	} else if res != nil {
		fmt.Printf("task     %s\nroute    %s  (%s)\n", res.TaskID, res.Route, res.Reason)
		if res.Provider != "" {
			fmt.Printf("provider %s / %s\n", res.Provider, res.Model)
		}
		fmt.Printf("tokens   in=%d out=%d   latency=%dms   cost=$%.6f   retries=%d\n",
			res.TokensIn, res.TokensOut, res.LatencyMS, res.CostUSD, res.Retries)
		fmt.Println(strings.Repeat("-", 60))
		fmt.Println(res.Output)
	}
	return runErr
}

func cmdRoute(args []string) error {
	fs := flag.NewFlagSet("route", flag.ExitOnError)
	ef := addEngineFlags(fs)
	task := strings.TrimSpace(strings.Join(parseAnywhere(fs, args), " "))
	if task == "" {
		return fmt.Errorf("usage: tokenos route \"task description\"")
	}
	eng, err := buildEngine(ef)
	if err != nil {
		return err
	}
	defer eng.Close()

	dec, _ := eng.RouteOnly(task)
	chain := eng.Cfg.ProviderChain(string(dec.Route))
	fmt.Printf("route       %s\nreason      %s\nconfidence  %.2f\nest tokens  %d\nchain       %s\n",
		dec.Route, dec.Reason, dec.Signals.Confidence, dec.Signals.EstimatedTokens,
		strings.Join(chain, " -> "))
	return nil
}
