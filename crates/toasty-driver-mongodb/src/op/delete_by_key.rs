use std::sync::Arc;

use toasty_core::{
    Error, Result, Schema,
    driver::{ExecResponse, operation},
    schema::db::Column,
    stmt::ExprContext,
};

use super::exists::{self, ExistsOutcome};
use crate::{Connection, and_documents, filter, pk_in_filter};

impl Connection {
    pub(crate) async fn exec_delete_by_key(
        &self,
        schema: &Arc<Schema>,
        op: operation::DeleteByKey,
    ) -> Result<ExecResponse> {
        // Evaluate any EXISTS pre-conditions in the post-filter. If the EXISTS
        // condition fails, the delete is a no-op.
        let remaining_filter = if let Some(filter_expr) = &op.filter {
            match exists::evaluate(self, schema, filter_expr).await? {
                ExistsOutcome::ShortCircuit => return Ok(ExecResponse::count(0)),
                ExistsOutcome::Proceed(rest) => rest,
            }
        } else {
            None
        };

        // MongoDB enforces unique indexes natively, so no secondary index
        // maintenance is needed (unlike the DynamoDB driver).
        let table = schema.db.table(op.table);
        let pk_columns: Vec<&Column> = table.primary_key_columns().collect();
        let cx = ExprContext::new_with_target(&schema.db, table);

        let mut base = pk_in_filter(table, &pk_columns, &op.keys)?;
        if let Some(filter) = &remaining_filter {
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
}
