# Commerce S3/HTTP benchmark

This is Rad's stable outside-in planning and storage benchmark. The runner:

1. starts RustFS and the real `rad serve` process;
2. migrates `schema.yaml` through the HTTP schema API;
3. generates and writes the row counts in `benchmark.yaml` through `/execute`;
4. shuts down cleanly, including SlateDB's SST flush, and reopens the store;
5. runs every JSON query through `/execute`; and
6. records release-mode latency, throughput and S3 traffic as versioned JSON.

Run it explicitly with `task benchmark:s3`. It requires Docker. Ordinary test
runs only validate the fixture and do not execute performance measurements.

The dataset is deterministic (`commerce-dataset-v1` in the runner): customers,
products, orders and line items use stable formatted identifiers and modular
distributions. Do not tune the fixture in place to improve a number. Changes to
the schema, formulas, row counts or queries define a new benchmark identity and
make historical timings incomparable.

`network_write_amplification` is S3 upload traffic divided by serialized logical
row bytes. It is useful now, but includes HTTP and metadata overhead. Future KV
and SlateDB tracing will split it into WAL, manifest, SST and compaction costs.
