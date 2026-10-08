use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::*;

const SPEC_PATH: &str = "tests/benchmarks/relation_cache/falsification.yaml";
const SPEC_FORMAT: &str = "rad-relation-cache-falsification-v1";
const FACTOR_COUNT: usize = 7;

#[derive(Debug, Deserialize)]
struct FalsificationSpec {
    format: String,
    name: String,
    description: String,
    family_size: usize,
    included_scenarios: Vec<String>,
    axes: FalsificationAxes,
    targets: FalsificationTargets,
}

#[derive(Debug, Deserialize)]
struct FalsificationAxes {
    conversion_percent: Vec<u8>,
    future_reuses: Vec<usize>,
    result_profile: Vec<ResultProfile>,
    cohort_count: Vec<usize>,
    phase_delay_milliseconds: Vec<u64>,
    cache_pressure: Vec<CachePressure>,
    conversion_order: Vec<ConversionOrder>,
}

impl FalsificationAxes {
    fn level_counts(&self) -> [usize; FACTOR_COUNT] {
        [
            self.conversion_percent.len(),
            self.future_reuses.len(),
            self.result_profile.len(),
            self.cohort_count.len(),
            self.phase_delay_milliseconds.len(),
            self.cache_pressure.len(),
            self.conversion_order.len(),
        ]
    }

    fn factors(&self, levels: [usize; FACTOR_COUNT]) -> FalsificationFactors {
        FalsificationFactors {
            conversion_percent: self.conversion_percent[levels[0]],
            future_reuses: self.future_reuses[levels[1]],
            result_profile: self.result_profile[levels[2]],
            cohort_count: self.cohort_count[levels[3]],
            phase_delay_milliseconds: self.phase_delay_milliseconds[levels[4]],
            cache_pressure: self.cache_pressure[levels[5]],
            conversion_order: self.conversion_order[levels[6]],
        }
    }
}

#[derive(Debug, Deserialize)]
struct FalsificationTargets {
    max_wasteful_to_useful_admission_percent: u64,
    min_useful_work_retained_percent: u64,
    min_useful_scenario_work_retained_percent: u64,
    min_pollution_work_retained_percent: u64,
    protected_work: Vec<ProtectedWorkTarget>,
}

#[derive(Debug, Deserialize)]
struct ProtectedWorkTarget {
    scenario: String,
    domain: ProtectedDomain,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProtectedDomain {
    Query,
    HashBuild,
    GroupedDimension,
}

impl ProtectedDomain {
    fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::HashBuild => "hash_build",
            Self::GroupedDimension => "grouped_dimension",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ResultProfile {
    PointSmall,
    PointLarge,
    Range,
    AggregateRange,
}

impl ResultProfile {
    fn as_str(self) -> &'static str {
        match self {
            Self::PointSmall => "point-small",
            Self::PointLarge => "point-large",
            Self::Range => "range",
            Self::AggregateRange => "aggregate-range",
        }
    }

    fn query(self) -> &'static str {
        match self {
            Self::PointSmall | Self::PointLarge => "point",
            Self::Range => "range",
            Self::AggregateRange => "aggregate-range",
        }
    }

    fn dataset_rows(self) -> usize {
        match self {
            Self::PointSmall | Self::PointLarge => 128,
            Self::Range | Self::AggregateRange => 512,
        }
    }

    fn payload_bytes(self) -> usize {
        match self {
            Self::PointSmall => 256,
            Self::PointLarge => 64 * 1024,
            Self::Range | Self::AggregateRange => 4 * 1024,
        }
    }

    fn estimated_result_bytes(self) -> usize {
        match self {
            Self::PointSmall => 512,
            Self::PointLarge => 65 * 1024,
            Self::Range => 48 * 1024,
            Self::AggregateRange => 1024,
        }
    }

    fn read_pattern(self) -> ReadPattern {
        match self {
            Self::PointSmall | Self::PointLarge => ReadPattern::Uniform,
            Self::Range => ReadPattern::HistoricalRange,
            Self::AggregateRange => ReadPattern::Aggregate,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CachePressure {
    Fits,
    Double,
    Quadruple,
}

impl CachePressure {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fits => "fits",
            Self::Double => "double",
            Self::Quadruple => "quadruple",
        }
    }

    fn divisor(self) -> usize {
        match self {
            Self::Fits => 1,
            Self::Double => 2,
            Self::Quadruple => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ConversionOrder {
    ColdFirst,
    HotFirst,
    Alternating,
}

impl ConversionOrder {
    fn as_str(self) -> &'static str {
        match self {
            Self::ColdFirst => "cold-first",
            Self::HotFirst => "hot-first",
            Self::Alternating => "alternating",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(super) struct FalsificationFactors {
    conversion_percent: u8,
    future_reuses: usize,
    result_profile: ResultProfile,
    cohort_count: usize,
    phase_delay_milliseconds: u64,
    cache_pressure: CachePressure,
    conversion_order: ConversionOrder,
}

#[derive(Debug)]
struct TargetEvaluation {
    report: String,
    failures: Vec<String>,
}

fn load_spec() -> TestResult<FalsificationSpec> {
    let spec: FalsificationSpec = serde_yaml::from_slice(&std::fs::read(SPEC_PATH)?)?;
    if spec.format != SPEC_FORMAT {
        return Err(format!(
            "unknown relation cache falsification format {:?}",
            spec.format
        )
        .into());
    }
    if spec.name.is_empty() || spec.description.is_empty() || spec.family_size == 0 {
        return Err("relation cache falsification metadata is incomplete".into());
    }
    let level_counts = spec.axes.level_counts();
    if level_counts.contains(&0) {
        return Err("relation cache falsification axes must not be empty".into());
    }
    if spec
        .axes
        .conversion_percent
        .iter()
        .any(|percent| *percent > 100)
    {
        return Err("relation cache conversion percentages must be at most 100".into());
    }
    if spec.axes.future_reuses.contains(&0) || spec.axes.cohort_count.contains(&0) {
        return Err("relation cache falsification counts must be positive".into());
    }
    if [
        spec.targets.max_wasteful_to_useful_admission_percent,
        spec.targets.min_useful_work_retained_percent,
        spec.targets.min_useful_scenario_work_retained_percent,
        spec.targets.min_pollution_work_retained_percent,
    ]
    .into_iter()
    .any(|percent| percent > 100)
    {
        return Err("relation cache falsification target percentages must be at most 100".into());
    }
    Ok(spec)
}

fn pairwise_levels(level_counts: [usize; FACTOR_COUNT]) -> TestResult<Vec<[usize; FACTOR_COUNT]>> {
    if level_counts.contains(&0) {
        return Err("pairwise factors must not be empty".into());
    }
    let candidate_count = level_counts
        .iter()
        .try_fold(1usize, |total, count| total.checked_mul(*count))
        .ok_or("pairwise candidate count overflow")?;
    let candidates = (0..candidate_count)
        .map(|ordinal| decode_levels(ordinal, level_counts))
        .collect::<Vec<_>>();
    let mut uncovered = BTreeSet::new();
    for left_axis in 0..FACTOR_COUNT {
        for right_axis in left_axis + 1..FACTOR_COUNT {
            for left_level in 0..level_counts[left_axis] {
                for right_level in 0..level_counts[right_axis] {
                    uncovered.insert((left_axis, left_level, right_axis, right_level));
                }
            }
        }
    }
    let mut selected = Vec::new();
    while !uncovered.is_empty() {
        let mut best = None;
        let mut best_score = 0usize;
        for candidate in &candidates {
            let score = uncovered_pairs(candidate, &uncovered);
            if score > best_score {
                best = Some(*candidate);
                best_score = score;
            }
        }
        let Some(best) = best else {
            return Err("pairwise generator cannot cover the remaining factor pairs".into());
        };
        remove_pairs(&best, &mut uncovered);
        selected.push(best);
    }
    Ok(selected)
}

fn decode_levels(mut ordinal: usize, level_counts: [usize; FACTOR_COUNT]) -> [usize; FACTOR_COUNT] {
    let mut levels = [0; FACTOR_COUNT];
    for axis in (0..FACTOR_COUNT).rev() {
        levels[axis] = ordinal % level_counts[axis];
        ordinal /= level_counts[axis];
    }
    levels
}

fn uncovered_pairs(
    levels: &[usize; FACTOR_COUNT],
    uncovered: &BTreeSet<(usize, usize, usize, usize)>,
) -> usize {
    let mut count = 0;
    for left_axis in 0..FACTOR_COUNT {
        for right_axis in left_axis + 1..FACTOR_COUNT {
            count += usize::from(uncovered.contains(&(
                left_axis,
                levels[left_axis],
                right_axis,
                levels[right_axis],
            )));
        }
    }
    count
}

fn remove_pairs(
    levels: &[usize; FACTOR_COUNT],
    uncovered: &mut BTreeSet<(usize, usize, usize, usize)>,
) {
    for left_axis in 0..FACTOR_COUNT {
        for right_axis in left_axis + 1..FACTOR_COUNT {
            uncovered.remove(&(left_axis, levels[left_axis], right_axis, levels[right_axis]));
        }
    }
}

fn generated_scenarios(spec: &FalsificationSpec) -> TestResult<Vec<Scenario>> {
    pairwise_levels(spec.axes.level_counts())?
        .into_iter()
        .enumerate()
        .map(|(index, levels)| scenario(spec, index, spec.axes.factors(levels)))
        .collect()
}

fn scenario(
    spec: &FalsificationSpec,
    index: usize,
    factors: FalsificationFactors,
) -> TestResult<Scenario> {
    let profile = factors.result_profile;
    let pressure_divisor = factors.cache_pressure.divisor();
    let entry_limit = spec.family_size.div_ceil(pressure_divisor).max(1);
    let result_limit = profile.estimated_result_bytes().saturating_mul(2);
    let byte_limit = entry_limit
        .saturating_mul(profile.estimated_result_bytes())
        .saturating_mul(2)
        .max(result_limit);
    let mut steps = Vec::new();
    for cohort in 0..factors.cohort_count {
        steps.extend(family_steps(spec.family_size, factors));
        if cohort + 1 < factors.cohort_count {
            if factors.phase_delay_milliseconds > 0 {
                steps.push(time_advance(factors.phase_delay_milliseconds));
            }
            steps.push(mutation());
            steps.push(commit());
        }
    }
    let expected = match (factors.conversion_percent, factors.cohort_count) {
        (0, 1) => ExpectedValue::Wasteful,
        (100, _) => ExpectedValue::Useful,
        _ => ExpectedValue::Mixed,
    };
    let name = format!(
        "falsify-{index:02}-c{}-r{}-{}-g{}-d{}-{}-{}",
        factors.conversion_percent,
        factors.future_reuses,
        profile.as_str(),
        factors.cohort_count,
        factors.phase_delay_milliseconds,
        factors.cache_pressure.as_str(),
        factors.conversion_order.as_str(),
    );
    let scenario = Scenario {
        name,
        expected,
        suite: WorkloadSuite::Falsification,
        shape: WorkloadShape {
            mutation_pattern: if factors.cohort_count == 1 {
                MutationPattern::None
            } else {
                MutationPattern::Append
            },
            read_pattern: profile.read_pattern(),
            literal_distribution: LiteralDistribution::Monotonic,
        },
        dataset_rows: profile.dataset_rows().max(spec.family_size + 10),
        dimension_rows: default_dimension_rows(),
        fact_rows: default_fact_rows(),
        seed_payload_bytes: profile.payload_bytes(),
        cache_capacity_bytes: Some(byte_limit),
        cache_entry_limit: Some(entry_limit),
        result_limit_bytes: Some(result_limit),
        falsification: Some(factors),
        steps,
    };
    expand(&scenario.steps)?;
    Ok(scenario)
}

fn family_steps(family_size: usize, factors: FalsificationFactors) -> Vec<Step> {
    let converted = family_size
        .saturating_mul(usize::from(factors.conversion_percent))
        .saturating_div(100);
    let cold = family_size.saturating_sub(converted);
    let hot_repeats = factors.future_reuses.saturating_add(2);
    match factors.conversion_order {
        ConversionOrder::ColdFirst => [
            query_series(factors.result_profile.query(), cold, converted, 2),
            query_series(factors.result_profile.query(), converted, 0, hot_repeats),
        ]
        .into_iter()
        .flatten()
        .collect(),
        ConversionOrder::HotFirst => [
            query_series(factors.result_profile.query(), converted, 0, hot_repeats),
            query_series(factors.result_profile.query(), cold, converted, 2),
        ]
        .into_iter()
        .flatten()
        .collect(),
        ConversionOrder::Alternating => alternating_family_steps(
            factors.result_profile.query(),
            family_size,
            converted,
            hot_repeats,
        ),
    }
}

fn alternating_family_steps(
    query: &str,
    family_size: usize,
    converted: usize,
    hot_repeats: usize,
) -> Vec<Step> {
    let converted_literals = (0..converted)
        .map(|index| index.saturating_mul(family_size) / converted.max(1))
        .collect::<BTreeSet<_>>();
    (0..family_size)
        .filter_map(|literal| {
            query_series(
                query,
                1,
                literal,
                if converted_literals.contains(&literal) {
                    hot_repeats
                } else {
                    2
                },
            )
        })
        .collect()
}

fn query_series(query: &str, count: usize, offset: usize, repeat_each: usize) -> Option<Step> {
    (count > 0).then(|| Step {
        kind: StepKind::QuerySeries,
        query: Some(query.to_owned()),
        table: None,
        count: Some(count),
        payload_bytes: None,
        milliseconds: None,
        interval_milliseconds: None,
        literal_distribution: Some(LiteralDistribution::Monotonic),
        literal_cardinality: Some(count),
        literal_offset: Some(offset),
        repeat_each: Some(repeat_each),
        mutation_pattern: None,
        hot_set_size: None,
        gate_fill: false,
        name: None,
        steps: Vec::new(),
    })
}

fn time_advance(milliseconds: u64) -> Step {
    Step {
        kind: StepKind::TimeAdvance,
        query: None,
        table: None,
        count: None,
        payload_bytes: None,
        milliseconds: Some(milliseconds),
        interval_milliseconds: None,
        literal_distribution: None,
        literal_cardinality: None,
        literal_offset: None,
        repeat_each: None,
        mutation_pattern: None,
        hot_set_size: None,
        gate_fill: false,
        name: None,
        steps: Vec::new(),
    }
}

fn mutation() -> Step {
    Step {
        kind: StepKind::Mutation,
        query: None,
        table: Some("items".to_owned()),
        count: Some(1),
        payload_bytes: Some(16),
        milliseconds: None,
        interval_milliseconds: None,
        literal_distribution: None,
        literal_cardinality: None,
        literal_offset: None,
        repeat_each: None,
        mutation_pattern: Some(MutationPattern::Append),
        hot_set_size: None,
        gate_fill: false,
        name: None,
        steps: Vec::new(),
    }
}

fn commit() -> Step {
    Step {
        kind: StepKind::Commit,
        query: None,
        table: None,
        count: None,
        payload_bytes: None,
        milliseconds: None,
        interval_milliseconds: None,
        literal_distribution: None,
        literal_cardinality: None,
        literal_offset: None,
        repeat_each: None,
        mutation_pattern: None,
        hot_set_size: None,
        gate_fill: false,
        name: None,
        steps: Vec::new(),
    }
}

fn all_scenarios(manifest: &Manifest, spec: &FalsificationSpec) -> TestResult<Vec<Scenario>> {
    let mut scenarios = generated_scenarios(spec)?;
    for name in &spec.included_scenarios {
        let scenario = manifest
            .scenarios
            .iter()
            .find(|scenario| scenario.name == *name)
            .ok_or_else(|| format!("falsification scenario {name:?} is missing"))?;
        scenarios.push(scenario.clone());
    }
    let names = scenarios
        .iter()
        .map(|scenario| scenario.name.as_str())
        .collect::<BTreeSet<_>>();
    if names.len() != scenarios.len() {
        return Err("relation cache falsification scenario names are not unique".into());
    }
    for target in &spec.targets.protected_work {
        if !names.contains(target.scenario.as_str()) {
            return Err(format!("protected work scenario {:?} is missing", target.scenario).into());
        }
    }
    Ok(scenarios)
}

fn evaluate(spec: &FalsificationSpec, results: &[BenchmarkResult]) -> TestResult<TargetEvaluation> {
    let baseline = results_for(results, RelationCacheReuseAdmission::SecondTouch)?;
    let candidate = results_for(results, RelationCacheReuseAdmission::FamilyConversion)?;
    let mut useful_baseline_work = 0u64;
    let mut useful_candidate_work = 0u64;
    let mut useful_candidate_admissions = 0u64;
    let mut wasteful_candidate_admissions = 0u64;
    let mut useful_rows = Vec::new();
    for (name, baseline_result) in &baseline {
        let candidate_result = candidate
            .get(name)
            .ok_or_else(|| format!("candidate result is missing scenario {name:?}"))?;
        if baseline_result.correctness_hash != candidate_result.correctness_hash {
            return Err(format!("scenario {name:?} has different correctness hashes").into());
        }
        match baseline_result.expected {
            ExpectedValue::Useful => {
                let (baseline_work, scope) = protected_work(spec, baseline_result)?;
                let (candidate_work, candidate_scope) = protected_work(spec, candidate_result)?;
                if scope != candidate_scope {
                    return Err(
                        format!("scenario {name:?} has different protected work scopes").into(),
                    );
                }
                useful_baseline_work = useful_baseline_work.saturating_add(baseline_work);
                useful_candidate_work = useful_candidate_work.saturating_add(candidate_work);
                useful_candidate_admissions =
                    useful_candidate_admissions.saturating_add(candidate_result.admissions);
                useful_rows.push((
                    name.clone(),
                    scope,
                    retention_basis_points(candidate_work, baseline_work),
                ));
            }
            ExpectedValue::Wasteful => {
                wasteful_candidate_admissions =
                    wasteful_candidate_admissions.saturating_add(candidate_result.admissions);
            }
            ExpectedValue::Mixed | ExpectedValue::Safe | ExpectedValue::Rejected => {}
        }
    }
    let admission_ratio =
        retention_basis_points(wasteful_candidate_admissions, useful_candidate_admissions);
    let useful_retention = retention_basis_points(useful_candidate_work, useful_baseline_work);
    let pollution_retention = scenario_retention(&baseline, &candidate, "cache_pollution")?;
    let mut failures = Vec::new();
    if admission_ratio > i128::from(spec.targets.max_wasteful_to_useful_admission_percent) * 100 {
        failures.push(format!(
            "wasteful-to-useful admission ratio {} exceeds {}%",
            format_basis_points(admission_ratio),
            spec.targets.max_wasteful_to_useful_admission_percent,
        ));
    }
    if useful_retention < i128::from(spec.targets.min_useful_work_retained_percent) * 100 {
        failures.push(format!(
            "useful work retention {} is below {}%",
            format_basis_points(useful_retention),
            spec.targets.min_useful_work_retained_percent,
        ));
    }
    let minimum_scenario = useful_rows
        .iter()
        .map(|(_, _, retained)| *retained)
        .min()
        .unwrap_or_default();
    if minimum_scenario < i128::from(spec.targets.min_useful_scenario_work_retained_percent) * 100 {
        failures.push(format!(
            "minimum useful scenario retention {} is below {}%",
            format_basis_points(minimum_scenario),
            spec.targets.min_useful_scenario_work_retained_percent,
        ));
    }
    if pollution_retention < i128::from(spec.targets.min_pollution_work_retained_percent) * 100 {
        failures.push(format!(
            "cache pollution work retention {} is below {}%",
            format_basis_points(pollution_retention),
            spec.targets.min_pollution_work_retained_percent,
        ));
    }
    useful_rows.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let mut report = String::new();
    writeln!(report, "# Relation cache policy falsification")?;
    writeln!(report)?;
    writeln!(
        report,
        "- Pairwise scenarios: `{}`",
        generated_scenarios(spec)?.len()
    )?;
    writeln!(report, "- Total scenarios: `{}`", baseline.len())?;
    writeln!(
        report,
        "- Wasteful-to-useful admission ratio: `{}`",
        format_basis_points(admission_ratio),
    )?;
    writeln!(
        report,
        "- Useful work retained: `{}`",
        format_basis_points(useful_retention),
    )?;
    writeln!(
        report,
        "- Minimum useful scenario work retained: `{}`",
        format_basis_points(minimum_scenario),
    )?;
    writeln!(
        report,
        "- Cache pollution work retained: `{}`",
        format_basis_points(pollution_retention),
    )?;
    writeln!(report)?;
    writeln!(report, "## Protected useful work")?;
    writeln!(report)?;
    writeln!(report, "| Scenario | Scope | Work retained |")?;
    writeln!(report, "| --- | --- | ---: |")?;
    for (scenario, scope, retained) in &useful_rows {
        writeln!(
            report,
            "| `{scenario}` | `{scope}` | {} |",
            format_basis_points(*retained),
        )?;
    }
    writeln!(report)?;
    writeln!(
        report,
        "The corpus separates conditional reuse probability, retained size, fill work, generation count, evidence age, request order, and cache pressure. See Beckmann, Chen, and Cidon, \"LHD: Improving Cache Hit Rate by Maximizing Hit Density,\" NSDI 2018, Section 3.1."
    )?;
    writeln!(report)?;
    writeln!(
        report,
        "The pollution cases test admission under skew and one-hit traffic. See Einziger, Friedman, and Manes, \"TinyLFU: A Highly Efficient Cache Admission Policy,\" 2015, Sections 3 and 4."
    )?;
    writeln!(report)?;
    if failures.is_empty() {
        writeln!(report, "All falsification targets pass.")?;
    } else {
        writeln!(report, "## Failed targets")?;
        writeln!(report)?;
        for failure in &failures {
            writeln!(report, "- {failure}")?;
        }
    }
    Ok(TargetEvaluation { report, failures })
}

fn protected_work(spec: &FalsificationSpec, result: &BenchmarkResult) -> TestResult<(u64, String)> {
    let Some(target) = spec
        .targets
        .protected_work
        .iter()
        .find(|target| target.scenario == result.scenario)
    else {
        return Ok((result.work_avoided, "all".to_owned()));
    };
    let domain = target.domain.as_str();
    let work = result
        .domains
        .get(domain)
        .ok_or_else(|| {
            format!(
                "scenario {:?} is missing domain {domain:?}",
                result.scenario
            )
        })?
        .work_avoided;
    Ok((work, domain.to_owned()))
}

fn results_for(
    results: &[BenchmarkResult],
    reuse_admission: RelationCacheReuseAdmission,
) -> TestResult<BTreeMap<String, &BenchmarkResult>> {
    let selected = results
        .iter()
        .filter(|result| {
            result.policy_mode == RelationCachePolicyMode::Enforced.as_str()
                && result.prior == RelationCachePrior::None.as_str()
                && result.reuse_admission == reuse_admission.as_str()
        })
        .map(|result| (result.scenario.clone(), result))
        .collect::<BTreeMap<_, _>>();
    if selected.is_empty() {
        return Err(format!(
            "falsification results are missing reuse admission {:?}",
            reuse_admission.as_str(),
        )
        .into());
    }
    Ok(selected)
}

fn scenario_retention(
    baseline: &BTreeMap<String, &BenchmarkResult>,
    candidate: &BTreeMap<String, &BenchmarkResult>,
    scenario: &str,
) -> TestResult<i128> {
    let baseline = baseline
        .get(scenario)
        .ok_or_else(|| format!("baseline result is missing scenario {scenario:?}"))?;
    let candidate = candidate
        .get(scenario)
        .ok_or_else(|| format!("candidate result is missing scenario {scenario:?}"))?;
    Ok(retention_basis_points(
        candidate.work_avoided,
        baseline.work_avoided,
    ))
}

fn retention_basis_points(candidate: u64, baseline: u64) -> i128 {
    if baseline == 0 {
        return 10_000;
    }
    i128::from(candidate)
        .saturating_mul(10_000)
        .saturating_div(i128::from(baseline))
}

fn input_hash() -> TestResult<String> {
    let mut hash = Sha256::new();
    for path in [MANIFEST, SPEC_PATH] {
        hash.update((path.len() as u64).to_be_bytes());
        hash.update(path.as_bytes());
        hash.update(std::fs::read(path)?);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn write_run(path: &Path, phase: &str, results: Vec<BenchmarkResult>) -> TestResult {
    if phase.is_empty() {
        return Err("benchmark phase must not be empty".into());
    }
    let run = BenchmarkRun {
        format: RUN_FORMAT.to_owned(),
        phase: phase.to_owned(),
        benchmark_format: SPEC_FORMAT.to_owned(),
        benchmark: "relation-cache-policy-falsification".to_owned(),
        manifest_hash: input_hash()?,
        source_revision: source_revision(),
        source_hash: source_hash()?,
        results,
    };
    let mut output = serde_json::to_vec_pretty(&run)?;
    output.push(b'\n');
    write_file(path, &output)
}

#[test]
fn pairwise_generator_covers_every_factor_pair() {
    let spec = load_spec().expect("load relation cache falsification specification");
    let level_counts = spec.axes.level_counts();
    let cases = pairwise_levels(level_counts).expect("generate pairwise cases");
    let mut covered = BTreeSet::new();
    for case in &cases {
        for left_axis in 0..FACTOR_COUNT {
            for right_axis in left_axis + 1..FACTOR_COUNT {
                covered.insert((left_axis, case[left_axis], right_axis, case[right_axis]));
            }
        }
    }
    let expected = (0..FACTOR_COUNT)
        .flat_map(|left_axis| {
            (left_axis + 1..FACTOR_COUNT)
                .map(move |right_axis| level_counts[left_axis] * level_counts[right_axis])
        })
        .sum::<usize>();
    assert_eq!(covered.len(), expected);
}

#[test]
fn generated_falsification_scenarios_are_valid_and_unique() {
    let manifest = load_manifest().expect("load relation cache benchmark manifest");
    let spec = load_spec().expect("load relation cache falsification specification");
    let scenarios = all_scenarios(&manifest, &spec).expect("generate falsification scenarios");
    let names = scenarios
        .iter()
        .map(|scenario| scenario.name.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(names.len(), scenarios.len());
}

#[test]
fn generated_falsification_scenarios_cover_value_classes() {
    let spec = load_spec().expect("load relation cache falsification specification");
    let scenarios = generated_scenarios(&spec).expect("generate falsification scenarios");
    let expected = scenarios
        .iter()
        .map(|scenario| scenario.expected)
        .collect::<Vec<_>>();
    assert!(
        expected
            .iter()
            .any(|value| matches!(value, ExpectedValue::Useful))
    );
    assert!(
        expected
            .iter()
            .any(|value| matches!(value, ExpectedValue::Wasteful))
    );
    assert!(
        expected
            .iter()
            .any(|value| matches!(value, ExpectedValue::Mixed))
    );
}

#[test]
fn repeated_zero_conversion_scenarios_allow_cross_cohort_value() {
    let spec = load_spec().expect("load relation cache falsification specification");
    let scenario = generated_scenarios(&spec)
        .expect("generate falsification scenarios")
        .into_iter()
        .find(|scenario| {
            scenario
                .falsification
                .is_some_and(|factors| factors.conversion_percent == 0 && factors.cohort_count > 1)
        })
        .expect("find repeated zero conversion scenario");

    assert!(matches!(scenario.expected, ExpectedValue::Mixed));
}

#[tokio::test]
#[ignore = "emits deterministic policy falsification measurements"]
async fn relation_cache_falsification_corpus_meets_targets() -> TestResult {
    let manifest = load_manifest()?;
    let spec = load_spec()?;
    let scenarios = all_scenarios(&manifest, &spec)?;
    let mut results = Vec::new();
    for reuse_admission in [
        RelationCacheReuseAdmission::SecondTouch,
        RelationCacheReuseAdmission::FamilyConversion,
    ] {
        let mut config = policy_config(RelationCachePolicyMode::Enforced, RelationCachePrior::None);
        config.policy.reuse_admission = reuse_admission;
        config.policy.family_minimum_observations = 1;
        config.policy.probation_minimum_work_units = u64::MAX;
        for scenario in &scenarios {
            let result = run_scenario(&manifest, scenario, config).await?;
            results.push(result);
        }
    }
    let evaluation = evaluate(&spec, &results)?;
    print!("{}", evaluation.report);
    if let Ok(output) = std::env::var("RAD_RELATION_CACHE_BENCHMARK_OUTPUT") {
        let phase = std::env::var("RAD_RELATION_CACHE_BENCHMARK_PHASE")
            .map_err(|_| "RAD_RELATION_CACHE_BENCHMARK_PHASE is required with benchmark output")?;
        write_run(Path::new(&output), &phase, results)?;
    }
    if let Ok(output) = std::env::var("RAD_RELATION_CACHE_BENCHMARK_REPORT") {
        write_file(Path::new(&output), evaluation.report.as_bytes())?;
    }
    if !evaluation.failures.is_empty() {
        return Err(evaluation.failures.join("; ").into());
    }
    Ok(())
}

#[tokio::test]
#[ignore = "emits deterministic default-policy falsification measurements"]
async fn relation_cache_default_policy_meets_targets() -> TestResult {
    let manifest = load_manifest()?;
    let spec = load_spec()?;
    let scenarios = all_scenarios(&manifest, &spec)?;
    let candidate = RelationCacheConfig::default();
    if candidate.policy.mode != RelationCachePolicyMode::Enforced
        || candidate.policy.reuse_admission != RelationCacheReuseAdmission::FamilyConversion
        || candidate.policy.prior != RelationCachePrior::None
    {
        return Err("relation cache defaults do not select enforced family conversion".into());
    }
    let mut baseline = candidate;
    baseline.policy.reuse_admission = RelationCacheReuseAdmission::SecondTouch;
    let mut results = Vec::new();
    for config in [baseline, candidate] {
        for scenario in &scenarios {
            results.push(run_scenario(&manifest, scenario, config).await?);
        }
    }
    let evaluation = evaluate(&spec, &results)?;
    print!("{}", evaluation.report);
    if let Ok(output) = std::env::var("RAD_RELATION_CACHE_BENCHMARK_OUTPUT") {
        let phase = std::env::var("RAD_RELATION_CACHE_BENCHMARK_PHASE")
            .map_err(|_| "RAD_RELATION_CACHE_BENCHMARK_PHASE is required with benchmark output")?;
        write_run(Path::new(&output), &phase, results)?;
    }
    if let Ok(output) = std::env::var("RAD_RELATION_CACHE_BENCHMARK_REPORT") {
        write_file(Path::new(&output), evaluation.report.as_bytes())?;
    }
    if !evaluation.failures.is_empty() {
        return Err(evaluation.failures.join("; ").into());
    }
    Ok(())
}
