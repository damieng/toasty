# MongoDB Driver — Integration Test Failures

> Last updated: 2026-06-10  
> Branch: `worktree-mongodb-provider`  
> Atlas Local running on `localhost:27017` via `docker run mongodb/mongodb-atlas-local:latest`

Run the suite with:

```bash
cargo test -p tests --features mongodb --no-default-features -- --test-threads=1
```

Current baseline: **563 passing, 3 failing** (out of 566 in the suite). The 3
remaining are DynamoDB-specific test expectations that do not apply to MongoDB;
see category 10.

---

## Failure categories

### 1. Primary-key `Value::Record` not flattened (FIXED)

**Symptom:** `crates/toasty-driver-mongodb/src/value.rs` `value_to_bson` hit the catch-all `todo!` for `Value::Record`, even for simple scalar models.

**Root cause:** Toasty represents every primary key as a `Value::Record` — one field per key column, even for a single-column key. On insert, `exec_insert` passed the primary-key column's value (a `Record`) straight to the scalar-only `value_to_bson`, which has no `Record` arm. `exec_get_by_key` had the same gap when building the key filter.

The DynamoDB driver does not hit this because `ddb_key` (`crates/toasty-driver-dynamodb/src/lib.rs`) flattens the key `Record` into per-column scalars before calling its scalar `to_ddb`. The MongoDB driver had no equivalent. (The two drivers share a capability — `Capability::MONGODB = { ..DYNAMODB }` — so the planner emits the same operations for both; an earlier note claiming DynamoDB never emits these Records was wrong.)

**Fix:** `pk_field` in `crates/toasty-driver-mongodb/src/lib.rs` extracts the field of a key `Record` for a given key column, mirroring `ddb_key`. `exec_insert` calls it for primary-key columns; `exec_get_by_key` calls it when building the `$in` filter. `value_to_bson` stays scalar-only, so a stray non-key `Record` still fails loudly rather than being silently mis-encoded as a BSON array (which would break the read path: `document_to_record` decodes each column with `from_bson` against its scalar type).

This change took the suite from 233 to 327 passing. Most of the remaining failures in modules listed under categories 2–6 are tests that insert a row (now working) and then update, delete, paginate, or filter it.

---

### 2. `UpdateByKey` (FIXED)

`exec_update_by_key` builds the key filter with `pk_in_filter`, ANDs in
`op.filter` when present, and translates `op.assignments` into a MongoDB update
document via `build_update_doc`: `Set` → `$set` (or `$unset` for null), `Add`
→ `$inc`, `Subtract` → `$inc` with the value negated, `Append` → `$push` with
`$each`.

- `op.returning` is `None`: one `update_many`; return `matched_count`.
- `op.returning` is `Some`: `update_many` cannot return documents, and
  re-querying after the update would miss rows whose updated columns no longer
  match `op.filter`. Each key is updated with `find_one_and_update` returning
  the post-update document (`ReturnDocument::After`), which also gives correct
  values for `$inc` assignments.

`op.condition` (optimistic-lock / version check) returns `unsupported_feature`;
distinguishing "row missing / filtered out" from "row present but condition
failed" needs more than MongoDB's update operators surface. The version *bump*
is an ordinary `Add` assignment and works; only the check is deferred. This is
the remaining cause for the `field_version` / `field_default_and_update`
failures.

---

### 3. `DeleteByKey` (FIXED)

`exec_delete_by_key` builds the key filter with the shared `pk_in_filter`
helper (`{ pk_col: { $in: [...] } }`), ANDs in `op.filter` when present, and
runs `delete_many`, returning the deleted count. MongoDB enforces unique
indexes natively, so no secondary-index maintenance is needed (unlike the
DynamoDB driver).

`op.condition` (optimistic-lock version check) still returns
`unsupported_feature`; one test depends on it and lands with update/version
support. Composite primary keys remain gated; see category 4.

---

### 4. Composite primary keys (FIXED)

MongoDB has no native composite primary key; each key column is a separate
field, and the key value is a `Value::Record` flattened per column by
`pk_field`.

- **Key lookup** (`pk_in_filter`, used by `exec_get_by_key` / `exec_delete_by_key`
  / `exec_update_by_key`): a single-column key stays `{ pk_col: { $in: [...] } }`;
  a composite key becomes `{ $or: [ { c1: v1, c2: v2 }, ... ] }` — one equality
  document per key tuple, collapsing to the bare document for a single key.
- **Cursor pagination** (`keyset_after` in `find_paginated`): resumes strictly
  after the previous page's last key with the lexicographic predicate
  `{ $or: [ { c1: <cmp> }, { c1: a1, c2: <cmp> }, ... ] }`, where `<cmp>` is
  `$gt` ascending / `$lt` descending. The cursor (`key_cursor`) carries the full
  key as a `Value::Record`. A fixed partition key in the `pk_filter` makes the
  partition branch of the `$or` collapse, leaving the sort-key keyset.

---

### 5. Filter expressions (FIXED)

Each expression type is an independent `match` arm in `translate_filter`
(`crates/toasty-driver-mongodb/src/filter.rs`):

| Expression | MongoDB equivalent |
|---|---|
| `ExprNot` | `{ $nor: [inner] }` (top-level negation; `$not` is field-level only) |
| `ExprStartsWith` | `{ field: { $regex: "^prefix" } }`, prefix escaped via `regex_escape` |
| `ExprBetween` | `{ field: { $gte: low, $lte: high } }` |
| `ExprAnyOp` (`= ANY`) | `value = ANY(arrayColumn)` → `{ field: value }` (array membership); `scalarColumn = ANY(literalArray)` → `$in` |

`contains` / `intersects` / `is_superset` on a `Vec` field all decompose into
`value = ANY(arrayColumn)` clauses combined by `$or` / `$and`.

The `InList` arm also drops null operands from `$in`: a NULL in a SQL `IN`
list matches nothing, whereas `{ field: { $in: [v, null] } }` would match
missing/null documents.

`LEN(array) <op> n` (`type_collection::vec_string_len_filter`) arrives as
`ExprBinaryOp` with an `ExprLength` operand: equality maps to
`{ field: { $size: n } }`, other comparisons to
`{ $expr: { <op>: [ { $size: "$field" }, n ] } }`.

---

### 6. Pagination (FIXED)

`find_paginated` handles both `Pagination` variants for `exec_scan` and
`exec_query_pk`:

- `Offset { limit, offset }` → `.limit(n)` and `.skip(offset)` on the cursor.
- `Cursor { page_size, after }` → keyset pagination ordered by the primary
  key, resuming with `{ pk: { $gt: after } }`, returning the last row's key as
  `next_cursor` (or `None` once a short page is reached).

`QueryPk.order` sorts by the primary key in that direction. Non-key ordering
(e.g. `order_by(age().desc())`) is applied by the engine before the limit is
pushed down, so it never reaches the driver.

Cursor pagination over a composite primary key uses a lexicographic keyset
bound; see category 4.

---

## Key files

| File | Role |
|---|---|
| `crates/toasty-driver-mongodb/src/lib.rs` | Driver entry point, `exec()` dispatch, `exec_insert`, `exec_scan`, `exec_query_pk`, `exec_get_by_key`, `exec_find_pk_by_index` |
| `crates/toasty-driver-mongodb/src/value.rs` | `Value ↔ Bson` conversions |
| `crates/toasty-driver-mongodb/src/filter.rs` | Filter expression → MongoDB query document translation |
| `crates/toasty-driver-dynamodb/src/op/` | DynamoDB implementations of the same operations — useful reference for operation shapes |
| `crates/toasty-core/src/driver/operation/` | Operation structs sent from the query engine to drivers |
| `crates/toasty-core/src/stmt/expr_*.rs` | AST definitions for filter expressions |
| `tests/tests/mongodb.rs` | Test entry point wiring the suite to the MongoDB driver |

### 7. Optimistic-lock conditions (FIXED)

`op.condition` (e.g. a `#[version]` check) must error on a present row that
fails it, not silently skip it. `exec_update_by_key` / `exec_delete_by_key`
apply the conditioned write, then compare its matched/deleted count against the
number of rows the key/filter selects; a shortfall returns `condition_failed`.
The update returning path uses a per-key `find_one_and_update`, falling back to
a key/filter count on a miss to tell a stale lock from an absent row.

---

### 8. Array length filter (FIXED)

`LEN(array) <op> n` — equality uses `{ field: { $size: n } }`, other
comparisons `{ $expr: { <op>: [ { $size: "$field" }, n ] } }`. See category 5.

---

### 9. Unique index on nullable columns (FIXED)

`push_schema` creates a unique index on a nullable column as sparse, so a
second document with the field absent (how `exec_insert` stores null) is
allowed, matching SQL's "multiple NULLs". See `push_schema`.

---

### 10. DynamoDB-specific test expectations (won't fix in the driver)

Three suite tests gated `requires(not(sql))` assert DynamoDB-specific errors
that do not apply to MongoDB, which handles the input natively. These are not
driver bugs; resolving them needs a capability flag or test gate, not a driver
change:

- `starts_with::starts_with_empty_prefix` — DynamoDB's `begins_with` rejects an
  empty prefix; MongoDB matches all rows (like SQL `LIKE '%'`).
- `index_composite::composite_unique_index_unsupported_on_dynamodb` — MongoDB
  supports composite unique indexes natively, so `setup_db` succeeds.
- `index_composite::composite_index_too_many_range_columns` — DynamoDB limits a
  key index to 4 range columns; MongoDB has no such limit, so `setup_db`
  succeeds.

---

## Suggested implementation order

1. ~~**Primary-key `Value::Record` flattening**~~ — done; see category 1.
2. ~~**`DeleteByKey`**~~ — done; see category 3.
3. ~~**`UpdateByKey`**~~ — done; see category 2.
4. ~~**Pagination**~~ — done; see category 6.
5. ~~**Filter expressions**~~ — done (`ExprNot`, `ExprStartsWith`, `ExprBetween`, `ExprAnyOp`, `InList`-with-null); see category 5.
6. ~~**Composite primary keys**~~ — done; see category 4.
7. ~~**Optimistic-lock conditions**~~ — done; see category 7.
8. ~~**Array length filter**~~ — done; see category 8.
9. ~~**Unique index on nullable columns**~~ — done; see category 9.
10. **DynamoDB-specific test expectations** — 3 tests; not driver bugs. See category 10.
