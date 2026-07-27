package bound

import (
	"strings"
	"testing"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func litPart(s string) lir.TextMatchPart { return lir.LiteralPart{Value: s} }
func anyPart() lir.TextMatchPart         { return lir.AnyManyPart{} }

func mustCompile(t *testing.T, parts ...lir.TextMatchPart) TextPattern {
	t.Helper()
	return mustCompileCompared(t, lir.TextComparisonExact, parts...)
}

func mustCompileCompared(t *testing.T, comparison lir.TextComparison, parts ...lir.TextMatchPart) TextPattern {
	t.Helper()
	p, err := CompileTextPattern(parts, comparison)
	if err != nil {
		t.Fatalf("compile %v: %v", parts, err)
	}
	return p
}

// The anchored shapes and their edges, hand-derived.
func TestTextPatternShapes(t *testing.T) {
	cases := []struct {
		name  string
		parts []lir.TextMatchPart
		match []string
		miss  []string
	}{
		{
			"exact",
			[]lir.TextMatchPart{litPart("foo")},
			[]string{"foo"},
			[]string{"", "fo", "foo ", " foo", "foobar", "Foo"},
		},
		{
			"prefix",
			[]lir.TextMatchPart{litPart("foo"), anyPart()},
			[]string{"foo", "foobar", "foo!"},
			[]string{"fo", "xfoo", ""},
		},
		{
			"suffix",
			[]lir.TextMatchPart{anyPart(), litPart("bar")},
			[]string{"bar", "foobar", "!bar"},
			[]string{"ba", "barx", ""},
		},
		{
			"infix",
			[]lir.TextMatchPart{anyPart(), litPart("mid"), anyPart()},
			[]string{"mid", "xmidy", "midy", "xmid"},
			[]string{"mi", "md", ""},
		},
		{
			"multigap a%b%c",
			[]lir.TextMatchPart{litPart("a"), anyPart(), litPart("b"), anyPart(), litPart("c")},
			[]string{"abc", "axbyc", "aXbXc", "abcbc"},
			[]string{"ab", "acb", "cba", "bac"},
		},
		{
			"all wildcard",
			[]lir.TextMatchPart{anyPart()},
			[]string{"", "anything", "a b c", "日本語"},
			nil,
		},
		{
			"utf8 suffix",
			[]lir.TextMatchPart{anyPart(), litPart("é")},
			[]string{"café", "é", "xé"},
			[]string{"cafe", "éx", ""},
		},
		{
			"coalesced adjacent literals",
			[]lir.TextMatchPart{litPart("ab"), litPart("cd")},
			[]string{"abcd"},
			[]string{"ab", "abccd", "abd"},
		},
		{
			"collapsed adjacent gaps",
			[]lir.TextMatchPart{anyPart(), anyPart(), litPart("x")},
			[]string{"x", "yyx", "x"},
			[]string{"xy", ""},
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			p := mustCompile(t, tc.parts...)
			for _, s := range tc.match {
				if !p.Match(s) {
					t.Errorf("%s: %q should match %s", tc.name, s, p)
				}
			}
			for _, s := range tc.miss {
				if p.Match(s) {
					t.Errorf("%s: %q should not match %s", tc.name, s, p)
				}
			}
		})
	}
}

func TestTextPatternCompileErrors(t *testing.T) {
	if _, err := CompileTextPattern(nil, lir.TextComparisonExact); err == nil {
		t.Error("empty parts should error")
	}
	if _, err := CompileTextPattern([]lir.TextMatchPart{litPart("")}, lir.TextComparisonExact); err == nil {
		t.Error("empty literal part should error")
	}
	if _, err := CompileTextPattern([]lir.TextMatchPart{litPart("x")}, "unknown"); err == nil {
		t.Error("unknown comparison should error")
	}
}

func TestTextPatternUnicodeSimpleFold(t *testing.T) {
	cases := []struct {
		name  string
		parts []lir.TextMatchPart
		match []string
		miss  []string
	}{
		{
			"exact shape",
			[]lir.TextMatchPart{litPart("foo")},
			[]string{"foo", "FOO", "fOo"},
			[]string{"fo", "xfoo", "fooo"},
		},
		{
			"prefix and suffix",
			[]lir.TextMatchPart{litPart("fOo"), anyPart(), litPart("BAR")},
			[]string{"FOObar", "foo---bar", "fOoBAR"},
			[]string{"xfoobar", "foobarx", "foo"},
		},
		{
			"unicode fold cycle",
			[]lir.TextMatchPart{anyPart(), litPart("k"), anyPart()},
			[]string{"K", "xKy"},
			nil,
		},
		{
			"no full fold expansion",
			[]lir.TextMatchPart{litPart("STRASSE")},
			nil,
			[]string{"Straße"},
		},
		{
			"no accent folding",
			[]lir.TextMatchPart{litPart("e")},
			nil,
			[]string{"é"},
		},
		{
			"no normalization",
			[]lir.TextMatchPart{litPart("é")},
			nil,
			[]string{"e\u0301"},
		},
		{
			"locale independent dotted I",
			[]lir.TextMatchPart{litPart("i")},
			nil,
			[]string{"İ"},
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			p := mustCompileCompared(t, lir.TextComparisonUnicodeSimpleFold, tc.parts...)
			for _, s := range tc.match {
				if !p.Match(s) {
					t.Errorf("%q should match %s", s, p)
				}
			}
			for _, s := range tc.miss {
				if p.Match(s) {
					t.Errorf("%q should not match %s", s, p)
				}
			}
		})
	}
}

// refMatch is an independent, obviously-correct anchored %-glob matcher over
// the raw parts — deliberately slow backtracking. It exists only to
// cross-check the compiled TextPattern, so the matcher is validated against a
// second implementation that shares no code with it.
func refMatch(parts []lir.TextMatchPart, s string) bool {
	if len(parts) == 0 {
		return s == ""
	}
	switch p := parts[0].(type) {
	case lir.LiteralPart:
		if !strings.HasPrefix(s, p.Value) {
			return false
		}
		return refMatch(parts[1:], s[len(p.Value):])
	case lir.AnyManyPart:
		for i := 0; i <= len(s); i++ {
			if refMatch(parts[1:], s[i:]) {
				return true
			}
		}
		return false
	}
	return false
}

// refMatchSimpleFold is the rune-boundary backtracking oracle for the folded
// matcher. It delegates each literal comparison to strings.EqualFold, directly
// pinning the implementation to the standard library's equality relation.
func refMatchSimpleFold(parts []lir.TextMatchPart, s []rune) bool {
	if len(parts) == 0 {
		return len(s) == 0
	}
	switch p := parts[0].(type) {
	case lir.LiteralPart:
		literal := []rune(p.Value)
		if len(literal) > len(s) || !strings.EqualFold(string(s[:len(literal)]), p.Value) {
			return false
		}
		return refMatchSimpleFold(parts[1:], s[len(literal):])
	case lir.AnyManyPart:
		for i := 0; i <= len(s); i++ {
			if refMatchSimpleFold(parts[1:], s[i:]) {
				return true
			}
		}
	}
	return false
}

// Every pattern of up to four parts over {a, b, %} against every input of up
// to four characters over {a, b, c}: the compiled matcher must agree with the
// backtracking oracle on all ~14.5k pairs.
func TestTextPatternExhaustiveVsOracle(t *testing.T) {
	elems := []lir.TextMatchPart{litPart("a"), litPart("b"), anyPart()}
	var patterns [][]lir.TextMatchPart
	var build func(prefix []lir.TextMatchPart, depth int)
	build = func(prefix []lir.TextMatchPart, depth int) {
		if len(prefix) > 0 {
			patterns = append(patterns, prefix)
		}
		if depth == 0 {
			return
		}
		for _, e := range elems {
			next := make([]lir.TextMatchPart, len(prefix)+1)
			copy(next, prefix)
			next[len(prefix)] = e
			build(next, depth-1)
		}
	}
	build(nil, 4)

	var inputs []string
	var buildIn func(prefix string, depth int)
	buildIn = func(prefix string, depth int) {
		inputs = append(inputs, prefix)
		if depth == 0 {
			return
		}
		for _, ch := range []string{"a", "b", "c"} {
			buildIn(prefix+ch, depth-1)
		}
	}
	buildIn("", 4)

	pairs := 0
	for _, parts := range patterns {
		cp := mustCompile(t, parts...)
		for _, s := range inputs {
			if got, want := cp.Match(s), refMatch(parts, s); got != want {
				t.Fatalf("pattern %s input %q: compiled=%v oracle=%v", cp, s, got, want)
			}
			pairs++
		}
	}
	t.Logf("cross-checked %d (pattern, input) pairs against the oracle", pairs)
}

func TestTextPatternSimpleFoldExhaustiveVsEqualFoldOracle(t *testing.T) {
	elems := []lir.TextMatchPart{litPart("k"), litPart("ſ"), anyPart()}
	var patterns [][]lir.TextMatchPart
	var buildPattern func([]lir.TextMatchPart, int)
	buildPattern = func(prefix []lir.TextMatchPart, depth int) {
		if len(prefix) > 0 {
			patterns = append(patterns, prefix)
		}
		if depth == 0 {
			return
		}
		for _, elem := range elems {
			next := append(append([]lir.TextMatchPart(nil), prefix...), elem)
			buildPattern(next, depth-1)
		}
	}
	buildPattern(nil, 3)

	var inputs []string
	var buildInput func(string, int)
	buildInput = func(prefix string, depth int) {
		inputs = append(inputs, prefix)
		if depth == 0 {
			return
		}
		for _, char := range []string{"K", "K", "S", "x"} {
			buildInput(prefix+char, depth-1)
		}
	}
	buildInput("", 3)

	for _, parts := range patterns {
		pattern := mustCompileCompared(t, lir.TextComparisonUnicodeSimpleFold, parts...)
		for _, input := range inputs {
			if got, want := pattern.Match(input), refMatchSimpleFold(parts, []rune(input)); got != want {
				t.Fatalf("pattern %s input %q: compiled=%v EqualFold oracle=%v", pattern, input, got, want)
			}
		}
	}
}
