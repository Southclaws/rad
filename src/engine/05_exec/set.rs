//! Shared state machine for physical set operators.

use std::collections::{HashMap, HashSet};

use crate::engine::lir::eval::{CanonicalRowKey, Env, canonical_row_key};
use crate::engine::lir::{RowType, SetQuantifier};

pub(super) struct State {
    quantifier: SetQuantifier,
    subtract: bool,
    right_counts: HashMap<CanonicalRowKey, usize>,
    emitted: HashSet<CanonicalRowKey>,
}

impl State {
    pub(super) fn new(quantifier: SetQuantifier, subtract: bool) -> Self {
        Self {
            quantifier,
            subtract,
            right_counts: HashMap::new(),
            emitted: HashSet::new(),
        }
    }

    pub(super) fn add_right(&mut self, output: &RowType, frame: &Env) {
        *self
            .right_counts
            .entry(canonical_row_key(&output.fields, frame))
            .or_default() += 1;
    }

    pub(super) fn keep_left(&mut self, output: &RowType, frame: &Env) -> bool {
        let key = canonical_row_key(&output.fields, frame);
        if self.quantifier == SetQuantifier::Distinct {
            if self.emitted.contains(&key) {
                return false;
            }
            let in_right = self.right_counts.get(&key).copied().unwrap_or_default() > 0;
            let keep = self.subtract != in_right;
            if keep {
                self.emitted.insert(key);
            }
            return keep;
        }
        if let Some(count) = self.right_counts.get_mut(&key).filter(|count| **count > 0) {
            *count -= 1;
            !self.subtract
        } else {
            self.subtract
        }
    }
}
