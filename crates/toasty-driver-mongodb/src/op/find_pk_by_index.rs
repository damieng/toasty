use std::sync::Arc;

use toasty_core::{
    Result, Schema,
    driver::{ExecResponse, operation},
    schema::db::Column,
    stmt::ExprContext,
};

use crate::{Connection, document_to_record, filter, rows_response};

impl Connection {
    pub(crate) async fn exec_find_pk_by_index(
        &mut self,
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
}
