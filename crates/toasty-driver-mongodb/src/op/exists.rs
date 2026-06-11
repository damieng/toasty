use std::sync::Arc;

use toasty_core::{Error, Result, Schema, stmt};

use crate::{Connection, filter};

/// Outcome of evaluating EXISTS conditions found in a filter expression.
pub(crate) enum ExistsOutcome {
    /// All EXISTS conditions satisfied — proceed with this optional remaining filter.
    Proceed(Option<stmt::Expr>),
    /// At least one EXISTS condition failed — the calling operation should be a no-op.
    ShortCircuit,
}

/// Evaluates all top-level `Expr::Exists` predicates in `filter` using
/// `count_documents`. Returns `ShortCircuit` as soon as one EXISTS returns 0.
/// The remainder of the filter (non-EXISTS parts) is returned via `Proceed`.
pub(crate) async fn evaluate(
    conn: &Connection,
    schema: &Arc<Schema>,
    filter: &stmt::Expr,
) -> Result<ExistsOutcome> {
    match filter {
        stmt::Expr::Exists(exists) => {
            if check_one(conn, schema, exists).await? {
                Ok(ExistsOutcome::Proceed(None))
            } else {
                Ok(ExistsOutcome::ShortCircuit)
            }
        }
        stmt::Expr::And(and) => {
            let mut remaining = Vec::new();
            for operand in &and.operands {
                if let stmt::Expr::Exists(exists) = operand {
                    if !check_one(conn, schema, exists).await? {
                        return Ok(ExistsOutcome::ShortCircuit);
                    }
                } else {
                    remaining.push(operand.clone());
                }
            }
            let rest = match remaining.len() {
                0 => None,
                1 => Some(remaining.into_iter().next().unwrap()),
                _ => Some(stmt::Expr::And(stmt::ExprAnd {
                    operands: remaining,
                })),
            };
            Ok(ExistsOutcome::Proceed(rest))
        }
        // No EXISTS at the top level — pass through unchanged.
        other => Ok(ExistsOutcome::Proceed(Some(other.clone()))),
    }
}

/// Runs `count_documents` against the table named by the EXISTS subquery.
async fn check_one(
    conn: &Connection,
    schema: &Arc<Schema>,
    exists: &stmt::ExprExists,
) -> Result<bool> {
    let stmt::ExprSet::Select(select) = &exists.subquery.body else {
        return Err(Error::unsupported_feature(
            "MongoDB native_exists: expected SELECT body in EXISTS subquery",
        ));
    };

    let source_table = select.source.as_table().ok_or_else(|| {
        Error::unsupported_feature(
            "MongoDB native_exists: expected table source in EXISTS subquery",
        )
    })?;

    let table_ref = source_table.tables.first().ok_or_else(|| {
        Error::unsupported_feature("MongoDB native_exists: EXISTS subquery has no table references")
    })?;

    let stmt::TableRef::Table(table_id) = table_ref else {
        return Err(Error::unsupported_feature(
            "MongoDB native_exists: EXISTS subquery source must be a direct table reference",
        ));
    };

    let table = schema.db.table(*table_id);
    let cx = stmt::ExprContext::new_with_target(&schema.db, table);

    let query = select
        .filter
        .expr
        .as_ref()
        .map(|expr| filter::translate_filter(&cx, expr))
        .transpose()?
        .unwrap_or_default();

    let count = conn
        .collection(&table.name)
        .count_documents(query)
        .await
        .map_err(toasty_core::Error::driver_operation_failed)?;

    Ok(count > 0)
}
