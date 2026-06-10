package kernel

import (
	"regexp"
	"strings"
)

// Signals are the deterministic inputs the router uses to pick a route.
// They are computed locally (zero token cost) from the task description,
// the workspace index and prior telemetry.
type Signals struct {
	// EstimatedTokens is the local tokenizer estimate of the full task
	// payload (description + minimum viable context).
	EstimatedTokens int `json:"estimated_tokens"`
	// Confidence is a 0..1 deterministic heuristic estimate that the task
	// can be completed without clarification.
	Confidence float64 `json:"confidence"`
	// Trivial means execution cost is obviously lower than routing cost.
	Trivial bool `json:"trivial"`
	// HasExistingSolution means the local index found a near-match artifact.
	HasExistingSolution bool `json:"has_existing_solution"`
	// LocalizedChange means the affected surface area is small and the
	// existing architecture remains intact.
	LocalizedChange bool `json:"localized_change"`
	// Repetitive+Bounded together make the task a delegation candidate.
	Repetitive bool `json:"repetitive"`
	Bounded    bool `json:"bounded"`
	// ExternalBlocker indicates a genuine dependency outside our control.
	ExternalBlocker bool `json:"external_blocker"`
	// ConflictingRequirements / SafetyViolation force escalation.
	ConflictingRequirements bool `json:"conflicting_requirements"`
	SafetyViolation         bool `json:"safety_violation"`
	// MissingCriticalInfo means execution is blocked until one question
	// is answered.
	MissingCriticalInfo bool `json:"missing_critical_info"`
	// RepeatedFailure indicates failure memory matched this action before.
	RepeatedFailure bool `json:"repeated_failure"`
	// LoopDetected indicates the semantic loop detector fired.
	LoopDetected bool `json:"loop_detected"`
}

// RouterPolicy holds tunable, empirically adjusted thresholds. These live in
// deterministic code — never in a worker prompt — so routing costs zero tokens.
type RouterPolicy struct {
	AskThreshold       float64 `json:"ask_threshold" yaml:"ask_threshold"`               // below this confidence => ASK
	DirectMaxTokens    int     `json:"direct_max_tokens" yaml:"direct_max_tokens"`       // above this size DIRECT is off the table
	DelegationPenalty  float64 `json:"delegation_penalty" yaml:"delegation_penalty"`     // fixed cost assumed for any delegation
	DelegationMinScale float64 `json:"delegation_min_scale" yaml:"delegation_min_scale"` // expected savings multiplier required
}

// DefaultPolicy returns the kernel default thresholds.
func DefaultPolicy() RouterPolicy {
	return RouterPolicy{
		AskThreshold:       0.35,
		DirectMaxTokens:    600,
		DelegationPenalty:  1500, // tokens-equivalent fixed cost
		DelegationMinScale: 1.5,  // savings must exceed 1.5x the penalty
	}
}

// Decision is the router output: the chosen route plus a one-line reason.
// The reason is for the human flight recorder, never transmitted upstream.
type Decision struct {
	Route   Route   `json:"route"`
	Reason  string  `json:"reason"`
	Signals Signals `json:"signals"`
}

// Decide applies the kernel's strict priority ladder. Earliest applicable
// route wins. Escalations and information blocks preempt everything because
// they cost the least and prevent guaranteed waste.
func Decide(s Signals, p RouterPolicy) Decision {
	switch {
	// Hard preemptions: progress is impossible.
	case s.SafetyViolation:
		return Decision{RouteEscalateSafety, "execution would violate constraints or policy", s}
	case s.ConflictingRequirements:
		return Decision{RouteEscalateConflict, "requirements contradict each other", s}
	case s.LoopDetected:
		return Decision{RouteEscalateExternal, "semantic execution loop detected (edit-distance ceiling)", s}
	case s.ExternalBlocker && !s.Bounded:
		return Decision{RouteEscalateExternal, "external dependency prevents any execution", s}

	// Information block: ask exactly one question.
	case s.MissingCriticalInfo || s.Confidence < p.AskThreshold:
		return Decision{RouteAsk, "execution blocked by missing information; one question required", s}

	// 0: DIRECT — execution cheaper than routing.
	case s.Trivial && s.EstimatedTokens <= p.DirectMaxTokens:
		return Decision{RouteDirect, "trivial task; execution cost below routing cost", s}

	// 1: REUSE — one search pass already found a near-match.
	case s.HasExistingSolution:
		return Decision{RouteReuse, "existing solution satisfies most requirements (reuse > extend > build)", s}

	// 2: PATCH — localized change, architecture intact.
	case s.LocalizedChange && !s.RepeatedFailure:
		return Decision{RoutePatch, "localized change with intact architecture (patch > rewrite)", s}

	// 4: PARTIAL — external blocker but most value deliverable.
	case s.ExternalBlocker:
		return Decision{RoutePartial, "external blocker exists; delivering all completed work", s}

	// 5: DELEGATE — repetitive, bounded, and savings exceed the penalty.
	case s.Repetitive && s.Bounded &&
		float64(s.EstimatedTokens) > p.DelegationPenalty*p.DelegationMinScale:
		return Decision{RouteDelegate, "repetitive bounded work; expected savings exceed delegation penalty", s}

	// 3: IMPLEMENT — default productive path.
	default:
		return Decision{RouteImplement, "requirements sufficiently clear; direct completion is cheapest", s}
	}
}

// ---------------------------------------------------------------------------
// Local heuristics: deterministic signal extraction from a task description.
// These never call a model. They are intentionally simple, fast and auditable.
// ---------------------------------------------------------------------------

var (
	reQuestionable = regexp.MustCompile(`(?i)\b(maybe|somehow|something|not sure|tbd|\?\?\?)\b`)
	reLocalized    = regexp.MustCompile(`(?i)\b(fix|patch|typo|rename|bump|adjust|tweak|update (a|the|one)|small change|one[- ]line)\b`)
	reTrivial      = regexp.MustCompile(`(?i)\b(typo|rename|bump version|add comment|format|reformat|lint fix|sort imports)\b`)
	reRepetitive   = regexp.MustCompile(`(?i)\b(for (each|every|all)|batch|bulk|repeat|across \d+|all \d+ files|every file)\b`)
	reExternal     = regexp.MustCompile(`(?i)\b(waiting (on|for)|blocked by|api key missing|credentials missing|upstream outage|access denied|permission denied)\b`)
	reConflict     = regexp.MustCompile(`(?i)\b(but also must not|contradict|mutually exclusive|both .* and never)\b`)
	reSafety       = regexp.MustCompile(`(?i)\b(bypass auth|disable security|exfiltrate|leak credentials|ignore policy)\b`)
	reAskNeeded    = regexp.MustCompile(`(?i)\b(which one\?|unspecified|missing spec|need to know|clarify)\b`)
)

// ExtractSignals derives Signals from a task description plus environment
// facts supplied by the engine (token estimate, index hit, failure memory).
func ExtractSignals(task string, estimatedTokens int, indexHit, repeatedFailure, loopDetected bool) Signals {
	t := strings.TrimSpace(task)
	words := len(strings.Fields(t))

	s := Signals{
		EstimatedTokens:         estimatedTokens,
		Trivial:                 reTrivial.MatchString(t) && words <= 40,
		HasExistingSolution:     indexHit,
		LocalizedChange:         reLocalized.MatchString(t),
		Repetitive:              reRepetitive.MatchString(t),
		Bounded:                 words <= 200,
		ExternalBlocker:         reExternal.MatchString(t),
		ConflictingRequirements: reConflict.MatchString(t),
		SafetyViolation:         reSafety.MatchString(t),
		MissingCriticalInfo:     reAskNeeded.MatchString(t),
		RepeatedFailure:         repeatedFailure,
		LoopDetected:            loopDetected,
	}

	// Deterministic confidence: start high, subtract for vagueness markers,
	// extreme brevity, and prior failures. Bounded to [0,1].
	conf := 0.9
	// Each distinct vagueness marker compounds: one marker may still be
	// actionable, two or more signal a task that needs clarification (ASK).
	if n := len(reQuestionable.FindAllString(t, 3)); n > 0 {
		conf -= 0.35 + 0.25*float64(n-1)
	}
	if words < 3 {
		conf -= 0.4
	}
	if repeatedFailure {
		conf -= 0.2
	}
	if s.MissingCriticalInfo {
		conf -= 0.5
	}
	if conf < 0 {
		conf = 0
	}
	if conf > 1 {
		conf = 1
	}
	s.Confidence = conf
	return s
}
