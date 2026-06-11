use std::sync::Arc;

use toasty_core::{
    Result, Schema,
    driver::{ExecResponse, Rows, operation},
    stmt,
};

use super::exists::{self, ExistsOutcome};
use crate::{Connection, filter};

impl Connection {
    pub(crate) async fn exec_count_documents(
        &mut self,
        schema: &Arc<Schema>,
        op: operation::CountDocuments,
    ) -> Result<ExecResponse> {
        // If the filter contains an EXISTS pre-condition that is not satisfied,
        // short-circuit with a count of zero.
        let remaining_filter = if let Some(filter_expr) = &op.filter {
            match exists::evaluate(self, schema, filter_expr).await? {
                ExistsOutcome::ShortCircuit => {
                    let row = stmt::Value::record_from_vec(vec![stmt::Value::U64(0)]);
                    return Ok(ExecResponse {
                        values: Rows::Stream(stmt::ValueStream::from_vec(vec![row])),
                        next_cursor: None,
                        prev_cursor: None,
                    });
                }
                ExistsOutcome::Proceed(rest) => rest,
            }
        } else {
            None
        };

        let table = schema.db.table(op.table);
        let cx = stmt::ExprContext::new_with_target(&schema.db, table);

        let query = remaining_filter
            .as_ref()
            .map(|expr| filter::translate_filter(&cx, expr))
            .transpose()?
            .unwrap_or_default();

        let collection = self.collection(&table.name);
        let count = if let Some(sess) = self.session.as_mut() {
            collection
                .count_documents(query)
                .session(sess)
                .await
                .map_err(toasty_core::Error::driver_operation_failed)?
        } else {
            collection
                .count_documents(query)
                .await
                .map_err(toasty_core::Error::driver_operation_failed)?
        };

        let row = stmt::Value::record_from_vec(vec![stmt::Value::U64(count)]);
        Ok(ExecResponse {
            values: Rows::Stream(stmt::ValueStream::from_vec(vec![row])),
            next_cursor: None,
            prev_cursor: None,
        })
    }
}
