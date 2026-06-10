# MongoDB Driver — Integration Test Failures

> Last updated: 2026-06-10  
> Branch: `worktree-mongodb-provider`  
> Atlas Local running on `localhost:27017` via `docker run mongodb/mongodb-atlas-local:latest`

Run the suite with:

```bash
cargo test -p tests --features mongodb --no-default-features -- --test-threads=1
```

Current baseline: **503 passing, 63 failing** (out of 566 in the suite).

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

### 4. Composite primary keys not supported (17 failures)

**Root cause:** `exec_get_by_key` in `crates/toasty-driver-mongodb/src/lib.rs` early-returns `unsupported_feature` when `pk_columns.len() != 1`:

```rust
if pk_columns.len() != 1 {
    return Err(Error::unsupported_feature(
        "composite primary keys are not yet supported by the MongoDB driver",
    ));
}
```

**Failing test modules:** `relation_has_many_composite_key`, `index_composite`, `relation_chain_composite_key`, `crud_composite_key_in_list`, `crud_composite_key_pagination`.

**MongoDB translation:** MongoDB has no native composite primary key. The convention used by the DynamoDB driver (which the MongoDB driver mirrors) is to concatenate the key columns or store them as a compound filter:

```js
// For composite key {kind: "A", name: "B"}:
db.collection.find({ kind: "A", name: "B" })
```

Build the filter by iterating `pk_columns` alongside the corresponding key values, adding one `$eq` clause per column. For `$in` style multi-key lookup, use `$or` across each key tuple.

---

### 5. Filter expressions

Each remaining expression type hits the catch-all `todo!` in
`crates/toasty-driver-mongodb/src/filter.rs`. Each is an independent `match`
arm in `translate_filter`, done as its own change:

| Expression | Failures | MongoDB equivalent | Status |
|---|---|---|---|
| `ExprNot` | 16 | `{ $nor: [inner] }` (top-level negation; `$not` is field-level only) | **done** |
| `ExprStartsWith` | 8 | `{ field: { $regex: "^prefix" } }` (escape regex metacharacters) | pending |
| `ExprBetween` | 6 | `{ field: { $gte: low, $lte: high } }` | pending |
| `ExprAnyOp` | 3 | `{ field: { $in: [...] } }` when op is `Eq`; otherwise `$elemMatch` | pending |

**AST shapes** (all in `crates/toasty-core/src/stmt/`):

```rust
ExprNot     { expr: Box<Expr> }
ExprStartsWith { expr: Box<Expr>, prefix: Box<Expr> }
ExprBetween { expr: Box<Expr>, low: Box<Expr>, high: Box<Expr> }
ExprAnyOp   { lhs: Box<Expr>, op: BinaryOp, rhs: Box<Expr> }  // rhs is array
```

Separate from these: `query_in_list::in_list_with_null` is an `InList` (not
`NOT IN`) case. `{ field: { $in: [v, null] } }` matches missing/null documents
in MongoDB, but SQL semantics treat a NULL in the list as matching nothing.
The `InList` arm should drop null operands from the `$in` array.

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

Cursor pagination over a composite primary key is still gated
(`unsupported_feature`); it lands with composite-key support. This is the
remaining cause for the `paginate_for_dynamodb` failures.

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

## Suggested implementation order

1. ~~**Primary-key `Value::Record` flattening**~~ — done; see category 1.
2. ~~**`DeleteByKey`**~~ — done; see category 3.
3. ~~**`UpdateByKey`**~~ — done; see category 2.
4. ~~**Pagination**~~ — done; see category 6.
5. **Filter expressions** — done/pending per type; see category 5. `ExprNot` done (16); `ExprStartsWith` (8), `ExprBetween` (6), `ExprAnyOp` (3) pending. Plus the `InList`-with-null fix (1).
6. **Composite primary keys** — ~26 tests. `pk_field` already flattens by key-column position, so it generalizes to composite keys; the remaining work is the multi-column filter construction in `exec_get_by_key` / `pk_in_filter` and keyset cursor pagination in `find_paginated`.
7. **Optimistic-lock conditions** — 14 tests (`update`/`delete` conditions); needs to distinguish a filtered-out row from a condition failure.
8. **Unique index on nullable columns** — 2–3 tests. `exec_insert` omits null fields, and a plain MongoDB unique index rejects a second missing/null value (`E11000`, on insert and update). SQL allows multiple NULLs; `push_schema` should create unique indexes on nullable columns as sparse (or partial on existence).
9. **Composite unique indexes** — 1 test. MongoDB supports them natively, so `push_schema` succeeds, but `composite_unique_index_unsupported_on_dynamodb` asserts a DynamoDB-specific `unsupported_feature` error. Needs a capability flag or a test gate rather than a driver change.
