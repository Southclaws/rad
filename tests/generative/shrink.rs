use super::{
    Case, ModelCase, ProgramCase, SemanticModelCase, TestResult, check, check_invalid,
    check_invalid_program, check_metamorphic, check_model, check_program, check_semantic_model,
};

/// Minimize a failing choice tape while regenerating a valid case after every
/// edit. Chunk deletion removes whole schema/data/query decisions; value
/// reduction moves individual choices toward the generator's deliberately
/// simple zero branches. The returned case is guaranteed to retain the
/// original failure predicate.
pub async fn minimize(original: &Case, budget: usize) -> TestResult<Case> {
    let mut current = original.decisions.clone();
    let mut attempts = 0;

    let mut granularity = 2;
    while current.len() > 1 && attempts < budget {
        let chunk = current.len().div_ceil(granularity);
        let mut reduced = false;
        let mut start = 0;
        while start < current.len() && attempts < budget {
            let end = (start + chunk).min(current.len());
            let mut candidate = current.clone();
            candidate.drain(start..end);
            attempts += 1;
            if still_fails(original, &candidate).await {
                current = candidate;
                granularity = 2;
                reduced = true;
                break;
            }
            start = end;
        }
        if !reduced {
            if granularity >= current.len() {
                break;
            }
            granularity = (granularity * 2).min(current.len());
        }
    }

    for index in 0..current.len() {
        if attempts >= budget {
            break;
        }
        let original_value = current[index];
        for value in reductions(original_value) {
            if attempts >= budget {
                break;
            }
            let mut candidate = current.clone();
            candidate[index] = value;
            attempts += 1;
            if still_fails(original, &candidate).await {
                current = candidate;
                break;
            }
        }
    }
    Ok(original.regenerate(current))
}

/// Minimize a failing PIR decision tape while rebuilding a complete valid
/// program and its independent state-model expectation after every edit.
pub async fn minimize_program(original: &ProgramCase, budget: usize) -> TestResult<ProgramCase> {
    minimize_program_decisions(original, budget, program_still_fails).await
}

/// Minimize a failing invalid-PIR campaign while preserving its generated
/// table/data choices and the complete rejection/rollback oracle set.
pub async fn minimize_invalid_program(
    original: &ProgramCase,
    budget: usize,
) -> TestResult<ProgramCase> {
    minimize_program_decisions(original, budget, invalid_program_still_fails).await
}

async fn minimize_program_decisions<F, Fut>(
    original: &ProgramCase,
    budget: usize,
    still_fails: F,
) -> TestResult<ProgramCase>
where
    F: Fn(Vec<u64>) -> Fut + Copy,
    Fut: std::future::Future<Output = bool>,
{
    let mut current = original.decisions.clone();
    let mut attempts = 0;
    let mut granularity = 2;
    while current.len() > 1 && attempts < budget {
        let chunk = current.len().div_ceil(granularity);
        let mut reduced = false;
        let mut start = 0;
        while start < current.len() && attempts < budget {
            let end = (start + chunk).min(current.len());
            let mut candidate = current.clone();
            candidate.drain(start..end);
            attempts += 1;
            if still_fails(candidate.clone()).await {
                current = candidate;
                granularity = 2;
                reduced = true;
                break;
            }
            start = end;
        }
        if !reduced {
            if granularity >= current.len() {
                break;
            }
            granularity = (granularity * 2).min(current.len());
        }
    }
    for index in 0..current.len() {
        if attempts >= budget {
            break;
        }
        for value in reductions(current[index]) {
            if attempts >= budget {
                break;
            }
            let mut candidate = current.clone();
            candidate[index] = value;
            attempts += 1;
            if still_fails(candidate.clone()).await {
                current = candidate;
                break;
            }
        }
    }
    Ok(ProgramCase::generate(current))
}

pub async fn minimize_metamorphic(original: &Case, budget: usize) -> TestResult<Case> {
    let mut decisions = original.decisions.clone();
    let mut attempts = 0;
    let mut granularity = 2;
    while decisions.len() > 1 && attempts < budget {
        let chunk = decisions.len().div_ceil(granularity);
        let mut reduced = false;
        for start in (0..decisions.len()).step_by(chunk) {
            if attempts >= budget {
                break;
            }
            let end = (start + chunk).min(decisions.len());
            let mut candidate = decisions.clone();
            candidate.drain(start..end);
            attempts += 1;
            if metamorphic_still_fails(original, &candidate).await {
                decisions = candidate;
                granularity = 2;
                reduced = true;
                break;
            }
        }
        if !reduced {
            if granularity >= decisions.len() {
                break;
            }
            granularity = (granularity * 2).min(decisions.len());
        }
    }
    for index in 0..decisions.len() {
        if attempts >= budget {
            break;
        }
        let original_value = decisions[index];
        for value in reductions(original_value) {
            if attempts >= budget {
                break;
            }
            let mut candidate = decisions.clone();
            candidate[index] = value;
            attempts += 1;
            if metamorphic_still_fails(original, &candidate).await {
                decisions = candidate;
                break;
            }
        }
    }
    Ok(original.regenerate(decisions))
}

pub async fn minimize_invalid(original: &Case, budget: usize) -> TestResult<Case> {
    let mut decisions = original.decisions.clone();
    let mut attempts = 0;
    let mut granularity = 2;
    while decisions.len() > 1 && attempts < budget {
        let chunk = decisions.len().div_ceil(granularity);
        let mut reduced = false;
        for start in (0..decisions.len()).step_by(chunk) {
            if attempts >= budget {
                break;
            }
            let end = (start + chunk).min(decisions.len());
            let mut candidate = decisions.clone();
            candidate.drain(start..end);
            attempts += 1;
            if invalid_still_fails(original, &candidate).await {
                decisions = candidate;
                granularity = 2;
                reduced = true;
                break;
            }
        }
        if !reduced {
            if granularity >= decisions.len() {
                break;
            }
            granularity = (granularity * 2).min(decisions.len());
        }
    }
    for index in 0..decisions.len() {
        if attempts >= budget {
            break;
        }
        let original_value = decisions[index];
        for value in reductions(original_value) {
            if attempts >= budget {
                break;
            }
            let mut candidate = decisions.clone();
            candidate[index] = value;
            attempts += 1;
            if invalid_still_fails(original, &candidate).await {
                decisions = candidate;
                break;
            }
        }
    }
    Ok(original.regenerate(decisions))
}

pub async fn minimize_model(original: &ModelCase, budget: usize) -> TestResult<ModelCase> {
    let mut current = original.decisions.clone();
    let mut attempts = 0;
    let mut granularity = 2;
    while current.len() > 1 && attempts < budget {
        let chunk = current.len().div_ceil(granularity);
        let mut reduced = false;
        let mut start = 0;
        while start < current.len() && attempts < budget {
            let end = (start + chunk).min(current.len());
            let mut candidate = current.clone();
            candidate.drain(start..end);
            attempts += 1;
            if model_still_fails(&candidate).await {
                current = candidate;
                granularity = 2;
                reduced = true;
                break;
            }
            start = end;
        }
        if !reduced {
            if granularity >= current.len() {
                break;
            }
            granularity = (granularity * 2).min(current.len());
        }
    }
    for index in 0..current.len() {
        if attempts >= budget {
            break;
        }
        for value in reductions(current[index]) {
            if attempts >= budget {
                break;
            }
            let mut candidate = current.clone();
            candidate[index] = value;
            attempts += 1;
            if model_still_fails(&candidate).await {
                current = candidate;
                break;
            }
        }
    }
    Ok(ModelCase::generate(current))
}

pub async fn minimize_semantic_model(
    original: &SemanticModelCase,
    budget: usize,
) -> TestResult<SemanticModelCase> {
    let mut current = original.decisions.clone();
    let mut attempts = 0;
    let mut granularity = 2;
    while current.len() > 1 && attempts < budget {
        let chunk = current.len().div_ceil(granularity);
        let mut reduced = false;
        let mut start = 0;
        while start < current.len() && attempts < budget {
            let end = (start + chunk).min(current.len());
            let mut candidate = current.clone();
            candidate.drain(start..end);
            attempts += 1;
            if semantic_model_still_fails(&candidate).await {
                current = candidate;
                granularity = 2;
                reduced = true;
                break;
            }
            start = end;
        }
        if !reduced {
            if granularity >= current.len() {
                break;
            }
            granularity = (granularity * 2).min(current.len());
        }
    }
    for index in 0..current.len() {
        if attempts >= budget {
            break;
        }
        for value in reductions(current[index]) {
            if attempts >= budget {
                break;
            }
            let mut candidate = current.clone();
            candidate[index] = value;
            attempts += 1;
            if semantic_model_still_fails(&candidate).await {
                current = candidate;
                break;
            }
        }
    }
    Ok(SemanticModelCase::generate(current))
}

async fn still_fails(original: &Case, decisions: &[u64]) -> bool {
    check(&original.regenerate(decisions.to_vec()))
        .await
        .is_err()
}

async fn program_still_fails(decisions: Vec<u64>) -> bool {
    check_program(&ProgramCase::generate(decisions))
        .await
        .is_err()
}

async fn invalid_program_still_fails(decisions: Vec<u64>) -> bool {
    check_invalid_program(&ProgramCase::generate(decisions))
        .await
        .is_err()
}

async fn metamorphic_still_fails(original: &Case, decisions: &[u64]) -> bool {
    check_metamorphic(&original.regenerate(decisions.to_vec()))
        .await
        .is_err()
}

async fn invalid_still_fails(original: &Case, decisions: &[u64]) -> bool {
    check_invalid(&original.regenerate(decisions.to_vec()))
        .await
        .is_err()
}

async fn model_still_fails(decisions: &[u64]) -> bool {
    check_model(&ModelCase::generate(decisions.to_vec()))
        .await
        .is_err()
}

async fn semantic_model_still_fails(decisions: &[u64]) -> bool {
    check_semantic_model(&SemanticModelCase::generate(decisions.to_vec()))
        .await
        .is_err()
}

fn reductions(value: u64) -> Vec<u64> {
    let mut values = vec![0];
    let mut reduced = value;
    while reduced > 1 {
        reduced /= 2;
        if !values.contains(&reduced) {
            values.push(reduced);
        }
    }
    values.sort_unstable();
    values
}

#[cfg(test)]
mod tests {
    use super::reductions;

    #[test]
    fn reductions_are_unique_and_move_toward_zero() {
        assert_eq!(reductions(0), vec![0]);
        assert_eq!(reductions(9), vec![0, 1, 2, 4]);
    }
}
