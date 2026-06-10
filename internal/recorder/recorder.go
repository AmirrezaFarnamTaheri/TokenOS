// Package recorder implements the out-of-band Flight Recorder: while the
// LLM payloads stay pruned and cheap, the full forensic trail (prompts,
// raw responses, routing decisions, rejected alternatives) is written to a
// local content-addressable store for human debugging.
//
// Blobs are SHA-256 content-addressed (Git-plumbing style) so repeated
// contexts deduplicate to a single object on disk.
package recorder

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"time"
)

// Recorder writes trace events under a base directory.
type Recorder struct {
	base string
}

// DefaultDir returns the canonical trace directory.
func DefaultDir() string {
	if p := os.Getenv("TOKENOS_TRACES"); p != "" {
		return p
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return ".tokenos-traces"
	}
	return filepath.Join(home, ".local", "state", "tokenos", "traces")
}

// New creates a Recorder rooted at dir (empty = DefaultDir).
func New(dir string) (*Recorder, error) {
	if dir == "" {
		dir = DefaultDir()
	}
	if err := os.MkdirAll(filepath.Join(dir, "objects"), 0o755); err != nil {
		return nil, err
	}
	return &Recorder{base: dir}, nil
}

// Event is one flight-recorder entry.
type Event struct {
	TaskID    string    `json:"task_id"`
	Kind      string    `json:"kind"` // decision | prompt | response | error | verify
	Summary   string    `json:"summary,omitempty"`
	BlobSHA   string    `json:"blob_sha,omitempty"`
	Timestamp time.Time `json:"ts"`
}

// putBlob stores content-addressed payload bytes, returning the SHA-256 hex.
// Identical payloads write once (deduplication).
func (r *Recorder) putBlob(data []byte) (string, error) {
	sum := sha256.Sum256(data)
	sha := hex.EncodeToString(sum[:])
	dir := filepath.Join(r.base, "objects", sha[:2])
	path := filepath.Join(dir, sha[2:])
	if _, err := os.Stat(path); err == nil {
		return sha, nil // already stored
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return "", err
	}
	return sha, os.WriteFile(path, data, 0o600)
}

// Record writes a full payload blob plus an index line into the per-task
// journal (NDJSON, append-only).
func (r *Recorder) Record(taskID, kind, summary string, payload []byte) (string, error) {
	sha := ""
	if len(payload) > 0 {
		var err error
		sha, err = r.putBlob(payload)
		if err != nil {
			return "", err
		}
	}
	ev := Event{TaskID: taskID, Kind: kind, Summary: summary, BlobSHA: sha, Timestamp: time.Now().UTC()}
	line, err := json.Marshal(ev)
	if err != nil {
		return "", err
	}
	journal := filepath.Join(r.base, fmt.Sprintf("%s.ndjson", sanitize(taskID)))
	f, err := os.OpenFile(journal, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o600)
	if err != nil {
		return "", err
	}
	defer f.Close()
	if _, err := f.Write(append(line, '\n')); err != nil {
		return "", err
	}
	return sha, nil
}

// Events replays the journal for a task.
func (r *Recorder) Events(taskID string) ([]Event, error) {
	journal := filepath.Join(r.base, fmt.Sprintf("%s.ndjson", sanitize(taskID)))
	data, err := os.ReadFile(journal)
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var out []Event
	start := 0
	for i := 0; i <= len(data); i++ {
		if i == len(data) || data[i] == '\n' {
			if i > start {
				var ev Event
				if json.Unmarshal(data[start:i], &ev) == nil {
					out = append(out, ev)
				}
			}
			start = i + 1
		}
	}
	return out, nil
}

// Blob fetches a stored payload by SHA.
func (r *Recorder) Blob(sha string) ([]byte, error) {
	if len(sha) < 3 {
		return nil, fmt.Errorf("invalid sha")
	}
	return os.ReadFile(filepath.Join(r.base, "objects", sha[:2], sha[2:]))
}

func sanitize(s string) string {
	out := make([]rune, 0, len(s))
	for _, r := range s {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9', r == '-', r == '_':
			out = append(out, r)
		default:
			out = append(out, '_')
		}
	}
	return string(out)
}
