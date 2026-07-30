#[path = "support/schema_scheduler_scenario.rs"]
mod scenario;

use std::time::{Duration, Instant};

use scenario::{CrashBoundary, Scenario, campaign_cases, run_case, write_campaign_summary};

#[test]
fn turmoil_restarts_schema_work_at_every_engine_boundary() -> Result<(), Box<dyn std::error::Error>>
{
    for (scenario, boundary) in campaign_cases() {
        for seed in 0..2 {
            run_case(seed, scenario, boundary).map_err(|error| {
                format!(
                    "DST case failed: seed={seed} scenario={} boundary={}: {error}",
                    scenario.name(),
                    boundary.name()
                )
            })?;
        }
    }
    Ok(())
}

#[test]
#[ignore = "long-running randomized DST soak; configure RAD_DST_SECONDS or RAD_DST_SEEDS"]
fn turmoil_schema_work_soak() -> Result<(), Box<dyn std::error::Error>> {
    let seconds = std::env::var("RAD_DST_SECONDS")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()?;
    let seeds = std::env::var("RAD_DST_SEEDS")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(if seconds.is_some() { u64::MAX - 4 } else { 64 });
    let seed_start = std::env::var("RAD_DST_SEED_START")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(4);
    let started = Instant::now();
    let deadline = seconds.map(Duration::from_secs);
    let mut completed = 0_u64;

    let mut scenario_counts = std::collections::BTreeMap::new();
    let cases = campaign_cases();
    'campaign: for seed in seed_start..seed_start.saturating_add(seeds) {
        for &(scenario, boundary) in &cases {
            if completed > 0 && deadline.is_some_and(|budget| started.elapsed() >= budget) {
                break 'campaign;
            }
            run_case(seed, scenario, boundary).map_err(|error| {
                format!(
                    "DST case failed: seed={seed} scenario={} boundary={}: {error}",
                    scenario.name(),
                    boundary.name()
                )
            })?;
            completed += 1;
            *scenario_counts
                .entry(scenario.name().to_owned())
                .or_insert(0) += 1;
        }
    }
    write_campaign_summary(
        started.elapsed(),
        completed,
        seconds,
        seeds,
        seed_start,
        &scenario_counts,
    )?;
    Ok(())
}

#[test]
#[ignore = "single-case replay; configure RAD_DST_SEED, RAD_DST_SCENARIO and RAD_DST_BOUNDARY"]
fn turmoil_schema_work_replay() -> Result<(), Box<dyn std::error::Error>> {
    let seed = std::env::var("RAD_DST_SEED")?.parse()?;
    let scenario = Scenario::parse(&std::env::var("RAD_DST_SCENARIO")?)?;
    let boundary = CrashBoundary::parse(&std::env::var("RAD_DST_BOUNDARY")?)?;
    if !scenario.boundaries().contains(&boundary) {
        return Err(format!(
            "boundary {:?} does not occur in scenario {:?}",
            boundary, scenario
        )
        .into());
    }
    run_case(seed, scenario, boundary)
}
