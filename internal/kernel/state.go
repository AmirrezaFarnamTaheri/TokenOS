package kernel

import (
	"encoding/json"
	"time"
)

// State is the compressed, structured task state that replaces conversational
// history. State is preferred over summaries; summaries over transcripts;
// transcripts are a last resort and never stored here.
type State struct {
	TaskID      string         `json:"task_id"`
	Goal        string         `json:"goal"`
	Constraints []string       `json:"constraints,omitempty"`
	Status      Status         `json:"status"`
	Blocked     bool           `json:"blocked"`
	NextAction  string         `json:"next_action,omitempty"`
	Context     string         `json:"context,omitempty"` // minimum viable context only
	Failures    []FailureEntry `json:"failure_memory,omitempty"`
	CreatedAt   time.Time      `json:"created_at"`
	UpdatedAt   time.Time      `json:"updated_at"`
}

// Status is the lifecycle state of a task.
type Status string

const (
	StatusPending    Status = "pending"
	StatusRouted     Status = "routed"
	StatusInProgress Status = "in_progress"
	StatusVerifying  Status = "verifying"
	StatusDone       Status = "done"
	StatusBlocked    Status = "blocked"
	StatusEscalated  Status = "escalated"
	StatusFailed     Status = "failed"
)

// FailureEntry is one line of failure memory: a failed action and its reason.
// Maximum 5 entries are retained per task; oldest entries are evicted first.
type FailureEntry struct {
	Action string    `json:"action"`
	Reason string    `json:"reason"`
	At     time.Time `json:"at"`
}

// MaxFailureMemory is the hard cap on retained failure entries per task.
const MaxFailureMemory = 5

// RememberFailure appends a failure entry, evicting the oldest beyond the cap.
func (s *State) RememberFailure(action, reason string) {
	s.Failures = append(s.Failures, FailureEntry{Action: action, Reason: reason, At: time.Now().UTC()})
	if len(s.Failures) > MaxFailureMemory {
		s.Failures = s.Failures[len(s.Failures)-MaxFailureMemory:]
	}
}

// Compact returns the canonical compressed JSON form used for state transfer.
// Only goal, constraints, status, blocked flag, next action and failure
// memory survive; reasoning and history are deliberately absent.
func (s *State) Compact() ([]byte, error) {
	return json.Marshal(s)
}

// DelegationPacket is the minimal contract transmitted when work is delegated.
// No history, no reasoning — conclusions only.
type DelegationPacket struct {
	Task        string   `json:"task"`
	Scope       string   `json:"scope"`
	Constraints []string `json:"constraints,omitempty"`
	Acceptance  string   `json:"acceptance"`
	NextStep    string   `json:"next_step"`
}
