# Access-path benchmark

This workload supplies controlled distributions for cardinality estimation and
scan-versus-index decisions. It complements the outside-in commerce and
threaded-posts workloads. It does not model an application.

The `events` table contains 1,000 deterministic rows. Category values are
skewed and follow primary-key order. Score values have a uniform repeated
range. Region and segment values have full correlation. Optional tags are 80
percent NULL. The queries select rare and common values, narrow and broad
ranges, correlated predicates, NULL values, and an ordered page.

The manifest, schema, query documents, and `access-paths-dataset-v1` generator
form the fixture identity. Do not change a distribution in place. A changed
distribution defines a new benchmark identity.
