# Storyden library benchmark

This benchmark uses SQL from Storyden's library implementation. The source
queries are in these files:

- `app/resources/library/node_querier/node_querier.go`
- `app/resources/library/node_querier/child_sort.go`
- `app/resources/library/node_traversal/db.go`

The benchmark changes `?` parameters to PostgreSQL `$n` parameters. It fills
the property sort ID list with deterministic fixture IDs in the same way that
Storyden fills this list.

Run the release benchmark with this command:

```sh
cargo test --release --locked --test storyden_library_benchmark -- --ignored --nocapture
```

The benchmark measures PostgreSQL prepare, prepared execute, and complete
prepare-plus-execute requests. It uses different fixture roots for the execute
and complete request phases. This prevents a result cache hit from hiding the
execution cost.

Use these environment variables to control a run:

- `RAD_STORYDEN_WARMUP`: Warm-up request count. The default is 8.
- `RAD_STORYDEN_ITERATIONS`: Measured request count. The default is 16.
- `RAD_STORYDEN_QUERY`: `all`, `node_properties`, `property_sort`, or `subtree`.
- `RAD_BENCHMARK_ARTIFACT_DIR`: Optional JSON output directory.

Use Samply to profile one query:

```sh
RAD_STORYDEN_QUERY=subtree RAD_STORYDEN_ITERATIONS=500 \
  samply record --save-only --output storyden-subtree.json.gz \
  cargo test --release --locked --test storyden_library_benchmark -- --ignored --nocapture
```
