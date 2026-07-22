package bound

import (
	"fmt"
	"strings"
	"unicode"

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
// handling.
//
// TODO: no `any_one` (`_`) wildcard yet — its arrival forces the "one
// character = byte / code point / grapheme" decision. The simple-fold path
// allocates []rune(s) for each row; match directly over UTF-8 boundaries when
// profiling justifies the extra complexity. A future cross-cutting collation
// design may serve matching, equality, ordering, grouping, and indexes; this
// matcher's unicode_simple_fold comparison keeps its precise meaning.
type TextPattern struct {
	comparison   lir.TextComparison
	leading      bool
	segments     []string
	runeSegments [][]rune
	trailing     bool
}

// CompileTextPattern folds a parts list into a TextPattern, coalescing
// adjacent literals and collapsing adjacent gaps into the canonical shape the
// matcher expects. A literal part must be non-empty and the list non-empty.
func CompileTextPattern(parts []lir.TextMatchPart, comparison lir.TextComparison) (TextPattern, error) {
	if len(parts) == 0 {
		return TextPattern{}, fmt.Errorf("text_match needs at least one pattern part")
	}
	if comparison == "" {
		comparison = lir.TextComparisonExact
	}
	if comparison != lir.TextComparisonExact && comparison != lir.TextComparisonUnicodeSimpleFold {
		return TextPattern{}, fmt.Errorf("text_match has unknown comparison %q", comparison)
	}
	p := TextPattern{comparison: comparison}
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
	if comparison == lir.TextComparisonUnicodeSimpleFold {
		p.runeSegments = make([][]rune, len(p.segments))
		for i, segment := range p.segments {
			p.runeSegments[i] = []rune(segment)
		}
	}
	return p, nil
}

// Match reports whether the whole of s matches the anchored pattern.
func (p TextPattern) Match(s string) bool {
	if p.comparison == lir.TextComparisonUnicodeSimpleFold {
		return p.matchSimpleFold([]rune(s))
	}
	return p.matchExact(s)
}

// Comparison reports the literal equality relation compiled into the pattern.
func (p TextPattern) Comparison() lir.TextComparison { return p.comparison }

func (p TextPattern) matchExact(s string) bool {
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

func (p TextPattern) matchSimpleFold(s []rune) bool {
	segs := p.runeSegments
	if len(segs) == 0 {
		return true
	}
	if !p.leading {
		if !hasSimpleFoldPrefix(s, segs[0]) {
			return false
		}
		s = s[len(segs[0]):]
		segs = segs[1:]
		if len(segs) == 0 {
			return p.trailing || len(s) == 0
		}
	}
	if !p.trailing {
		last := segs[len(segs)-1]
		if len(last) > len(s) || !equalSimpleFoldRunes(s[len(s)-len(last):], last) {
			return false
		}
		s = s[:len(s)-len(last)]
		segs = segs[:len(segs)-1]
	}
	for _, seg := range segs {
		i := indexSimpleFold(s, seg)
		if i < 0 {
			return false
		}
		s = s[i+len(seg):]
	}
	return true
}

func hasSimpleFoldPrefix(s, prefix []rune) bool {
	return len(prefix) <= len(s) && equalSimpleFoldRunes(s[:len(prefix)], prefix)
}

func indexSimpleFold(s, needle []rune) int {
	for i := 0; i+len(needle) <= len(s); i++ {
		if equalSimpleFoldRunes(s[i:i+len(needle)], needle) {
			return i
		}
	}
	return -1
}

func equalSimpleFoldRunes(a, b []rune) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if !equalSimpleFoldRune(a[i], b[i]) {
			return false
		}
	}
	return true
}

func equalSimpleFoldRune(a, b rune) bool {
	if a == b {
		return true
	}
	for r := unicode.SimpleFold(a); r != a; r = unicode.SimpleFold(r) {
		if r == b {
			return true
		}
	}
	return false
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
