use std::sync::Arc;

use toasty_core::{
    Result, Schema,
    driver::{ExecResponse, Rows, operation},
    stmt,
};

use crate::{Connection, filter};

impl Connection {
    pub(crate) async fn exec_count_documents(
        &self,
        schema: &Arc<Schema>,
        op: operation::CountDocuments,
    ) -> Result<ExecResponse> {
        let table = schema.db.table(op.table);
        let cx = stmt::ExprContext::new_with_target(&schema.db, table);

        let query = op
            .filter
            .as_ref()
            .map(|expr| filter::translate_filter(&cx, expr))
            .transpose()?
            .unwrap_or_default();

        let count = self
            .collection(&table.name)
            .count_documents(query)
            .await
            .map_err(toasty_core::Error::driver_operation_failed)?;

        let row = stmt::Value::record_from_vec(vec![stmt::Value::U64(count)]);
        Ok(ExecResponse {
            values: Rows::Stream(stmt::ValueStream::from_vec(vec![row])),
            next_cursor: None,
            prev_cursor: None,
        })
    }
}
