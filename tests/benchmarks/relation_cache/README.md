# Relation cache benchmark

The benchmark uses fixed operation schedules. It does not use elapsed time as a result condition.

Record a phase:

```sh
task benchmark:relation-cache:record PHASE=phase-name
```

Compare two phases:

```sh
task benchmark:relation-cache:compare BASELINE=phase-a CANDIDATE=phase-b
```

Compare the reuse admission rules:

```sh
task benchmark:relation-cache:reuse-admission
```

Run the pairwise policy falsification corpus:

```sh
task benchmark:relation-cache:falsification
```

Record and verify the default policy:

```sh
task benchmark:relation-cache:default
```

The falsification corpus changes one workload factor independently of the other factors. It covers conditional reuse, future reuse count, result shape, cohort count, evidence age, request order, and cache pressure. A deterministic greedy generator covers every pair of factor levels.

The corpus applies these required targets:

- Wasteful admissions are at most 3% of useful admissions.
- Total useful avoided work is at least 99% of the second-touch baseline.
- Each useful workload retains at least 95% of baseline avoided work.
- The valuable set under cache pollution retains at least 99% of baseline avoided work.

A protected workload can select one materialization domain. The `stable_hash_build` workload protects hash-build work because the root query depends on the changing fact relation.

The result file and summary report are in `tests/benchmarks/relation_cache/runs`.

The record contains the benchmark manifest hash, the source revision, a source input hash, all configurations, and all scenario results. The comparison rejects different manifests, result sets, or correctness hashes.

The value fields use deterministic work units:

- `oracle_value` is a first-fill upper bound for positive residency value in the observed cohort.
- `actual_residency_value` is resident avoided work minus restore work and actual admission cost.
- `coalescing_value` is avoided fill work from concurrent request coalescing. It is separate from residency value.
- `policy_regret` is `oracle_value - actual_residency_value`.

Each result includes totals and separate `query`, `hash_build`, and `grouped_dimension` values. Decision counts identify each admission or rejection reason.

The `reuse_admission` field identifies the cold-start reuse rule. The rule can require a second request, a third request, enough current-cohort value to cover one admission cost, or a positive family conversion value.

The family conversion rule estimates the probability that a second touch becomes a third touch. It compares the expected saved work with retained size. It uses second-touch admission until the family observation minimum is reached. This model follows the conditional reuse probability and size relation in Beckmann, Chen, and Cidon, "LHD: Improving Cache Hit Rate by Maximizing Hit Density," NSDI 2018, Section 3.1.

The pollution workloads follow the admission problems in Einziger, Friedman, and Manes, "TinyLFU: A Highly Efficient Cache Admission Policy," 2015, Sections 3 and 4.

The decision-accounting fields classify each fill decision by the next non-coalesced request. `admissions_without_future_reuse` measures admitted fills with no later reuse. `rejections_followed_by_reuse` measures rejected fills that receive another request. `avoidable_work_after_rejection` measures the net work for those later requests.

The `recovery_probe` reason admits a candidate when reuse in the current cohort has already produced enough missed value to cover one residency cost. It lets current demand override stale negative history without weakening first-touch rejection.

The `recent_no_reuse` reason rejects a candidate when the most recently completed cohort had no reuse. Current-cohort value can end this cool-down through `recovery_probe`.
