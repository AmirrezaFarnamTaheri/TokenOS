package contextidx

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestExtractGoSymbols(t *testing.T) {
	src := `package main

func ValidateToken(tok string) bool {
	if tok == "" {
		return false
	}
	return true
}

type Session struct {
	ID string
}
`
	syms := ExtractSymbols("auth.go", "go", src)
	if len(syms) < 2 {
		t.Fatalf("want >=2 symbols, got %d: %+v", len(syms), syms)
	}
	var foundFunc, foundType bool
	for _, s := range syms {
		if s.Name == "ValidateToken" && s.Kind == "func" {
			foundFunc = true
			if !strings.Contains(s.Body, "return true") {
				t.Error("function body must include full block")
			}
		}
		if s.Name == "Session" && s.Kind == "type" {
			foundType = true
		}
	}
	if !foundFunc || !foundType {
		t.Errorf("missing symbols: func=%v type=%v", foundFunc, foundType)
	}
}

func TestExtractPythonSymbols(t *testing.T) {
	src := `import os

def validate_token(tok):
    if not tok:
        return False
    return True

class Session:
    def __init__(self):
        self.id = None
`
	syms := ExtractSymbols("auth.py", "python", src)
	names := map[string]bool{}
	for _, s := range syms {
		names[s.Name] = true
	}
	if !names["validate_token"] || !names["Session"] {
		t.Errorf("python extraction missing symbols: %+v", syms)
	}
}

func TestIndexAndSurgicalQuery(t *testing.T) {
	dir := t.TempDir()
	os.WriteFile(filepath.Join(dir, "auth.go"), []byte(`package auth

func ValidateToken(tok string) bool {
	return tok != ""
}

func RefreshSession(id string) error {
	return nil
}
`), 0o644)
	os.WriteFile(filepath.Join(dir, "billing.go"), []byte(`package billing

func ChargeCustomer(amount int) error {
	return nil
}
`), 0o644)

	ix, err := Open(":memory:")
	if err != nil {
		t.Fatal(err)
	}
	defer ix.Close()
	n, err := ix.IndexWorkspace(dir)
	if err != nil {
		t.Fatal(err)
	}
	if n < 3 {
		t.Fatalf("want >=3 symbols indexed, got %d", n)
	}

	ctx, err := ix.MinimumViableContext("fix the ValidateToken bug", 3)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(ctx, "ValidateToken") {
		t.Errorf("surgical context must contain target symbol, got: %s", ctx)
	}
	if strings.Contains(ctx, "ChargeCustomer") {
		t.Errorf("surgical context must NOT contain unrelated symbols")
	}
}

func TestSkipDirs(t *testing.T) {
	dir := t.TempDir()
	os.MkdirAll(filepath.Join(dir, "node_modules", "pkg"), 0o755)
	os.WriteFile(filepath.Join(dir, "node_modules", "pkg", "junk.js"),
		[]byte("function junkFn() { return 1 }"), 0o644)
	ix, _ := Open(":memory:")
	defer ix.Close()
	n, _ := ix.IndexWorkspace(dir)
	if n != 0 {
		t.Errorf("node_modules must be skipped, indexed %d", n)
	}
}
