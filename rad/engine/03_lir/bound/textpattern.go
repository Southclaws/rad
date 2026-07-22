package bound

import (
	"fmt"
	"strings"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// TextPattern is a compiled text_match pattern: an anchored sequence of
// literal segments with optional gaps. A `%` (any-many) wildcard becomes a
// gap; `leading`/`trailing` record whether a gap sits before the first or
// after the last segment, and a gap always separates adjacent segments.
// Compiling the bind-time-constant parts once lets Match run per row without
// re-walking parts.
//
// Match is the standard `%`-glob: anchor the first segment to the front
// (unless there is a leading gap) and the last to the back (unless a trailing
// gap), then find the interior segments left-to-right. Interior segments are
// never anchored, so leftmost search is complete; only the ends need special
// handling. Only `%` exists, so this works on bytes.
//
// TODO: no `any_one` (`_`) wildcard yet — its arrival forces the "one
// character = byte / code point / grapheme" decision. No allocation-free
// fast path. Literal comparison is byte-exact until a text-equivalence /
// collation concept serves matching, equality, ordering, and grouping
// together.
type TextPattern struct {
	leading  bool
	segments []string
	trailing bool
}

// CompileTextPattern folds a parts list into a TextPattern, coalescing
// adjacent literals and collapsing adjacent gaps into the canonical shape the
// matcher expects. A literal part must be non-empty and the list non-empty.
func CompileTextPattern(parts []lir.TextMatchPart) (TextPattern, error) {
	if len(parts) == 0 {
		return TextPattern{}, fmt.Errorf("text_match needs at least one pattern part")
	}
	var p TextPattern
	var seg strings.Builder
	pending := false
	for i, part := range parts {
		switch pt := part.(type) {
		case lir.LiteralPart:
			if pt.Value == "" {
				return TextPattern{}, fmt.Errorf("text_match literal part must not be empty")
			}
			seg.WriteString(pt.Value)
			pending = true
			p.trailing = false
		case lir.AnyManyPart:
			if pending {
				p.segments = append(p.segments, seg.String())
				seg.Reset()
				pending = false
			}
			if i == 0 {
				p.leading = true
			}
			p.trailing = true
		default:
			return TextPattern{}, fmt.Errorf("text_match has an unknown pattern part %T", part)
		}
	}
	if pending {
		p.segments = append(p.segments, seg.String())
	}
	return p, nil
}

// Match reports whether the whole of s matches the anchored pattern.
func (p TextPattern) Match(s string) bool {
	segs := p.segments
	if len(segs) == 0 {
		return true // only wildcards
	}
	if !p.leading {
		if !strings.HasPrefix(s, segs[0]) {
			return false
		}
		s = s[len(segs[0]):]
		segs = segs[1:]
		if len(segs) == 0 {
			return p.trailing || s == ""
		}
	}
	if !p.trailing {
		last := segs[len(segs)-1]
		if !strings.HasSuffix(s, last) {
			return false
		}
		s = s[:len(s)-len(last)]
		segs = segs[:len(segs)-1]
	}
	for _, seg := range segs {
		i := strings.Index(s, seg)
		if i < 0 {
			return false
		}
		s = s[i+len(seg):]
	}
	return true
}

// String renders the pattern the way it prints in a plan.
func (p TextPattern) String() string {
	var b strings.Builder
	if p.leading {
		b.WriteByte('%')
	}
	for i, seg := range p.segments {
		if i > 0 {
			b.WriteByte('%')
		}
		fmt.Fprintf(&b, "%q", seg)
	}
	if p.trailing {
		b.WriteByte('%')
	}
	return b.String()
}
