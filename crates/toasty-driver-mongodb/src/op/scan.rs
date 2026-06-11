use std::sync::Arc;

use toasty_core::{
    Result, Schema,
    driver::{ExecResponse, operation},
    schema::db::{self, Column},
    stmt::ExprContext,
};

use super::exists::{self, ExistsOutcome};
use crate::{Connection, document_to_record, filter, paginated_response};

impl Connection {
    pub(crate) async fn exec_scan(
        &mut self,
        schema: &Arc<Schema>,
        op: operation::Scan,
    ) -> Result<ExecResponse> {
        // Evaluate any EXISTS pre-conditions before translating the filter.
        let remaining_filter = if let Some(filter_expr) = &op.filter {
            match exists::evaluate(self, schema, filter_expr).await? {
                ExistsOutcome::ShortCircuit => return Ok(paginated_response(vec![], None)),
                ExistsOutcome::Proceed(rest) => rest,
            }
        } else {
            None
        };

        let table = schema.db.table(op.table);
        let cx = ExprContext::new_with_target(&schema.db, table);

        let query = remaining_filter
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
}
