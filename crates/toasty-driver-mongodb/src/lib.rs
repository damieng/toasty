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
//! [`FindPkByIndex`](operation::FindPkByIndex). Optimistic-lock conditions,
//! transactions, and migrations are not yet supported.

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

                let options = IndexOptions::builder()
                    .unique(index.unique || index.primary_key)
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
        todo!("MongoDB migrations are not yet implemented")
    }

    async fn apply_migration(
        &mut self,
        _id: u64,
        _name: &str,
        _migration: &db::Migration,
    ) -> Result<()> {
        todo!("MongoDB migrations are not yet implemented")
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
                    document.insert(column.name.clone(), Value::from(value.clone()).to_bson());
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
            .collect();

        Ok(paginated_response(rows, next_cursor))
    }

    async fn exec_query_pk(
        &self,
        schema: &Arc<Schema>,
        op: operation::QueryPk,
    ) -> Result<ExecResponse> {
        let table = schema.db.table(op.table);
        let cx = ExprContext::new_with_target(&schema.db, table);

        let mut query = filter::translate_filter(&cx, &op.pk_filter);
        if let Some(post_filter) = &op.filter {
            let extra = filter::translate_filter(&cx, post_filter);
            query = and_documents(query, extra);
        }

        let columns: Vec<&Column> = op.select.iter().map(|&id| schema.db.column(id)).collect();

        let (documents, next_cursor) = self
            .find_paginated(table, query, op.limit, op.order)
            .await?;
        let rows = documents
            .iter()
            .map(|doc| document_to_record(doc, columns.iter().copied()))
            .collect();

        Ok(paginated_response(rows, next_cursor))
    }

    async fn exec_get_by_key(
        &self,
        schema: &Arc<Schema>,
        op: operation::GetByKey,
    ) -> Result<ExecResponse> {
        let table = schema.db.table(op.table);
        let pk_columns: Vec<&Column> = table.primary_key_columns().collect();

        if pk_columns.len() != 1 {
            return Err(Error::unsupported_feature(
                "composite primary keys are not yet supported by the MongoDB driver",
            ));
        }

        let query = pk_in_filter(table, pk_columns[0], &op.keys);

        let columns: Vec<&Column> = op.select.iter().map(|&id| schema.db.column(id)).collect();

        let documents = self.find(&table.name, query).await?;
        let rows = documents
            .iter()
            .map(|doc| document_to_record(doc, columns.iter().copied()))
            .collect();

        Ok(rows_response(rows))
    }

    async fn exec_delete_by_key(
        &self,
        schema: &Arc<Schema>,
        op: operation::DeleteByKey,
    ) -> Result<ExecResponse> {
        // Optimistic-lock conditions are not yet supported; a plain key/filter
        // delete is. MongoDB enforces unique indexes natively, so no secondary
        // index maintenance is needed (unlike the DynamoDB driver).
        if op.condition.is_some() {
            return Err(Error::unsupported_feature(
                "delete conditions are not yet supported by the MongoDB driver",
            ));
        }

        let table = schema.db.table(op.table);
        let pk_columns: Vec<&Column> = table.primary_key_columns().collect();

        if pk_columns.len() != 1 {
            return Err(Error::unsupported_feature(
                "composite primary keys are not yet supported by the MongoDB driver",
            ));
        }

        let mut query = pk_in_filter(table, pk_columns[0], &op.keys);

        if let Some(filter) = &op.filter {
            let cx = ExprContext::new_with_target(&schema.db, table);
            let extra = filter::translate_filter(&cx, filter);
            query = and_documents(query, extra);
        }

        let result = self
            .collection(&table.name)
            .delete_many(query)
            .await
            .map_err(Error::driver_operation_failed)?;

        Ok(ExecResponse::count(result.deleted_count))
    }

    async fn exec_update_by_key(
        &self,
        schema: &Arc<Schema>,
        op: operation::UpdateByKey,
    ) -> Result<ExecResponse> {
        // Optimistic-lock conditions (e.g. a `#[version]` check) require
        // distinguishing "row missing / filtered out" from "row present but
        // condition failed". MongoDB's update operators don't surface that
        // directly, so conditions are a follow-up. The version *bump* itself is
        // an ordinary assignment and is handled below.
        if op.condition.is_some() {
            return Err(Error::unsupported_feature(
                "update conditions are not yet supported by the MongoDB driver",
            ));
        }

        let table = schema.db.table(op.table);
        let pk_columns: Vec<&Column> = table.primary_key_columns().collect();

        if pk_columns.len() != 1 {
            return Err(Error::unsupported_feature(
                "composite primary keys are not yet supported by the MongoDB driver",
            ));
        }

        let update = build_update_doc(table, &op.assignments)?;
        let collection = self.collection(&table.name);

        let mut filter = pk_in_filter(table, pk_columns[0], &op.keys);
        if let Some(post_filter) = &op.filter {
            let cx = ExprContext::new_with_target(&schema.db, table);
            let extra = filter::translate_filter(&cx, post_filter);
            filter = and_documents(filter, extra);
        }

        match &op.returning {
            None => {
                let result = collection
                    .update_many(filter, update)
                    .await
                    .map_err(Error::driver_operation_failed)?;

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
                    let mut key_filter =
                        pk_in_filter(table, pk_columns[0], std::slice::from_ref(key));
                    if let Some(post_filter) = &op.filter {
                        let cx = ExprContext::new_with_target(&schema.db, table);
                        let extra = filter::translate_filter(&cx, post_filter);
                        key_filter = and_documents(key_filter, extra);
                    }

                    let updated = collection
                        .find_one_and_update(key_filter, update.clone())
                        .return_document(ReturnDocument::After)
                        .await
                        .map_err(Error::driver_operation_failed)?;

                    if let Some(doc) = updated {
                        rows.push(document_to_record(&doc, columns.iter().copied()));
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

        let query = filter::translate_filter(&cx, &op.filter);

        let pk_columns: Vec<&Column> = table.primary_key_columns().collect();

        let documents = self.find(&table.name, query).await?;
        let rows = documents
            .iter()
            .map(|doc| document_to_record(doc, pk_columns.iter().copied()))
            .collect();

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
                let [pk] = pk_columns[..] else {
                    return Err(Error::unsupported_feature(
                        "cursor pagination over composite primary keys is not yet supported \
                         by the MongoDB driver",
                    ));
                };

                // Keyset pagination: order by the primary key and resume after
                // the previous page's last key.
                if let Some(after) = after {
                    let bound = mongodb::bson::doc! {
                        &pk.name: { "$gt": Value::from(after).to_bson() },
                    };
                    query = and_documents(query, bound);
                }

                let sort = mongodb::bson::doc! { &pk.name: 1_i32 };
                let docs = self
                    .run_find(&table.name, query, Some(sort), None, Some(page_size))
                    .await?;

                // A full page may have a successor; a short page is the last.
                let next_cursor = (docs.len() as i64 == page_size)
                    .then(|| docs.last())
                    .flatten()
                    .and_then(|doc| doc.get(&pk.name))
                    .map(|bson| Value::from_bson(&pk.ty, bson));

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
) -> stmt::Value {
    let record = stmt::ValueRecord::from_vec(
        columns
            .map(|column| match document.get(&column.name) {
                Some(bson) => Value::from_bson(&column.ty, bson),
                None => stmt::Value::Null,
            })
            .collect(),
    );

    stmt::Value::from(record)
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

/// Builds a sort document over the primary-key columns in the given direction,
/// or `None` when no direction is requested.
fn pk_sort(pk_columns: &[&Column], order: Option<stmt::Direction>) -> Option<Document> {
    let value = match order? {
        stmt::Direction::Asc => 1_i32,
        stmt::Direction::Desc => -1_i32,
    };

    let mut sort = Document::new();
    for column in pk_columns {
        sort.insert(column.name.clone(), value);
    }
    Some(sort)
}

/// Builds a `{ pk_col: { $in: [...] } }` filter selecting rows by primary key.
///
/// Each key is flattened with [`pk_field`] to the single key column's scalar,
/// matching the value stored on insert.
fn pk_in_filter(table: &db::Table, pk_column: &Column, keys: &[stmt::Value]) -> Document {
    let key_values: Vec<Bson> = keys
        .iter()
        .map(|key| Value::from(pk_field(table, pk_column, key).clone()).to_bson())
        .collect();

    let mut in_clause = Document::new();
    in_clause.insert("$in", key_values);
    let mut query = Document::new();
    query.insert(pk_column.name.clone(), in_clause);
    query
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
                set.insert(name, Value::from(value.clone()).to_bson());
            }
            stmt::Assignment::Add(_) => {
                inc.insert(name, Value::from(value.clone()).to_bson());
            }
            stmt::Assignment::Subtract(_) => {
                inc.insert(name, negate_bson(Value::from(value.clone()).to_bson()));
            }
            stmt::Assignment::Append(_) => {
                let items = match Value::from(value.clone()).to_bson() {
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
