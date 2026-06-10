// Package kernel implements the Token-Optimal Agent Execution Kernel:
// a deterministic, zero-token routing engine that decides HOW a task is
// executed before any network byte is spent.
//
// Primary objective:
//
//	maximize  Value Delivered / Total System Cost
//
// Meta rule: never spend more resources deciding than the decision can save.
package kernel

// Route is an execution route in strict priority order. The earliest
// applicable route always wins.
type Route string

const (
	RouteDirect    Route = "DIRECT"    // 0: trivial, execute immediately
	RouteReuse     Route = "REUSE"     // 1: existing solution satisfies most requirements
	RoutePatch     Route = "PATCH"     // 2: localized change, architecture intact
	RouteImplement Route = "IMPLEMENT" // 3: requirements clear, full build is cheapest
	RoutePartial   Route = "PARTIAL"   // 4: genuine external blocker, deliver what's done
	RouteDelegate  Route = "DELEGATE"  // 5: repetitive + bounded, delegation pays for itself
	RouteAsk       Route = "ASK"       // 6: blocked by missing information (one question)
	RouteVerify    Route = "VERIFY"    // internal: verification-only execution (sandbox)

	RouteEscalateConflict Route = "ESCALATE-CONFLICT" // requirements contradict
	RouteEscalateSafety   Route = "ESCALATE-SAFETY"   // violates constraints or policy
	RouteEscalateExternal Route = "ESCALATE-EXTERNAL" // external dependency blocks progress
)

// Priority returns the kernel priority index of a route (lower = earlier).
func (r Route) Priority() int {
	switch r {
	case RouteDirect:
		return 0
	case RouteReuse:
		return 1
	case RoutePatch:
		return 2
	case RouteImplement:
		return 3
	case RoutePartial:
		return 4
	case RouteDelegate:
		return 5
	case RouteAsk:
		return 6
	case RouteEscalateConflict, RouteEscalateSafety, RouteEscalateExternal:
		return 7
	default:
		return 99
	}
}

// IsEscalation reports whether the route terminates execution upward.
func (r Route) IsEscalation() bool {
	switch r {
	case RouteEscalateConflict, RouteEscalateSafety, RouteEscalateExternal:
		return true
	}
	return false
}

// IsTerminalLocal reports whether the route resolves with zero network cost
// (ASK and all ESCALATE-* routes never call a provider).
func (r Route) IsTerminalLocal() bool {
	return r == RouteAsk || r.IsEscalation()
}

// AllRoutes lists every user-facing route in kernel priority order.
func AllRoutes() []Route {
	return []Route{
		RouteDirect, RouteReuse, RoutePatch, RouteImplement,
		RoutePartial, RouteDelegate, RouteAsk,
		RouteEscalateConflict, RouteEscalateSafety, RouteEscalateExternal,
	}
}
