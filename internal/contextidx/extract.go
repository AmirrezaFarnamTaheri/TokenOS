package contextidx

import (
	"regexp"
	"strings"
)

// ExtractSymbols performs fast, language-aware structural extraction. It is
// a pragmatic single-pass scanner (declaration regex + brace/indent block
// tracking) — orders of magnitude cheaper than a full parser and accurate
// enough for surgical context selection.
func ExtractSymbols(file, lang, src string) []Symbol {
	switch lang {
	case "python":
		return extractIndentBlocks(file, lang, src)
	default:
		return extractBraceBlocks(file, lang, src)
	}
}

// declPatterns matches symbol declarations per language family.
var declPatterns = map[string]*regexp.Regexp{
	"go":         regexp.MustCompile(`^\s*(func|type|const|var)\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)`),
	"javascript": regexp.MustCompile(`^\s*(?:export\s+)?(?:default\s+)?(function|class|const|let|var|interface|type|enum)\s+([A-Za-z_$][\w$]*)`),
	"rust":       regexp.MustCompile(`^\s*(?:pub(?:\([^)]*\))?\s+)?(fn|struct|enum|trait|impl|mod|const)\s+([A-Za-z_]\w*)`),
	"java":       regexp.MustCompile(`^\s*(?:public|private|protected|static|final|abstract|\s)*\s*(class|interface|enum|record)\s+([A-Za-z_]\w*)`),
	"c":          regexp.MustCompile(`^\s*(?:static\s+|inline\s+|extern\s+)*[A-Za-z_][\w\*\s]+?\b([A-Za-z_]\w*)\s*\([^;]*$`),
	"ruby":       regexp.MustCompile(`^\s*(def|class|module)\s+([A-Za-z_][\w.?!]*)`),
}

// extractBraceBlocks scans for declarations and captures their brace-balanced
// bodies (for brace-delimited languages).
func extractBraceBlocks(file, lang, src string) []Symbol {
	pat, ok := declPatterns[lang]
	if !ok {
		return nil
	}
	lines := strings.Split(src, "\n")
	var out []Symbol
	for i := 0; i < len(lines); i++ {
		m := pat.FindStringSubmatch(lines[i])
		if m == nil {
			continue
		}
		name, kind := declName(lang, m)
		if name == "" {
			continue
		}
		end := braceBlockEnd(lines, i)
		body := strings.Join(lines[i:end+1], "\n")
		if len(body) > 8000 { // hard cap per symbol; surgical means small
			body = body[:8000] + "\n// ... truncated"
		}
		out = append(out, Symbol{
			File: file, Name: name, Kind: kind, Lang: lang,
			StartLine: i + 1, EndLine: end + 1, Body: body,
		})
		if end > i {
			i = end // skip past the block; nested decls are part of the parent
		}
	}
	return out
}

func declName(lang string, m []string) (name, kind string) {
	switch lang {
	case "go":
		return m[2], m[1]
	case "c":
		return m[1], "func"
	default:
		if len(m) >= 3 {
			return m[2], m[1]
		}
	}
	return "", ""
}

// braceBlockEnd returns the line index where the brace-balanced block that
// starts at (or shortly after) line i closes. Falls back to a short window
// for one-liners and declarations without braces.
func braceBlockEnd(lines []string, i int) int {
	depth := 0
	opened := false
	for j := i; j < len(lines) && j < i+400; j++ {
		for _, ch := range lines[j] {
			switch ch {
			case '{':
				depth++
				opened = true
			case '}':
				depth--
				if opened && depth <= 0 {
					return j
				}
			}
		}
		// Declaration with no opening brace within 3 lines => single-line decl.
		if !opened && j >= i+3 {
			return i
		}
	}
	if !opened {
		return i
	}
	end := i + 400
	if end >= len(lines) {
		end = len(lines) - 1
	}
	return end
}

var pyDecl = regexp.MustCompile(`^(\s*)(def|class)\s+([A-Za-z_]\w*)`)

// extractIndentBlocks handles indentation-scoped languages (Python).
func extractIndentBlocks(file, lang, src string) []Symbol {
	lines := strings.Split(src, "\n")
	var out []Symbol
	for i := 0; i < len(lines); i++ {
		m := pyDecl.FindStringSubmatch(lines[i])
		if m == nil {
			continue
		}
		indent := len(m[1])
		end := i
		for j := i + 1; j < len(lines) && j < i+400; j++ {
			t := lines[j]
			if strings.TrimSpace(t) == "" {
				continue
			}
			cur := len(t) - len(strings.TrimLeft(t, " \t"))
			if cur <= indent {
				break
			}
			end = j
		}
		body := strings.Join(lines[i:end+1], "\n")
		if len(body) > 8000 {
			body = body[:8000] + "\n# ... truncated"
		}
		kind := "func"
		if m[2] == "class" {
			kind = "class"
		}
		out = append(out, Symbol{
			File: file, Name: m[3], Kind: kind, Lang: lang,
			StartLine: i + 1, EndLine: end + 1, Body: body,
		})
	}
	return out
}
