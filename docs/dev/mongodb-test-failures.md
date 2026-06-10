# MongoDB Driver — Integration Test Failures

> Last updated: 2026-06-10  
> Branch: `worktree-mongodb-provider`  
> Atlas Local running on `localhost:27017` via `docker run mongodb/mongodb-atlas-local:latest`

Run the suite with:

```bash
cargo test -p tests --features mongodb --no-default-features -- --test-threads=1
```

Current baseline: **475 passing, 91 failing** (out of 566 in the suite).

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

### 5. Filter expressions not implemented (27 failures)

Three expression types hit the catch-all `todo!` in `crates/toasty-driver-mongodb/src/filter.rs`:

| Expression | Failures | MongoDB equivalent |
|---|---|---|
| `ExprNot` | 16 | `{ $nor: [inner] }` or `{ field: { $not: ... } }` |
| `ExprStartsWith` | 8 | `{ field: { $regex: "^prefix" } }` |
| `ExprBetween` | 6 | `{ field: { $gte: low, $lte: high } }` |
| `ExprAnyOp` | 3 | `{ field: { $in: [...] } }` (when op is `Eq`) |

**AST shapes** (all in `crates/toasty-core/src/stmt/`):

```rust
ExprNot     { expr: Box<Expr> }
ExprStartsWith { expr: Box<Expr>, prefix: Box<Expr> }
ExprBetween { expr: Box<Expr>, low: Box<Expr>, high: Box<Expr> }
ExprAnyOp   { lhs: Box<Expr>, op: BinaryOp, rhs: Box<Expr> }  // rhs is array
```

Add arms to `translate_filter` in `filter.rs` for each. `ExprAnyOp` with `BinaryOp::Eq` is `$in` (same as `ExprInList`); other operators map to `$elemMatch`.

---

### 6. Pagination (limit/offset) not implemented (15 failures)

**Root cause:** Both `exec_scan` and `exec_query_pk` return `unsupported_feature` when `op.limit.is_some()`.

**Failing test modules:** `crud_query`, `crud_query_macro`, `starts_with`, `filter_between`.

**MongoDB translation:** use `.limit(n)` and `.skip(offset)` on the cursor:

```rust
let mut find = self.collection(collection).find(query);
if let Some(limit) = op.limit {
    find = find.limit(limit as i64);
}
if let Some(offset) = op.offset {
    find = find.skip(offset as u64);
}
```

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
4. **Filter expressions** (`ExprNot`, `ExprStartsWith`, `ExprBetween`, `ExprAnyOp`) — 33 tests, all isolated to `filter.rs`.
5. **Composite primary keys** — 22 tests. `pk_field` already flattens by key-column position, so it generalizes to composite keys; the remaining work is the multi-column filter construction in `exec_get_by_key` and `pk_in_filter`.
6. **Pagination** — 15 tests, small change to the `find()` call sites.
7. **Optimistic-lock conditions** — 14 tests (`update`/`delete` conditions); needs to distinguish a filtered-out row from a condition failure.
8. **Unique index on nullable columns** — 2 tests. `exec_insert` omits null fields, and a plain MongoDB unique index rejects a second missing/null value (`E11000`). SQL allows multiple NULLs; `push_schema` should create unique indexes on nullable columns as sparse (or partial on existence).
