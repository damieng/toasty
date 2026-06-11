use std::sync::Arc;

use toasty_core::{
    Result, Schema,
    driver::{ExecResponse, operation},
    schema::db::Column,
};

use crate::{Connection, document_to_record, pk_in_filter, rows_response};

impl Connection {
    pub(crate) async fn exec_get_by_key(
        &mut self,
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
}
