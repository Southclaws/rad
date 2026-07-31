use std::fmt;

use casefold::simple_fold_char as unicode_16_simple_fold_char;

use super::super::{TextComparison, TextMatchPart};

/// Bind-time compiled, anchored `%`-glob.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextPattern {
    comparison: TextComparison,
    leading: bool,
    segments: Vec<String>,
    folded_segments: Vec<Vec<char>>,
    trailing: bool,
}

impl TextPattern {
    pub fn compile(
        parts: &[TextMatchPart],
        comparison: TextComparison,
    ) -> Result<Self, TextPatternError> {
        if parts.is_empty() {
            return Err(TextPatternError::Empty);
        }
        let mut pattern = Self {
            comparison,
            leading: false,
            segments: Vec::new(),
            folded_segments: Vec::new(),
            trailing: false,
        };
        let mut segment = String::new();
        for (index, part) in parts.iter().enumerate() {
            match part {
                TextMatchPart::Literal(value) => {
                    if value.is_empty() {
                        return Err(TextPatternError::EmptyLiteral);
                    }
                    segment.push_str(value);
                    pattern.trailing = false;
                }
                TextMatchPart::AnyMany => {
                    if !segment.is_empty() {
                        pattern.segments.push(std::mem::take(&mut segment));
                    }
                    if index == 0 {
                        pattern.leading = true;
                    }
                    pattern.trailing = true;
                }
            }
        }
        if !segment.is_empty() {
            pattern.segments.push(segment);
        }
        if comparison == TextComparison::UnicodeSimpleFold {
            pattern.folded_segments = pattern
                .segments
                .iter()
                .map(|segment| segment.chars().map(simple_fold_char).collect())
                .collect();
        }
        Ok(pattern)
    }

    pub fn is_match(&self, value: &str) -> bool {
        match self.comparison {
            TextComparison::Exact => match_segments(
                value.as_bytes(),
                &self.segments,
                self.leading,
                self.trailing,
            ),
            TextComparison::UnicodeSimpleFold => {
                let value: Vec<_> = value.chars().map(simple_fold_char).collect();
                match_segments(&value, &self.folded_segments, self.leading, self.trailing)
            }
        }
    }

    pub fn comparison(&self) -> TextComparison {
        self.comparison
    }
}

trait SegmentUnits<T> {
    fn units(&self) -> &[T];
}

impl SegmentUnits<u8> for String {
    fn units(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl SegmentUnits<char> for Vec<char> {
    fn units(&self) -> &[char] {
        self
    }
}

fn match_segments<T: PartialEq, S: SegmentUnits<T>>(
    mut value: &[T],
    mut segments: &[S],
    leading: bool,
    trailing: bool,
) -> bool {
    if segments.is_empty() {
        return true;
    }
    if !leading {
        let first = segments[0].units();
        if !value.starts_with(first) {
            return false;
        }
        value = &value[first.len()..];
        segments = &segments[1..];
        if segments.is_empty() {
            return trailing || value.is_empty();
        }
    }
    if !trailing {
        let last = segments.last().expect("non-empty segment set").units();
        if !value.ends_with(last) {
            return false;
        }
        value = &value[..value.len() - last.len()];
        segments = &segments[..segments.len() - 1];
    }
    for segment in segments {
        let segment = segment.units();
        let Some(index) = value
            .windows(segment.len())
            .position(|window| window == segment)
        else {
            return false;
        };
        value = &value[index + segment.len()..];
    }
    true
}

/// Canonical one-code-point fold pinned to the protocol's Unicode 15.0
/// contract. `casefold` ships Unicode 16.0 data; these source mappings were
/// added in 16.0 and remain distinct until the protocol deliberately advances.
fn simple_fold_char(value: char) -> char {
    if matches!(
        value,
        '\u{1c89}'
            | '\u{1fd3}'
            | '\u{1fe3}'
            | '\u{a7cb}'
            | '\u{a7cc}'
            | '\u{a7da}'
            | '\u{a7dc}'
            | '\u{fb05}'
            | '\u{10d50}'..='\u{10d65}'
    ) {
        value
    } else {
        unicode_16_simple_fold_char(value)
    }
}

impl fmt::Display for TextPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.leading {
            formatter.write_str("%")?;
        }
        for (index, segment) in self.segments.iter().enumerate() {
            if index > 0 {
                formatter.write_str("%")?;
            }
            write!(formatter, "{segment:?}")?;
        }
        if self.trailing {
            formatter.write_str("%")?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum TextPatternError {
    #[error("text_match needs at least one pattern part")]
    Empty,
    #[error("text_match literal part must not be empty")]
    EmptyLiteral,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(value: &str) -> TextMatchPart {
        TextMatchPart::Literal(value.into())
    }

    #[test]
    fn anchored_shapes_and_unicode_simple_fold_match_the_protocol() {
        let cases = [
            (vec![literal("foo")], vec!["foo"], vec!["Foo", "foobar"]),
            (
                vec![literal("foo"), TextMatchPart::AnyMany],
                vec!["foo", "foobar"],
                vec!["xfoo"],
            ),
            (
                vec![TextMatchPart::AnyMany, literal("bar")],
                vec!["bar", "foobar"],
                vec!["barx"],
            ),
            (
                vec![literal("a"), TextMatchPart::AnyMany, literal("b")],
                vec!["ab", "axxb"],
                vec!["ba"],
            ),
        ];
        for (parts, hits, misses) in cases {
            let pattern = TextPattern::compile(&parts, TextComparison::Exact).unwrap();
            for hit in hits {
                assert!(pattern.is_match(hit), "{hit:?} should match {pattern}");
            }
            for miss in misses {
                assert!(!pattern.is_match(miss), "{miss:?} should miss {pattern}");
            }
        }

        let pattern = TextPattern::compile(
            &[literal("fOo"), TextMatchPart::AnyMany, literal("BAR")],
            TextComparison::UnicodeSimpleFold,
        )
        .unwrap();
        assert!(pattern.is_match("FOO---bar"));
        assert!(!pattern.is_match("xfoobar"));
        let kelvin =
            TextPattern::compile(&[literal("k")], TextComparison::UnicodeSimpleFold).unwrap();
        assert!(kelvin.is_match("K"));
        let no_full_fold =
            TextPattern::compile(&[literal("STRASSE")], TextComparison::UnicodeSimpleFold).unwrap();
        assert!(!no_full_fold.is_match("Straße"));
    }

    #[test]
    fn compile_rejects_empty_shapes() {
        assert_eq!(
            TextPattern::compile(&[], TextComparison::Exact),
            Err(TextPatternError::Empty)
        );
        assert_eq!(
            TextPattern::compile(&[literal("")], TextComparison::Exact),
            Err(TextPatternError::EmptyLiteral)
        );
    }

    fn reference_match(parts: &[TextMatchPart], value: &str) -> bool {
        if parts.is_empty() {
            return value.is_empty();
        }
        match &parts[0] {
            TextMatchPart::Literal(literal) => value
                .strip_prefix(literal)
                .is_some_and(|rest| reference_match(&parts[1..], rest)),
            TextMatchPart::AnyMany => value
                .char_indices()
                .map(|(index, _)| index)
                .chain(std::iter::once(value.len()))
                .any(|index| reference_match(&parts[1..], &value[index..])),
        }
    }

    fn fold_equal(left: &str, right: &str) -> bool {
        left.chars()
            .map(simple_fold_char)
            .eq(right.chars().map(simple_fold_char))
    }

    fn reference_fold_match(parts: &[TextMatchPart], value: &[char]) -> bool {
        if parts.is_empty() {
            return value.is_empty();
        }
        match &parts[0] {
            TextMatchPart::Literal(literal) => {
                let length = literal.chars().count();
                length <= value.len()
                    && fold_equal(&value[..length].iter().collect::<String>(), literal)
                    && reference_fold_match(&parts[1..], &value[length..])
            }
            TextMatchPart::AnyMany => {
                (0..=value.len()).any(|index| reference_fold_match(&parts[1..], &value[index..]))
            }
        }
    }

    fn patterns(elements: &[TextMatchPart], depth: usize) -> Vec<Vec<TextMatchPart>> {
        fn build(
            result: &mut Vec<Vec<TextMatchPart>>,
            elements: &[TextMatchPart],
            prefix: Vec<TextMatchPart>,
            depth: usize,
        ) {
            if !prefix.is_empty() {
                result.push(prefix.clone());
            }
            if depth == 0 {
                return;
            }
            for element in elements {
                let mut next = prefix.clone();
                next.push(element.clone());
                build(result, elements, next, depth - 1);
            }
        }
        let mut result = Vec::new();
        build(&mut result, elements, Vec::new(), depth);
        result
    }

    fn inputs(alphabet: &[char], depth: usize) -> Vec<String> {
        fn build(result: &mut Vec<String>, alphabet: &[char], prefix: String, depth: usize) {
            result.push(prefix.clone());
            if depth == 0 {
                return;
            }
            for character in alphabet {
                let mut next = prefix.clone();
                next.push(*character);
                build(result, alphabet, next, depth - 1);
            }
        }
        let mut result = Vec::new();
        build(&mut result, alphabet, String::new(), depth);
        result
    }

    #[test]
    fn exact_matcher_agrees_with_independent_backtracking_oracle() {
        let patterns = patterns(&[literal("a"), literal("b"), TextMatchPart::AnyMany], 4);
        let inputs = inputs(&['a', 'b', 'c'], 4);
        for parts in patterns {
            let compiled = TextPattern::compile(&parts, TextComparison::Exact).unwrap();
            for input in &inputs {
                assert_eq!(
                    compiled.is_match(input),
                    reference_match(&parts, input),
                    "pattern {compiled} input {input:?}"
                );
            }
        }
    }

    #[test]
    fn simple_fold_matcher_agrees_with_backtracking_oracle() {
        let patterns = patterns(&[literal("k"), literal("ſ"), TextMatchPart::AnyMany], 3);
        let inputs = inputs(&['K', 'K', 'S', 'x'], 3);
        for parts in patterns {
            let compiled = TextPattern::compile(&parts, TextComparison::UnicodeSimpleFold).unwrap();
            for input in &inputs {
                assert_eq!(
                    compiled.is_match(input),
                    reference_fold_match(&parts, &input.chars().collect::<Vec<_>>()),
                    "pattern {compiled} input {input:?}"
                );
            }
        }
    }

    #[test]
    fn unicode_simple_fold_is_pinned_to_unicode_15() {
        for (old_source, unicode_16_target) in [
            ('\u{1c89}', '\u{1c8a}'),
            ('\u{1fd3}', '\u{0390}'),
            ('\u{1fe3}', '\u{03b0}'),
            ('\u{a7cb}', '\u{0264}'),
            ('\u{a7cc}', '\u{a7cd}'),
            ('\u{a7da}', '\u{a7db}'),
            ('\u{a7dc}', '\u{019b}'),
            ('\u{fb05}', '\u{fb06}'),
            ('\u{10d50}', '\u{10d70}'),
            ('\u{10d65}', '\u{10d85}'),
        ] {
            let compiled = TextPattern::compile(
                &[literal(&old_source.to_string())],
                TextComparison::UnicodeSimpleFold,
            )
            .unwrap();
            assert!(!compiled.is_match(&unicode_16_target.to_string()));
        }
    }
}
