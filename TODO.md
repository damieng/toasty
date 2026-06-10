# MongoDB Driver — TODO

Gap analysis for `crates/toasty-driver-mongodb`. The current scaffold runs the
`orders` example end-to-end (connect, `push_schema`, insert, `Scan`, `QueryPk`,
`GetByKey`, `FindPkByIndex`) but has correctness gaps and no automated tests.
`Capability::MONGODB` is currently a verbatim clone of DynamoDB's.

Grouped by priority, then by Query / Update / Test & Docs.

---

## Must have

### Query
- [ ] **Capability audit** — `Capability::MONGODB = { ..DYNAMODB }` inherits wrong
  flags. `native_starts_with: true` makes the planner emit `StartsWith`, which
  `filter.rs` has no arm for → `todo!()` panic on any `.starts_with()`.
  Reconcile `scan_supports_sort`, `bool_key_type`, `native_like`, and the
  temporal/decimal native flags against what the translator actually handles.
- [ ] **Filter completeness** — no panics for emitted exprs: `Not`, `Cast(bool)`,
  `StartsWith`/`Like` per the audited flags.
- [ ] **Pagination** — `Scan`/`QueryPk` reject any `limit`/`offset`/cursor; map to
  `.skip()`/`.limit()` and `_id`-keyset cursors.
- [ ] **Result ordering** — `exec_query_pk` ignores `op.order`; confirm engine
  re-sort behavior under `scan_supports_sort: false`, else add `.sort()`.
- [ ] **Composite primary keys** — `GetByKey` errors on >1 PK column (blocks the
  `composite-key` example).
- [ ] **Value decode correctness** — `u64` round-trips through `i64`, corrupting
  values above `i64::MAX`.

### Update
- [ ] **`UpdateByKey`** — no update path; needs `$set`/`$inc` mapping and OCC
  version-column handling.
- [ ] **`DeleteByKey`** — entirely missing.
- [ ] **Value encode correctness** — write side of the `u64 → i64` corruption; fix
  encode + decode together (or cap via `max_unsigned_integer`).

### Test & Docs
- [ ] **Integration suite wiring** — `tests/tests/mongodb.rs` (`Setup` with
  `driver()` + `delete_table()`), `mongodb` feature in `tests/Cargo.toml`,
  `generate_driver_tests!` block. Surfaces every gap above as a real test.

---

## Nice to have

### Query
- [ ] **Streaming reads** — `find()` buffers the whole result into a `Vec`; stream
  for large scans.
- [ ] **`count()` pushdown** via `countDocuments`.
- [ ] **Projection pushdown** — fetch only selected fields instead of whole docs.
- [ ] **Native BSON decode** — read UUID binary subtype, `Date`, `Decimal128`.

### Update
- [ ] **Map single-column PK → `_id`** — today the PK is a plain field + a separate
  unique index, alongside a redundant auto-generated `ObjectId _id`.
- [ ] **Native BSON encode** — store UUIDs/temporals/decimals natively instead of
  the inherited string encodings.
- [ ] **Index management** — compound-index key ordering, idempotent creation, drop
  the duplicate PK index.
- [ ] **Error classification** *(cross-cutting)* — map Mongo errors to
  `connection_lost` / `serialization_failure` for pool/retry semantics.

### Test & Docs
- [ ] **CI job** — mirror `test-dynamodb` in `ci.yml`: `docker run
  mongodb/mongodb-atlas-local`, `cargo test --features mongodb`, `cargo test -p
  toasty-driver-mongodb`.
- [ ] **Docs** — crate README, user-guide entry, and the `?directConnection=true`
  requirement for Atlas Local replica sets.

---

## Future possibilities

### Query
- [ ] **Array query operators** — `$elemMatch`, `$all`, array `$in` (+ design-doc
  `array_contains` flag).
- [ ] **Aggregation pipeline** — `$group`/`$unwind`/`$lookup`/`$project` and a query
  abstraction to drive it.
- [ ] **Atlas Search / Vector Search**.
- [ ] **`ObjectId` as a first-class key type**.

### Update
- [ ] **Embedded documents** *(headline; also touches Query)* —
  `#[has_many(embedded)]` as sub-document arrays and `#[derive(Embed)]` as nested
  sub-documents instead of flattened columns. The actual reason for a Mongo
  driver; everything above just makes it a better DynamoDB.
- [ ] **Array update operators** — `$push`/`$pull`/`$addToSet` (+ `array_push`/
  `array_pull` flags).
- [ ] **Multi-document transactions** — supported on replica sets (Atlas Local
  qualifies); maps to Toasty's future transaction API.
- [ ] **Migrations** — no DDL; an application-level data-migration story (e.g.
  field renames).
- [ ] **JSON Schema validation** (`collMod`) and **change streams**.
- [ ] **Cross-driver embedded portability** — same model file → table on SQL,
  embedded array on Mongo.

### Test & Docs
- [ ] Extend suite coverage for array/embedded behavior once those land.
