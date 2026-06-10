package verify

import "testing"

func TestStaticCheckEmpty(t *testing.T) {
	r := StaticCheck("IMPLEMENT", "   ")
	if r.Pass {
		t.Error("empty output must fail")
	}
}

func TestStaticCheckPatchContract(t *testing.T) {
	good := "--- a/f.go\n+++ b/f.go\n@@ -1 +1 @@\n-old\n+new"
	if r := StaticCheck("PATCH", good); !r.Pass {
		t.Errorf("valid diff must pass: %v", r.Issues)
	}
	if r := StaticCheck("PATCH", "here is some prose instead of a diff"); r.Pass {
		t.Error("non-diff PATCH output must fail")
	}
}

func TestStaticCheckAskContract(t *testing.T) {
	if r := StaticCheck("ASK", "Which database should the cache layer use?"); !r.Pass {
		t.Errorf("single question must pass: %v", r.Issues)
	}
	if r := StaticCheck("ASK", "no question here at all"); r.Pass {
		t.Error("ASK without question must fail")
	}
	multi := "Which DB?\nAnd which region?\n"
	if r := StaticCheck("ASK", multi); r.Pass {
		t.Error("ASK with two questions must fail")
	}
}

func TestStaticCheckBraceBalance(t *testing.T) {
	truncated := "func main() {\n\tif x {\n\t\tdoThing()\n" // missing two closers
	if r := StaticCheck("IMPLEMENT", truncated); r.Pass {
		t.Error("unbalanced code must fail")
	}
	complete := "func main() {\n\tif x {\n\t\tdoThing()\n\t}\n}"
	if r := StaticCheck("IMPLEMENT", complete); !r.Pass {
		t.Errorf("balanced code must pass: %v", r.Issues)
	}
}

func TestStaticCheckIgnoresBracesInStrings(t *testing.T) {
	code := "func f() {\n\ts := \"{{{\"\n\treturn s\n}"
	if r := StaticCheck("IMPLEMENT", code); !r.Pass {
		t.Errorf("braces inside strings must not count: %v", r.Issues)
	}
}
