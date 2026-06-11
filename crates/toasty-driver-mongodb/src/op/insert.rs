use mongodb::bson::Document;
use toasty_core::{Error, Result, driver::ExecResponse, schema::db, stmt};

use crate::{Connection, Value, pk_field};

impl Connection {
    pub(crate) async fn exec_insert(
        &mut self,
        schema: &db::Schema,
        insert: stmt::Insert,
    ) -> Result<ExecResponse> {
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
            if let Some(sess) = self.session.as_mut() {
                collection
                    .insert_many(documents)
                    .session(sess)
                    .await
                    .map_err(Error::driver_operation_failed)?;
            } else {
                collection
                    .insert_many(documents)
                    .await
                    .map_err(Error::driver_operation_failed)?;
            }
        }

        Ok(ExecResponse::count(count as u64))
    }
}
