#![warn(missing_docs)]

//! Toasty driver for [MongoDB](https://www.mongodb.com/) using the official
//! [`mongodb`](https://docs.rs/mongodb) crate.
//!
//! # Scope
//!
//! This is the first cut of the driver. It treats MongoDB as a
//! document-per-collection store — the same shape the DynamoDB driver fills —
//! so that the relational `examples/orders` project can run unchanged against
//! MongoDB by switching its connection URL. Each model maps to a collection,
//! and `#[has_many]` relations resolve to a second query against the child
//! collection.
//!
//! The idiomatic MongoDB representation (embedding child documents inside their
//! parent via `#[has_many(embedded)]`) is described in
//! `docs/dev/design/mongodb-provider.md` and is intentionally **not** part of
//! this scaffold.
//!
//! Implemented operations: [`push_schema`](Connection::push_schema), insert,
//! [`Scan`](operation::Scan), [`QueryPk`](operation::QueryPk),
//! [`GetByKey`](operation::GetByKey),
//! [`UpdateByKey`](operation::UpdateByKey),
//! [`DeleteByKey`](operation::DeleteByKey), and
//! [`FindPkByIndex`](operation::FindPkByIndex). Transactions and migrations
//! are not yet supported.

mod filter;
mod value;

pub(crate) use value::Value;

use async_trait::async_trait;
use mongodb::{
    Client, Collection, Database, IndexModel,
    bson::{Bson, Document},
    options::{IndexOptions, ReturnDocument},
};
use std::{borrow::Cow, sync::Arc};
use toasty_core::{
    Error, Result, Schema,
    driver::{Capability, Driver, ExecResponse, Rows, operation, operation::Operation},
    schema::{
        db::{self, Column},
        diff,
    },
    stmt::{self, ExprContext},
};
use url::Url;

/// A MongoDB [`Driver`] backed by the official `mongodb` crate.
///
/// Construct one with [`MongoDb::new`], passing a `mongodb://` connection URL.
/// The database name is taken from the URL's path (e.g. `orders` in
/// `mongodb://localhost:27017/orders`).
#[derive(Debug, Clone)]
pub struct MongoDb {
    url: String,
    client: Client,
    db_name: String,
}

impl MongoDb {
    /// Connect to MongoDB using the given `mongodb://` URL.
    pub async fn new(url: String) -> Result<Self> {
        let client = Client::with_uri_str(&url)
            .await
            .map_err(Error::driver_operation_failed)?;

        // Prefer the database named in the URL; fall back to parsing the path,
        // then to a sensible default.
        let db_name = client
            .default_database()
            .map(|db| db.name().to_string())
            .or_else(|| {
                Url::parse(&url).ok().and_then(|parsed| {
                    let path = parsed.path().trim_start_matches('/');
                    (!path.is_empty()).then(|| path.to_string())
                })
            })
            .unwrap_or_else(|| "test".to_string());

        Ok(Self {
            url,
            client,
            db_name,
        })
    }

    /// Create a driver from an already-initialized [`Client`].
    ///
    /// Useful in tests where the client must be kept alive on a dedicated
    /// runtime to prevent cancellation of MongoDB's background SDAM tasks.
    pub fn with_client(url: String, client: Client, db_name: String) -> Self {
        Self {
            url,
            client,
            db_name,
        }
    }

    fn database(&self) -> Database {
        self.client.database(&self.db_name)
    }
}

#[async_trait]
impl Driver for MongoDb {
    fn url(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.url)
    }

    fn capability(&self) -> &'static Capability {
        &Capability::MONGODB
    }

    async fn connect(&self) -> Result<Box<dyn toasty_core::driver::Connection>> {
        // `Database` (and the `Client` it holds) is cheap to clone; the driver
        // manages its own connection pool internally.
        Ok(Box::new(Connection {
            database: self.database(),
        }))
    }

    fn generate_migration(&self, _schema_diff: &diff::Schema<'_>) -> db::Migration {
        unimplemented!(
            "MongoDB migrations are not yet supported. MongoDB has no DDL; schema \
             changes are application-level data migrations."
        )
    }

    async fn reset_db(&self) -> Result<()> {
        self.database()
            .drop()
            .await
            .map_err(Error::driver_operation_failed)
    }
}

/// An open connection to a MongoDB database.
#[derive(Debug)]
pub struct Connection {
    database: Database,
}

impl Connection {
    fn collection(&self, name: &str) -> Collection<Document> {
        self.database.collection::<Document>(name)
    }
}

#[async_trait]
impl toasty_core::driver::Connection for Connection {
    async fn exec(&mut self, schema: &Arc<Schema>, op: Operation) -> Result<ExecResponse> {
        match op {
            Operation::QuerySql(op) => match op.stmt {
                stmt::Statement::Insert(insert) => self.exec_insert(&schema.db, insert).await,
                other => Err(Error::unsupported_feature(format!(
                    "the MongoDB driver does not support this statement: {other:#?}"
                ))),
            },
            Operation::Insert(op) => match op.stmt {
                stmt::Statement::Insert(insert) => self.exec_insert(&schema.db, insert).await,
                other => Err(Error::unsupported_feature(format!(
                    "the MongoDB driver does not support this statement: {other:#?}"
                ))),
            },
            Operation::Scan(op) => self.exec_scan(schema, op).await,
            Operation::QueryPk(op) => self.exec_query_pk(schema, op).await,
            Operation::GetByKey(op) => self.exec_get_by_key(schema, op).await,
            Operation::FindPkByIndex(op) => self.exec_find_pk_by_index(schema, op).await,
            Operation::UpdateByKey(op) => self.exec_update_by_key(schema, op).await,
            Operation::DeleteByKey(op) => self.exec_delete_by_key(schema, op).await,
            Operation::RawSql(_) => Err(Error::unsupported_feature(
                "raw SQL is only supported by SQL drivers",
            )),
            Operation::Transaction(_) => Err(Error::unsupported_feature(
                "transactions are not yet supported by the MongoDB driver",
            )),
        }
    }

    async fn push_schema(&mut self, schema: &Schema) -> Result<()> {
        for table in &schema.db.tables {
            tracing::debug!(collection = %table.name, "creating collection");

            // Creating the collection is optional in MongoDB (it is created
            // implicitly on first write), but doing it explicitly lets index
            // creation succeed up front. Ignore "already exists" on re-runs.
            if let Err(e) = self.database.create_collection(&table.name).await {
                let msg = e.to_string();
                if !msg.contains("already exists") && !msg.contains("NamespaceExists") {
                    return Err(Error::driver_operation_failed(e));
                }
            }

            let collection = self.collection(&table.name);

            for index in &table.indices {
                let mut keys = Document::new();
                for index_column in &index.columns {
                    let column = table.column(index_column.column);
                    keys.insert(column.name.clone(), 1_i32);
                }

                let unique = index.unique || index.primary_key;

                // `exec_insert` omits null fields, so a nullable column is
                // absent when null. A plain unique index would reject a second
                // such document (it indexes a missing field as null); a sparse
                // index skips them, matching SQL's "multiple NULLs allowed".
                let nullable = index
                    .columns
                    .iter()
                    .any(|index_column| table.column(index_column.column).nullable);

                let options = IndexOptions::builder()
                    .unique(unique)
                    .sparse(unique && nullable)
                    .build();

                let model = IndexModel::builder().keys(keys).options(options).build();

                collection
                    .create_index(model)
                    .await
                    .map_err(Error::driver_operation_failed)?;
            }
        }

        Ok(())
    }

    async fn applied_migrations(&mut self) -> Result<Vec<db::AppliedMigration>> {
        Err(Error::unsupported_feature(
            "MongoDB migrations are not supported: MongoDB has no DDL, so schema \
             changes are application-level data migrations",
        ))
    }

    async fn apply_migration(
        &mut self,
        _id: u64,
        _name: &str,
        _migration: &db::Migration,
    ) -> Result<()> {
        Err(Error::unsupported_feature(
            "MongoDB migrations are not supported: MongoDB has no DDL, so schema \
             changes are application-level data migrations",
        ))
    }
}

impl Connection {
    async fn exec_insert(&self, schema: &db::Schema, insert: stmt::Insert) -> Result<ExecResponse> {
        assert!(insert.returning.is_none());

        let insert_table = insert.target.as_table_unwrap();
        let table = schema.table(insert_table.table);
        let collection = self.collection(&table.name);

        let source = insert.source.body.into_values();

        let mut documents = Vec::new();
        for row in source.rows {
            let mut document = Document::new();
            for (i, column_id) in insert_table.columns.iter().enumerate() {
                let column = schema.column(*column_id);
                let entry = row.entry(i).unwrap();
                let value = entry.as_value_unwrap();

                // A primary-key column's value arrives wrapped in a
                // `Value::Record` (Toasty represents every primary key as a
                // record, one field per key column, even single-column keys).
                // Unwrap to this column's field before encoding so the scalar
                // is stored directly. Mirrors the DynamoDB driver's `ddb_key`.
                let value = if column.primary_key {
                    pk_field(table, column, value)
                } else {
                    value
                };

                // Skip nulls so absent fields stay absent in the document
                // rather than being stored as explicit nulls.
                if !value.is_null() {
                    document.insert(column.name.clone(), Value::from(value.clone()).to_bson()?);
                }
            }
            documents.push(document);
        }

        let count = documents.len();

        if !documents.is_empty() {
            collection
                .insert_many(documents)
                .await
                .map_err(Error::driver_operation_failed)?;
        }

        Ok(ExecResponse::count(count as u64))
    }

    async fn exec_scan(&self, schema: &Arc<Schema>, op: operation::Scan) -> Result<ExecResponse> {
        let table = schema.db.table(op.table);
        let cx = ExprContext::new_with_target(&schema.db, table);

        let query = op
            .filter
            .as_ref()
            .map(|expr| filter::translate_filter(&cx, expr))
            .transpose()?
            .unwrap_or_default();

        let columns: Vec<&Column> = op
            .columns
            .iter()
            .map(|&index| {
                schema.db.column(db::ColumnId {
                    table: op.table,
                    index,
                })
            })
            .collect();

        // A scan has no sort key; ordering on a scan path is rejected before
        // reaching the driver, so cursor pagination orders by the primary key.
        let (documents, next_cursor) = self.find_paginated(table, query, op.limit, None).await?;
        let rows = documents
            .iter()
            .map(|doc| document_to_record(doc, columns.iter().copied()))
            .collect::<Result<_>>()?;

        Ok(paginated_response(rows, next_cursor))
    }

    async fn exec_query_pk(
        &self,
        schema: &Arc<Schema>,
        op: operation::QueryPk,
    ) -> Result<ExecResponse> {
        let table = schema.db.table(op.table);
        let cx = ExprContext::new_with_target(&schema.db, table);

        let mut query = filter::translate_filter(&cx, &op.pk_filter)?;
        if let Some(post_filter) = &op.filter {
            let extra = filter::translate_filter(&cx, post_filter)?;
            query = and_documents(query, extra);
        }

        let columns: Vec<&Column> = op.select.iter().map(|&id| schema.db.column(id)).collect();

        let (documents, next_cursor) = self
            .find_paginated(table, query, op.limit, op.order)
            .await?;
        let rows = documents
            .iter()
            .map(|doc| document_to_record(doc, columns.iter().copied()))
            .collect::<Result<_>>()?;

        Ok(paginated_response(rows, next_cursor))
    }

    async fn exec_get_by_key(
        &self,
        schema: &Arc<Schema>,
        op: operation::GetByKey,
    ) -> Result<ExecResponse> {
        let table = schema.db.table(op.table);
        let pk_columns: Vec<&Column> = table.primary_key_columns().collect();

        let query = pk_in_filter(table, &pk_columns, &op.keys)?;

        let columns: Vec<&Column> = op.select.iter().map(|&id| schema.db.column(id)).collect();

        let documents = self.find(&table.name, query).await?;
        let rows = documents
            .iter()
            .map(|doc| document_to_record(doc, columns.iter().copied()))
            .collect::<Result<_>>()?;

        Ok(rows_response(rows))
    }

    async fn exec_delete_by_key(
        &self,
        schema: &Arc<Schema>,
        op: operation::DeleteByKey,
    ) -> Result<ExecResponse> {
        // MongoDB enforces unique indexes natively, so no secondary index
        // maintenance is needed (unlike the DynamoDB driver).
        let table = schema.db.table(op.table);
        let pk_columns: Vec<&Column> = table.primary_key_columns().collect();
        let cx = ExprContext::new_with_target(&schema.db, table);

        let mut base = pk_in_filter(table, &pk_columns, &op.keys)?;
        if let Some(filter) = &op.filter {
            base = and_documents(base, filter::translate_filter(&cx, filter)?);
        }

        let collection = self.collection(&table.name);

        let Some(condition) = &op.condition else {
            let result = collection
                .delete_many(base)
                .await
                .map_err(Error::driver_operation_failed)?;
            return Ok(ExecResponse::count(result.deleted_count));
        };

        // Optimistic-lock condition: a present row that fails the condition is
        // an error, not a silent no-op. Count the rows the key/filter selects,
        // then delete only those that also satisfy the condition; a shortfall
        // means the condition failed.
        let present = collection
            .count_documents(base.clone())
            .await
            .map_err(Error::driver_operation_failed)?;

        let query = and_documents(base, filter::translate_filter(&cx, condition)?);
        let result = collection
            .delete_many(query)
            .await
            .map_err(Error::driver_operation_failed)?;

        if result.deleted_count < present {
            return Err(Error::condition_failed(
                "delete condition not met (stale optimistic-lock version)",
            ));
        }

        Ok(ExecResponse::count(result.deleted_count))
    }

    async fn exec_update_by_key(
        &self,
        schema: &Arc<Schema>,
        op: operation::UpdateByKey,
    ) -> Result<ExecResponse> {
        // The `#[version]` bump is an ordinary assignment handled by
        // `build_update_doc`; `op.condition` is the optimistic-lock check
        // (e.g. `version == n`). A present row that fails the condition is an
        // error, not a silent no-op, so it is applied separately below rather
        // than folded into `op.filter`.
        let table = schema.db.table(op.table);
        let pk_columns: Vec<&Column> = table.primary_key_columns().collect();
        let cx = ExprContext::new_with_target(&schema.db, table);

        let update = build_update_doc(table, &op.assignments)?;
        let collection = self.collection(&table.name);

        let base_filter = |keys: &[stmt::Value]| -> Result<Document> {
            let mut filter = pk_in_filter(table, &pk_columns, keys)?;
            if let Some(post_filter) = &op.filter {
                filter = and_documents(filter, filter::translate_filter(&cx, post_filter)?);
            }
            Ok(filter)
        };
        let with_condition = |filter: Document| -> Result<Document> {
            match &op.condition {
                Some(condition) => Ok(and_documents(
                    filter,
                    filter::translate_filter(&cx, condition)?,
                )),
                None => Ok(filter),
            }
        };

        match &op.returning {
            None => {
                let base = base_filter(&op.keys)?;

                // When a condition is present, count the selected rows first so
                // a shortfall after the conditioned update signals a stale lock.
                let present = match &op.condition {
                    Some(_) => Some(
                        collection
                            .count_documents(base.clone())
                            .await
                            .map_err(Error::driver_operation_failed)?,
                    ),
                    None => None,
                };

                let result = collection
                    .update_many(with_condition(base)?, update)
                    .await
                    .map_err(Error::driver_operation_failed)?;

                if let Some(present) = present
                    && result.matched_count < present
                {
                    return Err(Error::condition_failed(
                        "update condition not met (stale optimistic-lock version)",
                    ));
                }

                Ok(ExecResponse::count(result.matched_count))
            }
            Some(returning) => {
                // `update_many` cannot return documents, and re-querying after
                // the update would miss rows whose updated columns no longer
                // match `op.filter`. Update each key individually with
                // `find_one_and_update` returning the post-update document.
                let columns: Vec<&Column> =
                    returning.iter().map(|&id| schema.db.column(id)).collect();

                let mut rows = Vec::new();
                for key in &op.keys {
                    let base = base_filter(std::slice::from_ref(key))?;

                    let updated = collection
                        .find_one_and_update(with_condition(base.clone())?, update.clone())
                        .return_document(ReturnDocument::After)
                        .await
                        .map_err(Error::driver_operation_failed)?;

                    match updated {
                        Some(doc) => rows.push(document_to_record(&doc, columns.iter().copied())?),
                        // No match: with a condition, a row that still exists
                        // means the condition failed (stale lock); otherwise the
                        // row was absent or filtered out.
                        None if op.condition.is_some() => {
                            let present = collection
                                .count_documents(base)
                                .await
                                .map_err(Error::driver_operation_failed)?;
                            if present > 0 {
                                return Err(Error::condition_failed(
                                    "update condition not met (stale optimistic-lock version)",
                                ));
                            }
                        }
                        None => {}
                    }
                }

                Ok(rows_response(rows))
            }
        }
    }

    async fn exec_find_pk_by_index(
        &self,
        schema: &Arc<Schema>,
        op: operation::FindPkByIndex,
    ) -> Result<ExecResponse> {
        let table = schema.db.table(op.table);
        let cx = ExprContext::new_with_target(&schema.db, table);

        let query = filter::translate_filter(&cx, &op.filter)?;

        let pk_columns: Vec<&Column> = table.primary_key_columns().collect();

        let documents = self.find(&table.name, query).await?;
        let rows = documents
            .iter()
            .map(|doc| document_to_record(doc, pk_columns.iter().copied()))
            .collect::<Result<_>>()?;

        Ok(rows_response(rows))
    }

    /// Runs a `find` with the given filter and collects all matching documents.
    async fn find(&self, collection: &str, query: Document) -> Result<Vec<Document>> {
        self.run_find(collection, query, None, None, None).await
    }

    /// Runs a `find` with optional sort, skip, and limit, collecting the
    /// matching documents.
    async fn run_find(
        &self,
        collection: &str,
        query: Document,
        sort: Option<Document>,
        skip: Option<u64>,
        limit: Option<i64>,
    ) -> Result<Vec<Document>> {
        tracing::trace!(collection, ?query, ?sort, ?skip, ?limit, "find");

        let coll = self.collection(collection);
        let mut find = coll.find(query);
        if let Some(sort) = sort {
            find = find.sort(sort);
        }
        if let Some(skip) = skip {
            find = find.skip(skip);
        }
        if let Some(limit) = limit {
            find = find.limit(limit);
        }

        let mut cursor = find.await.map_err(Error::driver_operation_failed)?;

        let mut documents = Vec::new();
        while cursor
            .advance()
            .await
            .map_err(Error::driver_operation_failed)?
        {
            documents.push(
                cursor
                    .deserialize_current()
                    .map_err(Error::driver_operation_failed)?,
            );
        }

        Ok(documents)
    }

    /// Runs a query with pagination, returning the matching documents and, for
    /// cursor pagination, the cursor to the next page (`None` once exhausted).
    ///
    /// `order` sorts by the primary key in that direction; non-key ordering is
    /// applied by the engine before the limit is pushed down, so it never
    /// reaches the driver.
    async fn find_paginated(
        &self,
        table: &db::Table,
        mut query: Document,
        limit: Option<operation::Pagination>,
        order: Option<stmt::Direction>,
    ) -> Result<(Vec<Document>, Option<stmt::Value>)> {
        use operation::Pagination;

        let pk_columns: Vec<&Column> = table.primary_key_columns().collect();

        match limit {
            None => {
                let docs = self
                    .run_find(&table.name, query, pk_sort(&pk_columns, order), None, None)
                    .await?;
                Ok((docs, None))
            }
            Some(Pagination::Offset { limit, offset }) => {
                let docs = self
                    .run_find(
                        &table.name,
                        query,
                        pk_sort(&pk_columns, order),
                        offset.map(|o| o as u64),
                        Some(limit),
                    )
                    .await?;
                Ok((docs, None))
            }
            Some(Pagination::Cursor { page_size, after }) => {
                // Keyset pagination requires a deterministic order over the
                // whole key; resume strictly after the previous page's last key.
                let direction = order.unwrap_or(stmt::Direction::Asc);

                if let Some(after) = after {
                    let bound = keyset_after(table, &pk_columns, &after, direction)?;
                    query = and_documents(query, bound);
                }

                let sort_value = direction_value(direction);
                let mut sort = Document::new();
                for column in &pk_columns {
                    sort.insert(column.name.clone(), sort_value);
                }

                let docs = self
                    .run_find(&table.name, query, Some(sort), None, Some(page_size))
                    .await?;

                // A full page may have a successor; a short page is the last.
                let next_cursor = (docs.len() as i64 == page_size)
                    .then(|| docs.last())
                    .flatten()
                    .map(|doc| key_cursor(&pk_columns, doc))
                    .transpose()?;

                Ok((docs, next_cursor))
            }
        }
    }
}

/// Projects a MongoDB document onto an ordered set of columns, producing a
/// Toasty value record. Absent fields become `Value::Null`.
fn document_to_record<'a>(
    document: &Document,
    columns: impl Iterator<Item = &'a Column>,
) -> Result<stmt::Value> {
    let fields = columns
        .map(|column| match document.get(&column.name) {
            Some(bson) => Value::from_bson(&column.ty, bson),
            None => Ok(stmt::Value::Null),
        })
        .collect::<Result<_>>()?;

    Ok(stmt::Value::from(stmt::ValueRecord::from_vec(fields)))
}

/// Extracts the field of a primary-key value that corresponds to a single key
/// column.
///
/// Toasty represents a primary key as a [`stmt::Value::Record`] with one field
/// per key column, even for single-column keys, so a key column's value reaches
/// the driver wrapped in a record. This unwraps it to the scalar for `column`.
/// A value that is already a scalar (not a record) passes through unchanged.
/// Mirrors the DynamoDB driver's `ddb_key`.
fn pk_field<'v>(table: &db::Table, column: &Column, key: &'v stmt::Value) -> &'v stmt::Value {
    match key {
        stmt::Value::Record(record) => {
            let index = table
                .primary_key
                .columns
                .iter()
                .position(|id| *id == column.id)
                .expect("primary key column missing from its table's primary key");
            &record[index]
        }
        value => value,
    }
}

/// Maps a sort direction to MongoDB's `1` (ascending) / `-1` (descending).
fn direction_value(direction: stmt::Direction) -> i32 {
    match direction {
        stmt::Direction::Asc => 1,
        stmt::Direction::Desc => -1,
    }
}

/// Builds a sort document over the primary-key columns in the given direction,
/// or `None` when no direction is requested.
fn pk_sort(pk_columns: &[&Column], order: Option<stmt::Direction>) -> Option<Document> {
    let value = direction_value(order?);

    let mut sort = Document::new();
    for column in pk_columns {
        sort.insert(column.name.clone(), value);
    }
    Some(sort)
}

/// Builds the keyset bound for resuming cursor pagination strictly after `key`.
///
/// For columns `(c1, …, cn)` compared in `direction`, this is the
/// lexicographic "after" predicate
/// `{ $or: [ { c1: <cmp> }, { c1: a1, c2: <cmp> }, … ] }`, where `<cmp>` is
/// `$gt` ascending or `$lt` descending. A single-column key collapses to
/// `{ c1: { <cmp>: a1 } }`.
fn keyset_after(
    table: &db::Table,
    pk_columns: &[&Column],
    key: &stmt::Value,
    direction: stmt::Direction,
) -> Result<Document> {
    let cmp = match direction {
        stmt::Direction::Asc => "$gt",
        stmt::Direction::Desc => "$lt",
    };

    let field_bson = |column: &Column| Value::from(pk_field(table, column, key).clone()).to_bson();

    let mut branches: Vec<Bson> = Vec::with_capacity(pk_columns.len());
    for (i, column) in pk_columns.iter().enumerate() {
        let mut branch = Document::new();
        // Equality on every earlier key column.
        for earlier in &pk_columns[..i] {
            branch.insert(earlier.name.clone(), field_bson(earlier)?);
        }
        // Strict comparison on this column.
        let mut cmp_doc = Document::new();
        cmp_doc.insert(cmp, field_bson(column)?);
        branch.insert(column.name.clone(), cmp_doc);
        branches.push(Bson::Document(branch));
    }

    if branches.len() == 1 {
        let Bson::Document(branch) = branches.remove(0) else {
            unreachable!()
        };
        return Ok(branch);
    }

    let mut doc = Document::new();
    doc.insert("$or", branches);
    Ok(doc)
}

/// Builds the cursor value for the last row of a page: the row's primary key —
/// a scalar for single-column keys, a record for composite keys.
fn key_cursor(pk_columns: &[&Column], doc: &Document) -> Result<stmt::Value> {
    let field = |column: &Column| match doc.get(&column.name) {
        Some(bson) => Value::from_bson(&column.ty, bson),
        None => Ok(stmt::Value::Null),
    };

    if let [pk] = pk_columns {
        return field(pk);
    }

    let fields = pk_columns
        .iter()
        .map(|column| field(column))
        .collect::<Result<_>>()?;
    Ok(stmt::Value::from(stmt::ValueRecord::from_vec(fields)))
}

/// Builds a filter selecting rows by primary key.
///
/// Each key is flattened with [`pk_field`] to its per-column scalars, matching
/// the values stored on insert.
///
/// - Single-column key: `{ pk_col: { $in: [...] } }`.
/// - Composite key: `{ $or: [ { c1: v1, c2: v2 }, ... ] }` — one equality
///   document per key tuple. A single key collapses to the bare document.
fn pk_in_filter(
    table: &db::Table,
    pk_columns: &[&Column],
    keys: &[stmt::Value],
) -> Result<Document> {
    if let [pk_column] = pk_columns {
        let key_values: Vec<Bson> = keys
            .iter()
            .map(|key| Value::from(pk_field(table, pk_column, key).clone()).to_bson())
            .collect::<Result<_>>()?;

        let mut in_clause = Document::new();
        in_clause.insert("$in", key_values);
        let mut query = Document::new();
        query.insert(pk_column.name.clone(), in_clause);
        return Ok(query);
    }

    let mut branches: Vec<Bson> = Vec::with_capacity(keys.len());
    for key in keys {
        let mut branch = Document::new();
        for column in pk_columns {
            branch.insert(
                column.name.clone(),
                Value::from(pk_field(table, column, key).clone()).to_bson()?,
            );
        }
        branches.push(Bson::Document(branch));
    }

    if branches.len() == 1 {
        let Bson::Document(branch) = branches.remove(0) else {
            unreachable!()
        };
        return Ok(branch);
    }

    let mut query = Document::new();
    query.insert("$or", branches);
    Ok(query)
}

/// Translates a set of column assignments into a MongoDB update document.
///
/// `Set` → `$set` (or `$unset` when the value is null, matching `exec_insert`,
/// which omits null fields), `Add` → `$inc`, `Subtract` → `$inc` with the value
/// negated, and `Append` → `$push` with `$each`. Collection mutations
/// (`Remove`, `Pop`, `RemoveAt`) are gated off by capability and never reach
/// the driver.
fn build_update_doc(table: &db::Table, assignments: &stmt::Assignments) -> Result<Document> {
    let mut set = Document::new();
    let mut unset = Document::new();
    let mut inc = Document::new();
    let mut push = Document::new();

    for (projection, assignment) in assignments.iter() {
        let name = table.resolve(projection).name.clone();

        let expr = match assignment {
            stmt::Assignment::Set(expr)
            | stmt::Assignment::Add(expr)
            | stmt::Assignment::Subtract(expr)
            | stmt::Assignment::Append(expr) => expr,
            other => {
                return Err(Error::unsupported_feature(format!(
                    "the MongoDB driver does not support this assignment: {other:#?}"
                )));
            }
        };

        let stmt::Expr::Value(value) = expr else {
            return Err(Error::unsupported_feature(format!(
                "the MongoDB driver only supports constant assignment values: {expr:#?}"
            )));
        };

        match assignment {
            stmt::Assignment::Set(_) if value.is_null() => {
                unset.insert(name, "");
            }
            stmt::Assignment::Set(_) => {
                set.insert(name, Value::from(value.clone()).to_bson()?);
            }
            stmt::Assignment::Add(_) => {
                inc.insert(name, Value::from(value.clone()).to_bson()?);
            }
            stmt::Assignment::Subtract(_) => {
                inc.insert(name, negate_bson(Value::from(value.clone()).to_bson()?));
            }
            stmt::Assignment::Append(_) => {
                let items = match Value::from(value.clone()).to_bson()? {
                    Bson::Array(items) => items,
                    other => vec![other],
                };
                push.insert(name, mongodb::bson::doc! { "$each": items });
            }
            _ => unreachable!(),
        }
    }

    let mut update = Document::new();
    if !set.is_empty() {
        update.insert("$set", set);
    }
    if !unset.is_empty() {
        update.insert("$unset", unset);
    }
    if !inc.is_empty() {
        update.insert("$inc", inc);
    }
    if !push.is_empty() {
        update.insert("$push", push);
    }
    Ok(update)
}

/// Negates a numeric BSON value, used to turn a `Subtract` assignment into a
/// negative `$inc`.
fn negate_bson(value: Bson) -> Bson {
    match value {
        Bson::Int32(v) => Bson::Int32(-v),
        Bson::Int64(v) => Bson::Int64(-v),
        Bson::Double(v) => Bson::Double(-v),
        other => other,
    }
}

/// Combines two query documents with `$and`.
fn and_documents(a: Document, b: Document) -> Document {
    let mut doc = Document::new();
    doc.insert("$and", vec![Bson::Document(a), Bson::Document(b)]);
    doc
}

/// Wraps a vector of row records in an [`ExecResponse`] value stream.
fn rows_response(rows: Vec<stmt::Value>) -> ExecResponse {
    paginated_response(rows, None)
}

/// Wraps row records in an [`ExecResponse`] value stream, carrying the
/// next-page cursor for cursor-based pagination.
fn paginated_response(rows: Vec<stmt::Value>, next_cursor: Option<stmt::Value>) -> ExecResponse {
    ExecResponse {
        values: Rows::Stream(stmt::ValueStream::from_vec(rows)),
        next_cursor,
        prev_cursor: None,
    }
}
