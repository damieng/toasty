# Orders Schema: PostgreSQL vs MongoDB

This document describes the current relational schema used by the `example-orders` app
and proposes an equivalent MongoDB document schema.

---

## Current PostgreSQL schema

Two tables connected by a foreign key reference managed in application code (Toasty
does not emit `FOREIGN KEY` constraints). The `ShippingAddress` embedded struct is
flattened into prefixed columns on `orders`.

### `orders`

| Column | Type | Nullable |
|---|---|---|
| `id` | `uuid` | NO |
| `customer_id` | `uuid` | NO |
| `status` | `text` | NO |
| `shipping_address_street` | `text` | NO |
| `shipping_address_city` | `text` | NO |
| `shipping_address_state` | `text` | NO |
| `shipping_address_zip` | `text` | NO |
| `shipping_address_country` | `text` | NO |

**Indexes**
- `orders_pkey` — unique on `id`

### `order_details`

| Column | Type | Nullable |
|---|---|---|
| `id` | `uuid` | NO |
| `order_id` | `uuid` | NO |
| `product_id` | `uuid` | NO |
| `description` | `text` | NO |
| `quantity` | `integer` | NO |
| `unit_price_cents` | `bigint` | NO |

**Indexes**
- `order_details_pkey` — unique on `id`
- `index_order_details_by_order_id` — btree on `order_id`

Reading a single order with its line items requires a query on `orders` plus a
second query on `order_details` filtered by `order_id` (or a JOIN).

---

## Proposed MongoDB schema

### Design rationale

Order line items have no useful existence outside their parent order. Every access
pattern that reads an order also reads its items: display, total calculation, fulfilment
processing. There is no use case that queries items independently of orders.

This makes items a natural candidate for embedding. A single document fetch returns
everything needed to work with an order, with no secondary query.

The `shipping_address` that Toasty flattens to prefixed columns in PostgreSQL becomes
a proper nested sub-document in MongoDB — no flattening needed, and queries can target
individual address fields by path (e.g. `shipping_address.city`).

`order_details` becomes the `items` array inside the `orders` document. The
`order_details` collection is removed entirely.

### Collection: `orders`

```json
{
  "_id": "019eae2c-31ae-7981-92a5-7b6e659b8c2c",
  "customer_id": "b28a863f-95c1-4b6b-870e-0965981a78f7",
  "status": "shipped",
  "created_at": { "$date": "2025-06-09T10:30:00Z" },
  "updated_at": { "$date": "2025-06-09T14:22:00Z" },
  "shipping_address": {
    "street": "742 Evergreen Terrace",
    "city": "New York",
    "state": "NY",
    "zip": "10001",
    "country": "US"
  },
  "items": [
    {
      "product_id": "c1cdee9d-4b36-42c6-b540-7f5fecd5b2f2",
      "description": "Premium Widget (Blue)",
      "quantity": 2,
      "unit_price_cents": 1999
    },
    {
      "product_id": "f8760ff2-0556-4013-961d-ff605a985afb",
      "description": "Smart Gadget Pro",
      "quantity": 1,
      "unit_price_cents": 4999
    }
  ]
}
```

### Field notes

**`_id`** — kept as UUID string to match the existing application identifier. Alternatively
use `ObjectId` if the app does not require externally stable IDs; `ObjectId` embeds a
creation timestamp which removes the need for a separate `created_at` field.

**`shipping_address`** — stored as a nested sub-document. Queries can filter on
individual fields by path (`"shipping_address.city": "New York"`), which maps cleanly
to Toasty's embedded struct field accessors. No column-name flattening required.

**`items`** — replaces the `order_details` table. Each element carries `product_id`,
`description`, `quantity`, and `unit_price_cents`. Item-level `_id` fields are omitted
here; add them if the app needs to address individual line items by ID using
MongoDB's positional array operators.

**`created_at` / `updated_at`** — operational timestamps absent from the relational
schema. Useful for sorting recent orders, TTL indexes, and audit trails.

### Indexes

```js
// Customer order history — primary lookup pattern
db.orders.createIndex({ customer_id: 1, created_at: -1 })

// Status-based processing queues (fulfilment, pending review, etc.)
db.orders.createIndex({ status: 1, created_at: -1 })

// Ship-to city — useful for regional fulfilment queries
db.orders.createIndex({ "shipping_address.city": 1 })

// Product lookup across all orders
db.orders.createIndex({ "items.product_id": 1 })
```

The `order_id` index on `order_details` disappears: the relationship is resolved by
document structure, not a secondary index lookup.

---

## Comparison summary

| Concern | PostgreSQL | MongoDB |
|---|---|---|
| Collections / tables | 2 (`orders`, `order_details`) | 1 (`orders`) |
| Shipping address storage | 5 flattened columns (`shipping_address_*`) | nested sub-document |
| Read an order + items | 2 queries or 1 JOIN | 1 document fetch |
| Write a new order + items | 2 inserts (order, then N details) | 1 insert |
| Add a line item | `INSERT INTO order_details` | `$push` onto `items` array |
| Filter by address field | `WHERE shipping_address_city = ?` | `{ "shipping_address.city": ? }` |
| Filter by item field | `JOIN order_details WHERE product_id = ?` | `{ "items.product_id": ? }` |
| Schema enforcement | Column types + NOT NULL constraints | Application-level or JSON Schema validator |
