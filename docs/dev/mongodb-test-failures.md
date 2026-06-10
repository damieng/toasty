# MongoDB Driver — Integration Test Failures

> Last updated: 2026-06-10  
> Branch: `worktree-mongodb-provider`  
> Atlas Local running on `localhost:27017` via `docker run mongodb/mongodb-atlas-local:latest`

Run the suite with:

```bash
cargo test -p tests --features mongodb --no-default-features -- --test-threads=1
```

Current baseline: **233 passing, 350 failing** (out of 566 total).

---

## Failure categories

### 1. `Value::Record` not serialized to BSON (176 failures)

**Root cause:** `crates/toasty-driver-mongodb/src/value.rs` `value_to_bson` hits the catch-all `todo!` for `Value::Record`. This fires even for simple scalar models because the query engine wraps certain values (primary key tuples, and embed struct fields) in a `ValueRecord` before handing them to the driver.

**Failing test modules (sample):** `type_primitives`, `type_collection`, `type_jiff`, `type_serialize`, `embed_struct`, `embed_struct_optional`, `deferred_embed`, `embed_enum_*`, `relation_*`, `crud_*`, `field_*`, `starts_with`.

**Files to change:**

- `crates/toasty-driver-mongodb/src/value.rs` — add `Value::Record` arms to `value_to_bson` and `from_bson`

**What to implement:**

`Value::Record` is a tuple of fields. In BSON the natural representation is an embedded `Document`:

```rust
stmt::Value::Record(record) => {
    // field names are not carried in ValueRecord; a caller that needs
    // named fields must zip with the schema columns.
    // For an unnamed tuple (key tuples, composite PK fragments) use array:
    Bson::Array(record.fields.iter().map(Self::value_to_bson).collect())
}
```

However, for **embed structs** the fields need to land in the *parent* document under prefixed column names (e.g. `shipping_address_street`). The schema mapping layer flattens embed structs to flat columns before the driver sees them, so the insert path receives individual scalar column values rather than nested Records. Verify this by adding a `dbg!` before the `todo!` to confirm the shape in each context.

If Records do appear as nested documents (e.g. from `GetByKey` or `QueryPk` results), they should round-trip through `Bson::Document`; use the column schema from the `ExprContext` to build/read field names.

**DynamoDB reference:** DynamoDB's `value.rs` (`crates/toasty-driver-dynamodb/src/value.rs`) also lacks a `Value::Record` arm but its tests pass because the DynamoDB planner never emits Records in the paths the suite exercises. The MongoDB driver's `exec_insert` is structurally identical but encounters Records — worth adding a failing DynamoDB unit test to confirm the difference before fixing MongoDB.

---

### 2. `UpdateByKey` not implemented (85 failures)

**Root cause:** `exec()` in `crates/toasty-driver-mongodb/src/lib.rs` returns `unsupported_feature` for `Operation::UpdateByKey`.

**Failing test modules:** `crud_update_macro`, `crud_update_arithmetic`, `crud_query`, `crud_query_macro`, `crud_create_macro`, `crud_basic`, `batch_update_delete`, `field_version`, `field_default_and_update`, `clone_query`, `relation_*`, `filter_*`.

**Operation shape** (`crates/toasty-core/src/driver/operation/update_by_key.rs`):

```rust
pub struct UpdateByKey {
    pub table: TableId,
    pub keys: Vec<stmt::Value>,       // PK values to match
    pub assignments: stmt::Assignments, // column → new value
    pub filter: Option<stmt::Expr>,   // optional post-key filter
    pub condition: Option<stmt::Expr>,// optimistic-lock condition
    pub returning: Option<Vec<ColumnId>>,
}
```

**MongoDB translation:**

```js
// keys → { _id: { $in: [key1, key2, ...] } }   (or the PK column name)
// assignments → { $set: { col: val, ... } }
db.collection.updateMany(keyFilter, { $set: assignments })
```

- Iterate `op.keys` to build a `$in` filter on the PK column (same as `exec_get_by_key`).
- Translate `op.assignments` into a `$set` document.
- If `op.filter` is set, AND it with the key filter using `and_documents`.
- `op.condition` (optimistic locking / version check) can be a follow-up; return `unsupported_feature` for now.
- `op.returning` — return the selected columns after update if set; can be a follow-up.

**DynamoDB reference:** `crates/toasty-driver-dynamodb/src/op/update_by_key.rs` — comprehensive, handles conditions and returning. Good structural reference but heavier than needed for a first pass.

---

### 3. `DeleteByKey` not implemented (17 failures)

**Root cause:** `exec()` returns `unsupported_feature` for `Operation::DeleteByKey`.

**Failing test modules:** `crud_basic`, `crud_query`, `batch_update_delete`, `relation_has_many_crud`, `relation_has_one_crud`.

**Operation shape** (`crates/toasty-core/src/driver/operation/delete_by_key.rs`):

```rust
pub struct DeleteByKey {
    pub table: TableId,
    pub keys: Vec<stmt::Value>,
    pub filter: Option<stmt::Expr>,
    pub condition: Option<stmt::Expr>,
}
```

**MongoDB translation:**

```js
db.collection.deleteMany({ pk_col: { $in: [key1, key2, ...] } })
```

- Same key-filter construction as `exec_get_by_key`.
- AND with `op.filter` if set.
- `op.condition` can return `unsupported_feature` for now.

**DynamoDB reference:** `crates/toasty-driver-dynamodb/src/op/delete_by_key.rs`.

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

1. **`Value::Record` in `value.rs`** — unblocks 176 tests including nearly all primitive and embed tests, and is a prerequisite for most other fixes.
2. **`UpdateByKey`** — 85 tests, needed for any test that mutates data after creation.
3. **`DeleteByKey`** — 17 tests, completes the basic CRUD surface.
4. **Filter expressions** (`ExprNot`, `ExprStartsWith`, `ExprBetween`, `ExprAnyOp`) — 27 tests, all isolated to `filter.rs`.
5. **Pagination** — 15 tests, small change to two `find()` call sites.
6. **Composite primary keys** — 17 tests, requires rethinking the key-filter construction in `exec_get_by_key` and related paths.
