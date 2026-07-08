<!-- Source: https://www.cockroachlabs.com/blog/sql-in-cockroachdb-mapping-table-data-to-key-value-storage/ — retrieved 2026-07-06 -->

# SQL in CockroachDB: Mapping table data to key-value storage

> **Outdated blog post alert!** CockroachDB no longer stores each non-primary-key column in a separate value by default. By default, all data in a table is stored in the same [column family](https://www.cockroachlabs.com/docs/stable/column-families).

---

## SQL? I thought CockroachDB was a key-value store?!?

In the past we described CockroachDB as a distributed, transactionally consistent, key-value store. We knew that a key-value API was not the endpoint we wanted to provide and a few months ago started work on a higher level structured data API that would support tables and indexes. Along with supporting such rich structures, we anticipated eventually supporting SQL for manipulating and accessing this structured data. After a bit of soul searching we embraced the inevitable and moved forward full-speed with SQL as the core of our structured data layer.

There are a lot of components to an SQL system: query parsing, query analysis, query planning, query execution, transactions, and persistent storage – to name a few. The CockroachDB SQL system is built on top of the internal CockroachDB key-value store and leverages the monolithic sorted key-value map to store all of the SQL table data and indexes. This post will focus on CockroachDB's mapping of SQL data to the key-value store and show how that mapping helps implement SQL functionality. Future posts will talk about query analysis, planning, and execution.

An SQL table is a set of rows where each row is a set of columns. Each column has an associated type (bool, int, float, string, bytes). A table also has associated indexes which allow for the efficient retrieval of a range of rows from a table. This sounds nothing at all like a key-value API where string keys map to string values. How do we map the SQL table data to KV storage?

First, a primer: CockroachDB's internal key-value API supports a number of operations, but we only need to know about a few of them for this post:

- `ConditionalPut(key, value, expected-value)` - Conditionally set the value at the specified key if the existing value matches the expected value.
- `Scan(start-key, end-key)` - Retrieve all of the keys between start-key (inclusive) and end-key (exclusive).

In CockroachDB, keys and values are both strings which can contain unrestricted byte values. OK, let's move on!

## Key Encoding

A foundational piece of the puzzle for mapping SQL table data to keys and values is encoding typed column data into strings. For example, given a tuple of values `<1, 2.3, "four">`, we would like to encode this to a string that conceptually looks like:

```
/1/2.3/"four"
```

We're using forward-slash as a visual separator between values, though this is only for readability purposes. We could dedicate an entire blog post to how these encodings work; in the interest of brevity, this post only discusses their properties, not their implementation. The encoded keys sort such that each field of the key is considered separately:

```
/1/2.3/"four"
/2/3.1/"six"
/10/4.6/"seven"
```

If you were to naively sort these strings you'd discover that `/10/…` sorts before `/2/…`. How the encoding works can appear a bit magical if you haven't encountered it before. If you're interested in the nitty-gritty details, see `{Encode,Decode}{Varint,Float,Bytes,Null}` in the [util/encoding](https://github.com/cockroachdb/cockroach/tree/master/util/encoding) package.

With this encoding tool at our disposal, we're ready to take a peek at the encoding of SQL table data. In CockroachDB, each table has a unique 64-bit integer ID assigned to it at creation. This table ID is used as the prefix for all keys associated with that table. Now let's consider the following table and data:

```sql
CREATE TABLE test (
  key INT PRIMARY KEY,
  floatVal FLOAT,
  stringVal STRING
)
INSERT INTO test VALUES (10, 4.5, "hello")
```

Every table in CockroachDB must have a primary key. The primary key is composed of one or more columns; in our `test` table above, it is composed of a single column. CockroachDB stores each non-primary key column under a separate key that is prefixed by the primary key and suffixed by the column name. The row `<10, 4.5, "hello">` would be stored in our `test` table as:

```
/test/10/floatVal -> 4.5
/test/10/stringVal -> "hello"
```

In this depiction, we're using `/test/` as a placeholder for the table ID and `/floatVal` and `/stringVal` as placeholders for the column ID (every column in a table has an ID that is unique within the table). Note that the primary key immediately follows the table ID in our encoding. This is the basis for index-scans in CockroachDB's SQL implementation.

If we were to look under the hood, we would see the table metadata:

```
/test/0/name -> "test"
/test/0/columns -> [key, floatVal, stringVal]
```

In numeric form, the key-value pairs for our table look like:

```
/1000/10/1 -> 4.5
/1000/10/2 -> "hello"
```

We'll stick with the symbolic form for keys for the rest of this post.

> Note: Common prefixes in keys don't waste storage because RocksDB, the underlying storage engine, eliminates almost all overhead via prefix compression.

Astute readers will note that storing key-value pairs for columns which are contained in the primary key is not necessary, as these columns' values are already encoded in the key itself. Indeed, CockroachDB elides these.

Notice that all of the keys for a particular row will be stored next to each other due to the primary key prefix (remember, key-value data is stored in a sorted monolithic map in CockroachDB, so this property is "free"). This allows retrieval of the values for a particular row using a Scan on the prefix. And this is exactly what CockroachDB does internally.

The query:

```sql
SELECT * FROM test WHERE key = 10
```

Will be translated into:

```
Scan(/test/10/, /test/10/Ω)
```

This scan will retrieve only the two keys for the row. The `Ω` represents the last possible key suffix. The query execution engine will then decode the keys to reconstruct the row.

## Null Column Values

There is a small twist to this story: column values can be `NULL` unless explicitly marked as `NOT NULL`. CockroachDB does not store `NULL` values and instead uses the absence of a key-value pair for a column to indicate `NULL`. The observant might notice a wrinkle here: if all of the non-primary key columns for a row are `NULL` we won't store any keys for the row. To address this case, CockroachDB always writes a sentinel key for each row that is the primary key without a column ID suffix. For our example row of `<10, 4.5, "hello">`, the sentinel key would be: `/test/10`. Huzzah!

## Secondary Indexes

So far we've ignored secondary indexes. Let's rectify that oversight:

```sql
CREATE INDEX foo ON test (stringVal)
```

This creates a secondary index on the column stringVal. We haven't declared the index as unique, so duplicate values are allowed. Similar to the rows for a table, we'll be storing all of the index data in keys prefixed by the table ID. But we want to separate the index data from the row data. We accomplish that by introducing an index ID which is unique for each index in the table, including the primary key index (sorry, we lied earlier!):

```
/tableID/indexID/indexColumns[/columnID]
```

The keys we used as examples above get slightly longer:

```
/test/1/10/floatVal -> 4.5
/test/1/10/stringVal -> "hello"
```

And now we'll also have a single key for the row for our index foo:

```
/test/2/"hello"/10
```

You might be wondering why we suffixed this encoding with the primary key value (`/10`). For a non-unique index like `foo`, this is necessary to allow the same value to occur in multiple rows. Since the primary key is by definition unique for the table, appending it as a suffix to a non-unique key results in a unique key. In general, for a non-unique index, CockroachDB appends the values of all columns which are contained in the primary key but not contained in the index in question.

Now let's see what happens if we insert `<4, NULL, "hello">` into our table:

```
/test/1/4/stringVal -> "hello"
/test/1/4
/test/2/"hello"/4
/test/2/"hello"/10
```

All of the table data together looks like:

```
/test/1/4
/test/1/4/stringVal -> "hello"
/test/1/10/floatVal -> 4.5
/test/1/10/stringVal -> "hello"
/test/2/"hello"/4
/test/2/"hello"/10
```

Secondary indexes are used during SELECT processing to scan a smaller set of keys. Consider:

```sql
SELECT key FROM test WHERE stringVal = "hello"
```

The query planner will notice there is an index on stringVal and translate this into:

```
Scan(/test/foo/"hello"/, /test/foo/"hello"/Ω)
```

Which will retrieve the keys:

```
/test/2/"hello"/4
/test/2/"hello"/10
```

Notice that these keys contain not only the index column stringVal, but also the primary key column key. CockroachDB will notice the primary key column key and avoid an unnecessary lookup of the full row.

Finally, let's look at how unique indexes are encoded. Instead of the index foo we created earlier, we'll create `uniqueFoo`:

```sql
CREATE UNIQUE INDEX uniqueFoo ON test (stringVal)
```

Unlike non-unique indexes, the key for a unique index is composed of only the columns that are part of the index. The value stored at the key is an encoding of all primary key columns that are not part of the index. The two rows in our test table would encode to:

```
/test/3/"hello" -> [4]
/test/3/"hello" -> [10]
```

We use `ConditionalPut` when writing the key in order to detect if the key already exists which indicates a violation of the uniqueness constraint.

So that's how CockroachDB maps SQL data to the key-value store in a nutshell. Keep an eye out for upcoming posts on query analysis, planning, and execution.

The idea of implementing SQL on top of a key-value store isn't unique to CockroachDB. This is essentially the design of MySQL on InnoDB, Sqlite4, and other databases, as well.
