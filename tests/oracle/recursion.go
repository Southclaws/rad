// Package oracle holds small, from-scratch semantic models used as testing
// oracles alongside refexec (the broad, whole-query reference interpreter).
//
// Each model captures exactly one feature's semantics and knows nothing about
// LIR, slots, binding, physical plans, or the engine — it is deliberately a
// different implementation on different mechanics. That independence is the
// point: "engine == refexec" can be doubled with "engine == model" at the
// semantic boundaries where an accidental shared mistake between the engine and
// refexec would otherwise pass unseen. refexec shares the bound-LIR value model
// and (justified separately) scalar evaluation; the models here share nothing.
//
// One concern per file, named for its use-case. Today: recursive accumulation.
// Future set-operation, aggregate, and join-multiplicity models belong beside
// this one, each a self-contained model of just its own semantics.
package oracle

// Recursive accumulation, modelled as an abstract fixpoint over states with
// value identity. A "state" is whatever the recursion carries: a bare id for
// reachability, or a struct carrying recursive state (a depth, a nullable tag
// encoded so NULL equals NULL). The caller supplies the anchors and the step
// (transition) as a plain function; this models only how the two accumulation
// modes admit what the step produces.

// FixpointNew models `accumulation: new`: the set of states reachable from the
// anchors under step, each admitted at most once by value identity, in
// breadth-first discovery order. It terminates on any finite state space,
// including cyclic transitions, because a revisited state is never re-expanded.
func FixpointNew[S comparable](anchors []S, step func(S) []S) []S {
	seen := make(map[S]bool)
	var out, queue []S
	admit := func(s S) {
		if !seen[s] {
			seen[s] = true
			out = append(out, s)
			queue = append(queue, s)
		}
	}
	for _, a := range anchors {
		admit(a)
	}
	for len(queue) > 0 {
		cur := queue[0]
		queue = queue[1:]
		for _, nx := range step(cur) {
			admit(nx)
		}
	}
	return out
}

// FixpointAll models `accumulation: all`: every state reached along every
// distinct path from an anchor, with multiplicity, by enumerating paths
// depth-first. Each anchor is a length-zero path admitted once. It terminates
// only when step is acyclic — every path is then finite — which the caller
// guarantees, exactly as admit-all over a cycle would not terminate in the
// engine either. Enumerating paths, rather than iterating a frontier, keeps it
// mechanically distinct from the engine's semi-naive loop.
func FixpointAll[S any](anchors []S, step func(S) []S) []S {
	var out []S
	var walk func(S)
	walk = func(s S) {
		out = append(out, s)
		for _, nx := range step(s) {
			walk(nx)
		}
	}
	for _, a := range anchors {
		walk(a)
	}
	return out
}
