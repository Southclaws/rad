// Package bound is the resolved form of the relation-graph IR: every name
// bound against the catalog, every literal typed, every relation output
// addressed by dense slots, every node carrying its laws (output row type,
// free slots, cardinality) precomputed. The binder (04_planner) is the only
// producer; the planner and executor trust bound IR completely.
package bound

import (
	"slices"

	lir "rad/rad/03_lir"
)

// SlotSet is a set of slot IDs, backed by a bitset over the binder's dense
// slot space. The zero value is the empty set. Operations return new sets;
// a SlotSet is never mutated after it escapes a constructor.
type SlotSet struct {
	words []uint64
}

// NewSlotSet builds a set from ids.
func NewSlotSet(ids ...lir.SlotID) SlotSet {
	var s SlotSet
	for _, id := range ids {
		s.add(id)
	}
	return s
}

func (s *SlotSet) add(id lir.SlotID) {
	w := int(id) / 64
	for len(s.words) <= w {
		s.words = append(s.words, 0)
	}
	s.words[w] |= 1 << (uint(id) % 64)
}

// Contains reports membership.
func (s SlotSet) Contains(id lir.SlotID) bool {
	w := int(id) / 64
	return w < len(s.words) && s.words[w]&(1<<(uint(id)%64)) != 0
}

// Union returns s ∪ o.
func (s SlotSet) Union(o SlotSet) SlotSet {
	out := SlotSet{words: make([]uint64, max(len(s.words), len(o.words)))}
	copy(out.words, s.words)
	for i, w := range o.words {
		out.words[i] |= w
	}
	return out.compact()
}

// Without returns s \ o.
func (s SlotSet) Without(o SlotSet) SlotSet {
	out := SlotSet{words: slices.Clone(s.words)}
	for i := range out.words {
		if i < len(o.words) {
			out.words[i] &^= o.words[i]
		}
	}
	return out.compact()
}

// Empty reports whether the set has no members.
func (s SlotSet) Empty() bool {
	for _, w := range s.words {
		if w != 0 {
			return false
		}
	}
	return true
}

// Slots lists members in ascending order.
func (s SlotSet) Slots() []lir.SlotID {
	var out []lir.SlotID
	for wi, w := range s.words {
		for b := range 64 {
			if w&(1<<uint(b)) != 0 {
				out = append(out, lir.SlotID(wi*64+b))
			}
		}
	}
	return out
}

func (s SlotSet) compact() SlotSet {
	n := len(s.words)
	for n > 0 && s.words[n-1] == 0 {
		n--
	}
	s.words = s.words[:n]
	return s
}
