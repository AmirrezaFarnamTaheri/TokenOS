package tokenizer

import (
	"strings"
	"testing"
)

func TestEstimateBasics(t *testing.T) {
	if Estimate("") != 0 {
		t.Error("empty string must be 0 tokens")
	}
	if Estimate("a") < 1 {
		t.Error("non-empty must be at least 1 token")
	}
	// ~4 chars/token for prose: 400 chars ≈ 100 tokens (±50%).
	prose := strings.Repeat("the quick brown fox jumps over the lazy dog ", 10)
	n := Estimate(prose)
	if n < 60 || n > 160 {
		t.Errorf("prose estimate %d outside sane band for %d chars", n, len(prose))
	}
}

func TestEstimateCodeDenser(t *testing.T) {
	code := strings.Repeat("if(x){y+=1;}else{z-=2;}\n", 20)
	prose := strings.Repeat("hello world and more text here ok\n", 20)
	// Code (symbol-dense) should produce more tokens per char.
	cr := float64(Estimate(code)) / float64(len(code))
	pr := float64(Estimate(prose)) / float64(len(prose))
	if cr <= pr {
		t.Errorf("code density %f should exceed prose %f", cr, pr)
	}
}

func TestTruncate(t *testing.T) {
	text := strings.Repeat("line of reasonable content here\n", 200)
	out := Truncate(text, 50)
	if Estimate(out) > 50 {
		t.Errorf("truncated text estimates %d > budget 50", Estimate(out))
	}
	if out == "" {
		t.Error("truncation must not produce empty output for positive budget")
	}
	// Within budget => unchanged.
	if Truncate("short", 100) != "short" {
		t.Error("text within budget must be unchanged")
	}
}

func TestFitsBudget(t *testing.T) {
	if !FitsBudget("tiny", 10) {
		t.Error("tiny text fits 10 tokens")
	}
	if FitsBudget(strings.Repeat("x", 10000), 10) {
		t.Error("huge text must not fit 10 tokens")
	}
}
