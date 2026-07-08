<!-- Source: https://www.cockroachlabs.com/blog/cockroachdb-on-rocksd/ — retrieved 2026-07-06 -->

# Why we built CockroachDB on top of RocksDB

By Arjun Narayan and Peter Mattis | Published January 17, 2019

> _In September of 2020 we introduced our own homecooked replacement for RocksDB — a storage engine called Pebble._

## Introduction

Most database students learn that log-structured merge trees (LSMs) suit write-heavy workloads while B-Trees work better for read-heavy scenarios. Yet most NewSQL databases use RocksDB, an LSM-based engine. This might suggest modern applications are write-heavy, but that assumption is incorrect.

The actual motivation behind RocksDB adoption stems from its rich feature set—necessary for complex distributed database products. At Cockroach Labs, the team depends on numerous RocksDB capabilities unavailable in competing storage engines regardless of their underlying data structure.

## What is a Storage Engine?

A storage engine manages disk persistence on a single node. In distributed databases, distribution, replication, and transaction coordination create additional engineering complexity. Rather than building a storage engine from scratch, Cockroach Labs chose to leverage a mature product.

A storage engine's primary responsibility involves **atomicity** and **durability**—the A and D in ACID. This frees higher database layers to focus on distributed coordination for strong **isolation** guarantees like serializability and **consistency** primitives ensuring data integrity.

Beyond ACID compliance, storage engines provide performance optimization. All engines implement write-ahead logs for rapid write durability, but the engineering details prove tricky. Committing writes while maintaining high concurrency requires delicate performance tradeoffs.

Storage engines must define isolation models supporting concurrent operations. While RocksDB provides snapshot isolation, CockroachDB adds bookkeeping for serializability. Maintaining such models at high performance remains complex—unlike Postgres, which integrates everything monolithically.

## What is RocksDB?

RocksDB is a single-node key-value storage engine based on log-structured merge trees. It evolved from Google's earlier LevelDB project, which drew inspiration from BigTable's low-level storage engine. RocksDB has become substantially more robust and feature-complete than its predecessor.

In RocksDB, keys and values exist as sorted strings in files called SSTables. These arrange into multiple levels where SSTables within a level don't overlap (one might cover keys `[a,b)`, another `[b,d)`), but overlap occurs between levels.

Finding a key like "aardvark" requires checking multiple SSTables. Each SSTable maintains internal sorting, enabling logarithmic lookup time. SSTables organize data into blocks with internal indexes, allowing efficient access even for gigabyte-sized files.

Levels structure so each is roughly 10x larger than the level above it. New keys arrive at the highest layer. As that level grows and hits thresholds, SSTables compact into fewer, larger SSTables one level lower. Compaction timing and methodology significantly affect performance—Leveled Compaction represents one strategy among many.

Above on-disk levels sits an in-memory memtable—a sorted structure (typically using concurrent skiplists) enabling cheap reads. This persists as an unsorted Write-Ahead-Log (WAL). Upon crashes, the durable WAL reconstructs the memtable. As the memtable grows with writes, it flushes to disk—a potential bottleneck during sustained writing.

To make memtable flushes efficient, L0 allows overlapping SSTables. However, this creates critical bottlenecks for compactions and reads—L0 usually compacts in large chunks into L1, and each L0 table increases read amplification.

Writes create deferred write amplification through eventual compactions pushing keys down the hierarchy. Bursty workloads accommodate numerous writes. Sustained writing requires dedicating IO bandwidth to concurrent compactions—a core motivation for RocksDB's multicore approach over LevelDB.

## Translating Higher Level SQL Operations into K and V Operations

CockroachDB translates SQL operations into key-value operations across a single logical keyspace. This keyspace shards into physical key ranges, each replicated across three or more nodes. SQL operations transform into sets of KV operations distributed across machines.

At each machine, KV operations execute against the underlying storage engine. While RocksDB provides a key-value interface, standardization masks subtle details. The interface extends beyond basic `put`, `get`, and `delete` operations:

- **Range scans** over intervals `[start, end)`
- **Range deletion** over `[start, end)`
- **Bulk ingestion** of keys and values

Performing these operations efficiently proves critical—otherwise some SQL operations become prohibitively slow.

## Fast Scans

A surprising engineering insight: scans occur more frequently than intuition suggests. While academic papers typically benchmark storage engines using put/get operations (like YCSB), CockroachDB's dominant operations are `put/scan` due to serializable SQL guarantees.

Multi-version concurrency control (MVCC) stores multiple values per key along with write timestamps. Transactions at particular timestamps read the latest value as of that point. What appears as a `GET` operation at the database level—like `SELECT * FROM tablename WHERE primarykeycol = 123`—becomes a storage-level `SCAN` seeking the newest value for that key. Each CockroachDB key receives a timestamp annotation creating equivalent RocksDB keys. Key updates create additional RocksDB keys.

MVCC introduces complications. Many key-value engines offer fast `GET` but slower `SCAN` operations. In any LSM implementation, a key might exist at any level, yielding read amplification factors equal to the level count. To reduce this logarithmic multiple, engines use bloom filters—probabilistic structures small enough for memory residence answering "is this key present?" with either "no" or "maybe."

Unfortunately, bloom filters work per-key. Scans involve potentially infinite keyspaces between endpoints, preventing bloom filters from ruling out levels. RocksDB elegantly solves this through **prefix bloom filters**—constructing filters on key prefixes. Scans over matching prefixes benefit from filter usage. Without this, engines scan every level on every logical `GET`—a severe performance penalty. Prefix bloom filters represent essential functionality for MVCC databases like CockroachDB.

## RocksDB Snapshots

CockroachDB's distributed replication occasionally requires bringing new nodes up-to-date with data copies. This involves scanning large keyspace chunks and transmitting them over the network—potentially taking seconds to tens of seconds depending on data size and network speed.

For prolonged operations, CockroachDB faces two options: scan over extended periods or complete the scan while holding data separately until transmission finishes. Most storage engines, including RocksDB, provide snapshots—scans operate over consistent database states where post-scan writes remain invisible. This storage-level isolation guarantees create useful building blocks for databases.

Yet providing snapshot functionality without excessive resource consumption proves challenging. Reading snapshots pins memtables during operations, slowing incoming writes. Naively pinning memtables costs significantly—the alternative involves reading snapshots to free the storage engine, then holding keys in higher-level memory until transmission completes.

The problem intensifies because CockroachDB shards the keyspace into many ranges—each up to 64MB. New nodes trigger numerous snapshot creation and transmission events across the cluster before reaching parity, all while normal operations continue.

RocksDB provides a middle ground: **explicit snapshots**. These don't pin memtables. Instead, they signal the storage engine not to perform compactions crossing the snapshot's time boundary. Holding explicit snapshots—even for hours—consumes no additional resources beyond preventing compactions that might improve storage efficiency. When iteration begins, implicit snapshots (potentially pinning memtables) activate, avoiding prolonged expensive snapshot holding.

## Blitzing Through More RocksDB Features

CockroachDB leverages numerous RocksDB features beyond those covered exhaustively here. Key capabilities include:

### SSTable Ingestion

During backup restoration, files containing keyspace portions guaranteed empty at ingestion begin are ingested. Using the normal write path wastes resources—the normal path writes to high LSM levels, then compacts downward with substantial write amplification. Pre-constructing SSTables at low levels guaranteed non-overlapping with existing SSTables avoids this. This greatly improves restore throughput and represents critical CockroachDB functionality.

### Custom Key Comparators

MVCC timestamp suffixes could encode lexicographically-equivalent comparisons, but such encoding is slower to encode and decode than non-lexicographic approaches. RocksDB enables custom comparators, allowing quick encoding/decoding while maintaining correct ordering—timestamps sort descending since latest versions sort first, enabling scan termination at first matches. Efficient custom comparators are essential CockroachDB features.

### Range Deletion Tombstones

Efficiently deleting entire key ranges is necessary. Operations like `DROP TABLE` or inter-node data transfers would otherwise block for extended periods. RocksDB achieves this through **range deletion tombstones**. When reading keys, range tombstones read at higher levels than concrete values mark those keys as deleted. Actual SSTable deletions occur during compaction.

### Backwards Iteration

Backwards iteration makes queries like `SELECT * FROM TABLE ORDER BY key DESC LIMIT 100` efficient even with ascending indexes. Some storage engines lack backwards iteration capability, making such queries inefficient. Backwards iteration always costs more than forward iteration without underlying data layout changes. RocksDB abstracts these complexities from higher system levels—though opportunities for performance improvements exist.

### Indexed Batches

Batches constitute atomic write operation units containing set, delete, or delete range operations. Standard batches are write-only. RocksDB supports **indexed batches** enabling batch reads. In distributed databases, some writes take longer awaiting remote operations, yet other same-transaction operations must read pending writes. Support for atomic batches with merged views of updates layered atop the full database becomes critical. RocksDB's indexed batch support simplifies this substantially.

### Encryption Support

For encryption-at-rest features, the team relies on RocksDB's modular encryption support for SSTables. This handles heavy encryption lifting, letting the team focus on key management, rotation, and user-facing encryption features.

## RocksDB at CockroachDB Today

RocksDB is deeply embedded in Cockroach's architecture. Alternative storage engines lacking the above features would require significant re-engineering and likely cause performance degradation. Raw key/value access speed improvements frequently disappear considering all these requirements.

However, RocksDB isn't without drawbacks. Substantial engineering effort achieves current performance. Additionally, having a performance-critical C++ codebase within a Go system creates complications. Crossing the CGo barrier involves approximately 70 nanoseconds overhead per call—fast in isolation but significant given RocksDB's call frequency.

The team minimizes CGo crossings by constructing entire RocksDB operation batches in Go, transferring them in single CGo calls for efficiency. Performance penalties arise from copying values from C-allocated memory to Go-allocated memory. A Go-native storage engine could provide performance benefits and codebase streamlining, though existing Go-native engines lack required functionality. Given choosing between implementing delicate performance-critical features versus engineering around CGo overhead minimization, the latter has proven manageable so far.

Notably, RocksDB includes unused features like column families, FIFO compaction, backups and checkpoints, persistent caches, and transactions. Future CockroachDB versions might find performance reasons for inclusion.
