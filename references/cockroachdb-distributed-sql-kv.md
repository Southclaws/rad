<!-- Source: https://www.cockroachlabs.com/blog/distributed-sql-key-value-store/ — retrieved 2026-07-06 -->
<!-- Note: condensed rendering of the article body; consult the source URL for the full original text and diagrams. -->

# The architecture of a distributed SQL database, part 1: Converting SQL to a KV store

CockroachDB converts SQL statements into key-value data distributed among nodes and written to disk. The bottom layer is a distributed, replicated, transactional key-value store that enables the SQL interface users interact with.

## Why Convert SQL to a KV Store?

CockroachDB is fundamentally "a distributed SQL database that's enabled by a distributed, replicated, transactional key value store." The KV layer facilitates:

- Efficient data distribution within the database
- Natural separation for distributed transactions
- Atomic columns that enable dynamic schema changes
- Extensibility for features like geo-partitioning

This architecture allows CockroachDB to "retain the efficiency of a KV store but gain the natural ability to distribute data, and still speak SQL."

## The KV Store Structure

In CockroachDB's internal architecture, table data becomes key-value pairs where keys and values are arbitrary strings. Keys determine sort order, while values contain the columns associated with that key.

The system uses a monolithic keyspace divided into 64-megabyte ranges. This size balances the ability to move ranges quickly while maintaining efficient indexing overhead. Ranges grow and shrink dynamically as data is added or removed.

### Multi-Version Concurrency Control

The system employs multi-version concurrency control, meaning "the keys and values are never updated in place." Instead, updates create newer values that shadow older versions, with tombstone values marking deletions. This approach provides snapshot consistency for transactions.

### Range Scans

Because keys are lexicographically ordered, the system enables efficient range scans. Querying keys between specific values requires scanning only the relevant ranges rather than traversing the entire dataset.

## Range Splitting: Automated Sharding

When a range reaches capacity, CockroachDB automatically splits it — a process involving creating a new replica, moving approximately half the data to it, and updating the indexing structure using the same distributed transaction mechanism.

This eliminates manual sharding requirements that traditional databases like PostgreSQL demand.

## Storage Engine

CockroachDB stores key-value data on Pebble, a local key-value store. Version 20.1 introduced Pebble as an alternative to RocksDB, and version 20.2 made it the default storage engine.
