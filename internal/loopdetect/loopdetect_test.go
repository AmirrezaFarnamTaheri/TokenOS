package loopdetect

import (
	"strings"
	"testing"
)

func TestNormalizedDistance(t *testing.T) {
	if d := NormalizedDistance("abc", "abc"); d != 0 {
		t.Errorf("identical strings distance = %f, want 0", d)
	}
	if d := NormalizedDistance("abc", "xyz"); d != 1 {
		t.Errorf("fully different distance = %f, want 1", d)
	}
	if d := NormalizedDistance("", "abc"); d != 1 {
		t.Errorf("empty vs non-empty = %f, want 1", d)
	}
	// kitten -> sitting = 3 edits / 7 max
	got := NormalizedDistance("kitten", "sitting")
	want := 3.0 / 7.0
	if got < want-0.001 || got > want+0.001 {
		t.Errorf("kitten/sitting = %f, want %f", got, want)
	}
}

func TestDetectorCatchesSemanticLoop(t *testing.T) {
	d := New()
	base := strings.Repeat("patch attempt with retry logic and variable renaming ", 20)
	if d.Observe(base) {
		t.Error("first attempt must not be a loop")
	}
	// Tiny variation (<3% change) => loop.
	variant := strings.Replace(base, "retry", "retri", 1)
	if !d.Observe(variant) {
		t.Error("near-identical attempt must trigger loop detection")
	}
	// Completely different attempt => no loop.
	if d.Observe(strings.Repeat("entirely new strategy using async queue draining ", 20)) {
		t.Error("genuinely different attempt must not loop")
	}
}

func TestDetectorWindow(t *testing.T) {
	d := New()
	d.Window = 2
	d.Observe("aaaa")
	d.Observe("bbbb")
	d.Observe("cccc") // evicts aaaa
	if d.Observe("aaaa") {
		t.Error("evicted history must not trigger loop")
	}
}

func TestDetectorReset(t *testing.T) {
	d := New()
	d.Observe("same attempt payload here")
	d.Reset()
	if d.Observe("same attempt payload here") {
		t.Error("reset must clear history")
	}
}
