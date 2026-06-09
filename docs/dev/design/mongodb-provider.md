# MongoDB Driver

## Summary

Add a MongoDB driver to Toasty. MongoDB is the first database in the supported set
that is a true document store: records are JSON-like documents, collections have no
fixed schema, and the natural modeling pattern is embedding related data inside a
parent document rather than normalizing it into separate tables.

This driver would expose MongoDB's document capabilities rather than mapping them onto
Toasty's existing relational abstractions. The goal is not to make MongoDB behave like
PostgreSQL — it is to make MongoDB's strengths accessible through Toasty's model and
query layer.

## Motivation

Toasty's stated philosophy is that it does not aim to abstract the database:

> "Toasty leans into the target database's capabilities and aims to help the user
> avoid issuing inefficient queries for that database."

MongoDB is a widely-used database whose access patterns differ from both SQL and
DynamoDB in ways that affect how models and queries should be written. A MongoDB
driver that treats the database as a slow SQL engine would produce poor schemas and
inefficient queries. The value is in surfacing the document model idiomatically.

### What MongoDB is not

MongoDB is not DynamoDB. DynamoDB is a key-value store with limited secondary index
support and no schema. MongoDB is a document database where:

- Every record is a structured document (BSON), not a flat row.
- Related data is embedded as nested sub-documents or arrays rather than normalized
  into separate collections.
- The query language operates on document structure: field paths, array operators,
  sub-document matching, and an aggregation pipeline.
- Collections have no enforced column schema (though a JSON Schema validator can be
  attached). Types live in documents, not columns.

### The `has_many` mismatch

In the relational and DynamoDB models, a `#[has_many]` relation maps to a separate
table/collection with a foreign key. Fetching a parent and its children requires two
queries.

In MongoDB the idiomatic representation for a bounded one-to-many relationship where
the children are always accessed with the parent is an embedded array. A `has_many`
that resolves to a second `find()` against a `order_details` collection is the
MongoDB anti-pattern.

The ordering example from this branch illustrates the difference directly.

**Relational (current):** two tables, two queries.

```
orders          order_details
──────────      ─────────────────────────────────────────────────
id              id  order_id  product_id  quantity  unit_price_cents
customer_id
status
```

**MongoDB (proposed):** one collection, one document fetch.

```json
{
  "_id": "019eae21-26ec-7f12-be26-103400a64d34",
  "customer_id": "f7db7ba6-...",
  "status": "shipped",
  "created_at": { "$date": "2025-06-09T10:30:00Z" },
  "updated_at": { "$date": "2025-06-09T14:22:00Z" },
  "total_cents": 8997,
  "items": [
    { "product_id": "5c671b3f-...", "quantity": 2, "unit_price_cents": 1999 },
    { "product_id": "d23966cd-...", "quantity": 1, "unit_price_cents": 4999 }
  ]
}
```

## User-facing API

*This section is a sketch. Full API design follows once the embedding question is
resolved (see Open questions).*

### Connection

```rust
let mut db = toasty::Db::builder()
    .models(toasty::models!(crate::*))
    .connect("mongodb://localhost:27017/mydb")
    .await?;
```

### Model definition — embedded arrays

A new annotation, tentatively `#[embed]` or `#[embedded_in]`, marks a struct as a
sub-document embedded inside its parent rather than stored in its own collection.

```rust
#[derive(Debug, toasty::Model)]
struct Order {
    #[key]
    #[auto]
    id: uuid::Uuid,

    customer_id: uuid::Uuid,
    status: String,

    #[has_many(embedded)]   // items live inside the order document
    items: toasty::Deferred<Vec<OrderItem>>,
}

#[derive(Debug, toasty::Embed)]   // not a Model; no collection of its own
struct OrderItem {
    product_id: uuid::Uuid,
    quantity: i32,
    unit_price_cents: i64,
}
```

On a SQL or DynamoDB backend, `#[has_many(embedded)]` could fall back to the
normalized representation. On MongoDB it uses native sub-document arrays.

### Queries

For embedded sub-documents, predicates on the array elements translate to MongoDB
array query operators:

```rust
// Orders containing at least one item for a given product
Order::filter(|o| o.items().any(|i| i.product_id.eq(&pid)))
    .exec(&mut db)
    .await?;
```

This maps to `{ "items.product_id": { "$eq": pid } }` in the MongoDB query language.

## Behavior

*To be specified once the API section is settled.*

Key questions for behavior:

- What does `push_schema()` do? MongoDB collections are created implicitly on first
  write. `push_schema()` could be a no-op, or it could create the collection
  explicitly and attach an optional JSON Schema validator.
- What does `reset_db()` do? Drop the database, matching the DynamoDB behavior.
- How are embedded sub-documents updated? MongoDB `$push` / `$pull` / `$set` on
  array elements differ from row-level UPDATE semantics.

## Driver integration

### What exists today

Toasty's driver interface (`toasty-core/src/driver.rs`) is already split between SQL
operations (`QuerySql`, `Insert`) and key-value operations (`GetByKey`, `QueryPk`).
MongoDB fits neither cleanly: it speaks a document query language, not SQL, and its
access patterns go beyond primary-key lookups.

The driver would implement `Driver` + `Connection` and receive `Operation` variants.
New operation variants will be needed for document-level operations:

- `FindDocuments` — filter by field path predicates
- `InsertDocument` — insert a full document
- `UpdateDocument` — field-level updates including array operators
- `DeleteDocument` — delete by filter

### Capability flags

New flags in `Capability` needed for MongoDB:

| Flag | Meaning |
|---|---|
| `embedded_has_many` | `#[has_many(embedded)]` stores children inside the parent document |
| `array_contains` | filter by array element membership |
| `array_push` / `array_pull` | atomic array modification operators |
| `aggregation_pipeline` | full `$group`, `$project`, `$unwind` support (later phase) |

### Schema and migrations

MongoDB has no table schema. `push_schema()` is a no-op for plain collections.
Optional JSON Schema validation could be emitted as a `collMod` command if the user
opts in via an annotation (out of scope for first driver).

The `generate_migration()` / `apply_migration()` driver methods would return an empty
migration. Schema changes that affect MongoDB (e.g. renaming a field) require an
application-level data migration; there is no DDL equivalent.

## Alternatives considered

### Treat MongoDB like DynamoDB (separate collection per model, no embedding)

DynamoDB is already in Toasty and has its limitations documented. Mapping MongoDB onto
the same normalized model works mechanically but produces the MongoDB anti-pattern: a
`find()` on `orders` followed by a `find()` on `order_details` filtered by `order_id`
— exactly what embedding is intended to avoid. The main benefit of MongoDB over a SQL
database is lost.

### Make embedding work across all drivers

Embedding `#[has_many` children inside the parent is not specific to MongoDB.
PostgreSQL `JSONB` columns and DynamoDB nested attribute maps can store sub-documents.
A cross-driver embedding design is worth pursuing but is a larger change and should
not block the MongoDB driver. Start with MongoDB-native embedding; generalize later.

## Open questions

1. **`#[has_many(embedded)]` vs `#[derive(Embed)]`** — Should embedding be an
   annotation on the relation, on the child struct, or both? Blocking API design.

2. **Embedded sub-document updates** — MongoDB's positional operators (`$`,
   `$[identifier]`) are needed to update a single array element by a filter. How
   does this surface in Toasty's update builder? Blocking implementation.

3. **Cross-driver portability of embedded models** — If `OrderItem` is marked
   `#[derive(Embed)]`, can the same model file target PostgreSQL (where it becomes a
   separate table) and MongoDB (where it becomes an embedded array) without code
   changes? This is the core portability question. Deferrable.

4. **`_id` strategy** — MongoDB uses `ObjectId` by default. Toasty uses UUID. The
   driver should accept both; a `#[auto]` UUID key should work unchanged.
   Deferrable.

5. **Index creation** — `push_schema()` could create the indexes listed in the model
   annotations. Whether this is done eagerly or lazily, and how it interacts with
   existing indexes, needs deciding. Deferrable.

## Out of scope

- **Aggregation pipeline** — `$group`, `$unwind`, `$lookup` are powerful but require
  a new query abstraction. First driver ships without them.
- **Change streams** — real-time document change notifications; a separate feature.
- **Transactions** — MongoDB supports multi-document transactions since 4.0; mapping
  them onto Toasty's (not-yet-designed) transaction API is deferred.
- **JSON Schema validation** — attaching a validator to a collection is optional and
  operator-facing, not part of the core driver.
- **Atlas Search / Vector Search** — full-text and vector search are Atlas-specific
  features; out of scope for the open-source driver.
