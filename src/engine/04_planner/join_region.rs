use super::physical::{
    AccessQuantity, JoinGraphCandidate, JoinGraphClassification, JoinGraphCost, JoinGraphDecision,
    JoinGraphDecisionBasis, JoinGraphRejectionReason,
};

pub(super) struct StrategyCandidate<T> {
    pub method: &'static str,
    pub cost: Option<JoinGraphCost>,
    pub rejection_reason: Option<JoinGraphRejectionReason>,
    pub plan: Option<T>,
}

pub(super) struct DecisionMetadata {
    pub classification: JoinGraphClassification,
    pub input_count: usize,
    pub edge_count: usize,
    pub root_input: usize,
    pub semijoin_passes: u32,
    pub predicate_transfer_passes: u32,
}

pub(super) struct StrategySelection<T> {
    selected: usize,
    structural_fallback: usize,
    candidates: Vec<StrategyCandidate<T>>,
}

impl<T> StrategySelection<T> {
    pub fn finish(self, metadata: DecisionMetadata) -> (usize, T, JoinGraphDecision) {
        let mut selected_plan = None;
        let candidates = self
            .candidates
            .into_iter()
            .enumerate()
            .map(|(index, candidate)| {
                if index == self.selected {
                    selected_plan = candidate.plan;
                }
                JoinGraphCandidate {
                    method: candidate.method.into(),
                    cost: candidate.cost,
                    decision_basis: (index == self.selected).then_some(
                        if index == self.structural_fallback {
                            JoinGraphDecisionBasis::Structural
                        } else {
                            JoinGraphDecisionBasis::BoundedNoRegret
                        },
                    ),
                    rejection_reason: candidate.rejection_reason,
                    chosen: index == self.selected,
                }
            })
            .collect();
        let decision = JoinGraphDecision {
            classification: metadata.classification,
            input_count: metadata.input_count,
            edge_count: metadata.edge_count,
            root_input: metadata.root_input,
            semijoin_passes: metadata.semijoin_passes,
            predicate_transfer_passes: metadata.predicate_transfer_passes,
            candidates,
            structural_fallback: self.structural_fallback,
        };
        (
            self.selected,
            selected_plan.expect("the selected join-region strategy has a physical plan"),
            decision,
        )
    }
}

pub(super) fn select<T>(
    mut candidates: Vec<StrategyCandidate<T>>,
    structural_fallback: usize,
    cost_selection_enabled: bool,
) -> StrategySelection<T> {
    assert!(
        candidates
            .get(structural_fallback)
            .is_some_and(|candidate| candidate.plan.is_some()),
        "the structural join-region strategy must have a physical plan"
    );
    let costs = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.rejection_reason.is_none())
        .filter_map(|(index, candidate)| candidate.cost.map(|cost| (index, cost)))
        .collect::<Vec<_>>();
    let selected = if cost_selection_enabled {
        strict_winner(&costs).unwrap_or(structural_fallback)
    } else {
        structural_fallback
    };
    for (index, candidate) in candidates.iter_mut().enumerate() {
        if index == selected || candidate.rejection_reason.is_some() {
            continue;
        }
        candidate.rejection_reason = Some(if index == structural_fallback {
            JoinGraphRejectionReason::MoreExpensive
        } else {
            loser_reason(selected, index, &costs, cost_selection_enabled)
        });
    }
    StrategySelection {
        selected,
        structural_fallback,
        candidates,
    }
}

fn strict_winner(candidates: &[(usize, JoinGraphCost)]) -> Option<usize> {
    candidates.iter().find_map(|(index, candidate)| {
        candidates
            .iter()
            .filter(|(other, _)| other != index)
            .all(|(_, other)| {
                scenario_cost_dominates(
                    candidate.logical_row_operations,
                    other.logical_row_operations,
                )
            })
            .then_some(*index)
    })
}

fn loser_reason(
    selected: usize,
    candidate: usize,
    costs: &[(usize, JoinGraphCost)],
    cost_selection_enabled: bool,
) -> JoinGraphRejectionReason {
    if !cost_selection_enabled {
        return JoinGraphRejectionReason::StructuralFallback;
    }
    let selected_cost = costs
        .iter()
        .find(|(index, _)| *index == selected)
        .map(|(_, cost)| *cost);
    let candidate_cost = costs
        .iter()
        .find(|(index, _)| *index == candidate)
        .map(|(_, cost)| *cost);
    if selected_cost
        .zip(candidate_cost)
        .is_some_and(|(selected, candidate)| {
            scenario_cost_dominates(
                selected.logical_row_operations,
                candidate.logical_row_operations,
            )
        })
    {
        JoinGraphRejectionReason::MoreExpensive
    } else {
        JoinGraphRejectionReason::CostOverlap
    }
}

pub(super) fn scenario_cost_dominates(candidate: AccessQuantity, fallback: AccessQuantity) -> bool {
    let Some(candidate_upper) = candidate.upper_bound else {
        return false;
    };
    let Some(fallback_upper) = fallback.upper_bound else {
        return false;
    };
    candidate.lower_bound <= fallback.lower_bound
        && candidate.central <= fallback.central
        && candidate_upper <= fallback_upper
        && (candidate.lower_bound < fallback.lower_bound
            || candidate.central < fallback.central
            || candidate_upper < fallback_upper)
}

#[cfg(test)]
mod tests {
    use super::super::physical::AccessOrdering;
    use super::*;

    fn cost(lower: u64, central: u64, upper: u64) -> JoinGraphCost {
        JoinGraphCost {
            logical_row_operations: AccessQuantity {
                central,
                lower_bound: lower,
                upper_bound: Some(upper),
            },
            reduction_row_operations: None,
            lookup_row_operations: None,
            expanded_rows: None,
            filter_row_operations: None,
            filtered_rows: None,
            filter_bytes: None,
            filter_paths: None,
            filter_builds: None,
            shared_filter_paths: None,
            pruned_filter_paths: None,
            filter_input_scans: None,
            filter_schedule_root: None,
            peak_retained_bytes: None,
            ordering: AccessOrdering::NotRequired,
        }
    }

    fn candidate(method: &'static str, cost: JoinGraphCost) -> StrategyCandidate<&'static str> {
        StrategyCandidate {
            method,
            cost: Some(cost),
            rejection_reason: None,
            plan: Some(method),
        }
    }

    fn metadata() -> DecisionMetadata {
        DecisionMetadata {
            classification: JoinGraphClassification::Acyclic,
            input_count: 3,
            edge_count: 2,
            root_input: 0,
            semijoin_passes: 4,
            predicate_transfer_passes: 2,
        }
    }

    #[test]
    fn structural_mode_keeps_the_fallback() {
        let selection = select(
            vec![
                candidate("binary", cost(100, 100, 100)),
                candidate("shredded", cost(10, 10, 10)),
            ],
            0,
            false,
        );
        let (selected, plan, decision) = selection.finish(metadata());
        assert_eq!(selected, 0);
        assert_eq!(plan, "binary");
        assert_eq!(
            decision.candidates[1].rejection_reason,
            Some(JoinGraphRejectionReason::StructuralFallback)
        );
    }

    #[test]
    fn cost_mode_selects_one_strict_winner() {
        let selection = select(
            vec![
                candidate("binary", cost(100, 100, 100)),
                candidate("shredded", cost(10, 10, 10)),
                StrategyCandidate {
                    method: "predicate",
                    cost: None,
                    rejection_reason: Some(JoinGraphRejectionReason::MissingEvidence),
                    plan: None,
                },
            ],
            0,
            true,
        );
        let (selected, plan, decision) = selection.finish(metadata());
        assert_eq!(selected, 1);
        assert_eq!(plan, "shredded");
        assert_eq!(
            decision.candidates[0].rejection_reason,
            Some(JoinGraphRejectionReason::MoreExpensive)
        );
        assert_eq!(
            decision.candidates[2].rejection_reason,
            Some(JoinGraphRejectionReason::MissingEvidence)
        );
    }

    #[test]
    fn overlapping_costs_keep_the_fallback() {
        let selection = select(
            vec![
                candidate("binary", cost(0, 30, 100)),
                candidate("shredded", cost(10, 20, 90)),
            ],
            0,
            true,
        );
        let (selected, plan, decision) = selection.finish(metadata());
        assert_eq!(selected, 0);
        assert_eq!(plan, "binary");
        assert_eq!(
            decision.candidates[1].rejection_reason,
            Some(JoinGraphRejectionReason::CostOverlap)
        );
    }
}
