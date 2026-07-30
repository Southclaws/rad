# Threaded posts benchmark

This outside-in benchmark models a Reddit/Hacker News-style discussion forest
in one `posts` table. A root post has null `root_post_id` and
`reply_to_post_id`. Every reply stores both the root of its thread and its
immediate parent.

The deterministic dataset deliberately combines different recursion shapes:

- 12 balanced binary threads, each 6 reply levels deep;
- one 128-reply chain, stressing iteration depth and upward ancestry;
- one root with 1,024 direct replies, stressing a wide frontier.

The query programs use Rad recursive relations directly. They cover downward
thread traversal with computed depth, upward ancestry, a wide two-level
traversal, and a whole-forest recursive aggregate. All setup, writes, process
restart, and queries travel through the public HTTP API against S3/RustFS.

The manifest, schema, queries, and dataset generator version form the fixture
identity recorded with every result. Change them deliberately: results with
different fixture hashes are not comparable benchmark samples.
