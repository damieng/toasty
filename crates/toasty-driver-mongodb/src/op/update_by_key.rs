use std::sync::Arc;

use mongodb::{bson::Document, options::ReturnDocument};
use toasty_core::{
    Error, Result, Schema,
    driver::{ExecResponse, operation},
    schema::db::Column,
    stmt::{self, ExprContext},
};

use super::exists::{self, ExistsOutcome};
use crate::{
    Connection, and_documents, build_update_doc, document_to_record, filter, pk_in_filter,
    rows_response,
};

impl Connection {
    pub(crate) async fn exec_update_by_key(
        &self,
        schema: &Arc<Schema>,
        op: operation::UpdateByKey,
    ) -> Result<ExecResponse> {
        // Evaluate any EXISTS pre-conditions in the post-filter. If the EXISTS
        // condition fails (the related entity does not exist), the update is a
        // no-op — return 0 matched rows.
        let remaining_filter = if let Some(filter_expr) = &op.filter {
            match exists::evaluate(self, schema, filter_expr).await? {
                ExistsOutcome::ShortCircuit => return Ok(ExecResponse::count(0)),
                ExistsOutcome::Proceed(rest) => rest,
            }
        } else {
            None
        };

        // The `#[version]` bump is an ordinary assignment handled by
        // `build_update_doc`; `op.condition` is the optimistic-lock check
        // (e.g. `version == n`). A present row that fails the condition is an
        // error, not a silent no-op, so it is applied separately below rather
        // than folded into `op.filter`.
        let table = schema.db.table(op.table);
        let pk_columns: Vec<&Column> = table.primary_key_columns().collect();
        let cx = ExprContext::new_with_target(&schema.db, table);

        let update = build_update_doc(table, &op.assignments)?;
        let collection = self.collection(&table.name);

        let base_filter = |keys: &[stmt::Value]| -> Result<Document> {
            let mut filter = pk_in_filter(table, &pk_columns, keys)?;
            if let Some(post_filter) = &remaining_filter {
                filter = and_documents(filter, filter::translate_filter(&cx, post_filter)?);
            }
            Ok(filter)
        };
        let with_condition = |filter: Document| -> Result<Document> {
            match &op.condition {
                Some(condition) => Ok(and_documents(
                    filter,
                    filter::translate_filter(&cx, condition)?,
                )),
                None => Ok(filter),
            }
        };

        match &op.returning {
            None => {
                let base = base_filter(&op.keys)?;

                // When a condition is present, count the selected rows first so
                // a shortfall after the conditioned update signals a stale lock.
                let present = match &op.condition {
                    Some(_) => Some(
                        collection
                            .count_documents(base.clone())
                            .await
                            .map_err(Error::driver_operation_failed)?,
                    ),
                    None => None,
                };

                let result = collection
                    .update_many(with_condition(base)?, update)
                    .await
                    .map_err(Error::driver_operation_failed)?;

                if let Some(present) = present
                    && result.matched_count < present
                {
                    return Err(Error::condition_failed(
                        "update condition not met (stale optimistic-lock version)",
                    ));
                }

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
                    let base = base_filter(std::slice::from_ref(key))?;

                    let updated = collection
                        .find_one_and_update(with_condition(base.clone())?, update.clone())
                        .return_document(ReturnDocument::After)
                        .await
                        .map_err(Error::driver_operation_failed)?;

                    match updated {
                        Some(doc) => rows.push(document_to_record(&doc, columns.iter().copied())?),
                        // No match: with a condition, a row that still exists
                        // means the condition failed (stale lock); otherwise the
                        // row was absent or filtered out.
                        None if op.condition.is_some() => {
                            let present = collection
                                .count_documents(base)
                                .await
                                .map_err(Error::driver_operation_failed)?;
                            if present > 0 {
                                return Err(Error::condition_failed(
                                    "update condition not met (stale optimistic-lock version)",
                                ));
                            }
                        }
                        None => {}
                    }
                }

                Ok(rows_response(rows))
            }
        }
    }
}
