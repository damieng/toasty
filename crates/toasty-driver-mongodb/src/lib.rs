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
//! [`DeleteByKey`](operation::DeleteByKey), and
//! [`FindPkByIndex`](operation::FindPkByIndex). Updates, transactions, and
//! migrations are not yet supported.

mod filter;
mod value;

pub(crate) use value::Value;

use async_trait::async_trait;
use mongodb::{
    Client, Collection, Database, IndexModel,
    bson::{Bson, Document},
    options::IndexOptions,
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
            Operation::UpdateByKey(_) => Err(Error::unsupported_feature(
                "updates are not yet supported by the MongoDB driver",
            )),
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
        if op.limit.is_some() {
            return Err(Error::unsupported_feature(
                "scan pagination is not yet supported by the MongoDB driver",
            ));
        }

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

        let documents = self.find(&table.name, query).await?;
        let rows = documents
            .iter()
            .map(|doc| document_to_record(doc, columns.iter().copied()))
            .collect();

        Ok(rows_response(rows))
    }

    async fn exec_query_pk(
        &self,
        schema: &Arc<Schema>,
        op: operation::QueryPk,
    ) -> Result<ExecResponse> {
        if op.limit.is_some() {
            return Err(Error::unsupported_feature(
                "query pagination is not yet supported by the MongoDB driver",
            ));
        }

        let table = schema.db.table(op.table);
        let cx = ExprContext::new_with_target(&schema.db, table);

        let mut query = filter::translate_filter(&cx, &op.pk_filter);
        if let Some(post_filter) = &op.filter {
            let extra = filter::translate_filter(&cx, post_filter);
            query = and_documents(query, extra);
        }

        let columns: Vec<&Column> = op.select.iter().map(|&id| schema.db.column(id)).collect();

        let documents = self.find(&table.name, query).await?;
        let rows = documents
            .iter()
            .map(|doc| document_to_record(doc, columns.iter().copied()))
            .collect();

        Ok(rows_response(rows))
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
        tracing::trace!(collection, ?query, "find");

        let mut cursor = self
            .collection(collection)
            .find(query)
            .await
            .map_err(Error::driver_operation_failed)?;

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

/// Combines two query documents with `$and`.
fn and_documents(a: Document, b: Document) -> Document {
    let mut doc = Document::new();
    doc.insert("$and", vec![Bson::Document(a), Bson::Document(b)]);
    doc
}

/// Wraps a vector of row records in an [`ExecResponse`] value stream.
fn rows_response(rows: Vec<stmt::Value>) -> ExecResponse {
    ExecResponse {
        values: Rows::Stream(stmt::ValueStream::from_vec(rows)),
        next_cursor: None,
        prev_cursor: None,
    }
}
