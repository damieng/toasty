use std::sync::Arc;

use toasty_core::{
    Result, Schema,
    driver::{ExecResponse, operation},
    schema::db::Column,
    stmt::ExprContext,
};

use super::exists::{self, ExistsOutcome};
use crate::{Connection, and_documents, document_to_record, filter, paginated_response};

impl Connection {
    pub(crate) async fn exec_query_pk(
        &self,
        schema: &Arc<Schema>,
        op: operation::QueryPk,
    ) -> Result<ExecResponse> {
        // Evaluate any EXISTS pre-conditions in the post-filter.
        let remaining_post_filter = if let Some(post_filter) = &op.filter {
            match exists::evaluate(self, schema, post_filter).await? {
                ExistsOutcome::ShortCircuit => return Ok(paginated_response(vec![], None)),
                ExistsOutcome::Proceed(rest) => rest,
            }
        } else {
            None
        };

        let table = schema.db.table(op.table);
        let cx = ExprContext::new_with_target(&schema.db, table);

        let mut query = filter::translate_filter(&cx, &op.pk_filter)?;
        if let Some(post_filter) = &remaining_post_filter {
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
}
