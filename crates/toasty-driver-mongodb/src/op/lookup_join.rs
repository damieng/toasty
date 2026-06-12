use std::sync::Arc;

use mongodb::bson::{Bson, Document};
use toasty_core::{
    Error, Result, Schema,
    driver::{ExecResponse, operation},
    schema::db,
    stmt,
};

use crate::{Connection, Value, document_to_record, rows_response};

impl Connection {
    pub(crate) async fn exec_lookup_join(
        &mut self,
        schema: &Arc<Schema>,
        op: operation::LookupJoin,
    ) -> Result<ExecResponse> {
        let root_table = schema.db.table(op.root_table);

        // --- $match stage ---
        let match_doc = build_match_filter(root_table, &op.filter)?;

        let mut pipeline: Vec<Document> = vec![mongodb::bson::doc! { "$match": match_doc }];

        // --- $lookup + $unwind stages ---
        // steps[0] starts from root_table; steps[N] starts from steps[N-1].foreign_table.
        for (i, step) in op.steps.iter().enumerate() {
            let alias = format!("_j{i}");

            // Determine the localField path.
            // For i=0: direct column name in root_table.
            // For i>0: "_j(i-1).<col_name>" (column in the previous step's foreign table).
            let local_field = if i == 0 {
                root_table.columns[step.local_column].name.clone()
            } else {
                let prev_foreign = schema.db.table(op.steps[i - 1].foreign_table);
                format!("_j{}.", i - 1) + &prev_foreign.columns[step.local_column].name
            };

            let foreign_table = schema.db.table(step.foreign_table);
            let foreign_field = foreign_table.columns[step.foreign_column].name.clone();

            pipeline.push(mongodb::bson::doc! {
                "$lookup": {
                    "from": &foreign_table.name,
                    "localField": &local_field,
                    "foreignField": &foreign_field,
                    "as": &alias,
                }
            });
            pipeline.push(mongodb::bson::doc! {
                "$unwind": {
                    "path": format!("${alias}"),
                    "preserveNullAndEmptyArrays": false,
                }
            });
        }

        // --- $group stage (DISTINCT deduplication) ---
        // The last step's alias holds the target document.
        let n_steps = op.steps.len();
        let target_alias = format!("_j{}", n_steps - 1);
        let link_col_name = &root_table.columns[op.link_column].name;

        // Use the target table's first PK column to identify unique targets.
        let target_table = schema.db.table(op.steps[n_steps - 1].foreign_table);
        let target_pk_col = target_table
            .primary_key_columns()
            .next()
            .ok_or_else(|| Error::unsupported_feature("lookup_join: target table has no PK"))?;

        if op.distinct {
            pipeline.push(mongodb::bson::doc! {
                "$group": {
                    "_id": {
                        "link": format!("${link_col_name}"),
                        "target": format!("${target_alias}.{}", target_pk_col.name),
                    },
                    "_link": { "$first": format!("${link_col_name}") },
                    "_target": { "$first": format!("${target_alias}") },
                }
            });
        } else {
            pipeline.push(mongodb::bson::doc! {
                "$project": {
                    "_id": 0,
                    "_link": format!("${link_col_name}"),
                    "_target": format!("${target_alias}"),
                }
            });
        }

        // --- Execute the aggregation pipeline ---
        let root_coll = self.collection(&root_table.name);
        let docs = run_aggregate(root_coll, pipeline, self.session.as_mut()).await?;

        // --- Convert each result doc to [link_val, target_record] ---
        let link_col_ty = &root_table.columns[op.link_column].ty;
        let target_columns: Vec<&db::Column> = target_table.columns.iter().collect();

        let rows: Vec<stmt::Value> = docs
            .iter()
            .map(|doc| {
                let link_bson = doc.get("_link").ok_or_else(|| {
                    Error::driver_operation_failed("lookup_join: result missing '_link' field")
                })?;
                let link_val = Value::from_bson(link_col_ty, link_bson)?;

                let target_doc = doc
                    .get_document("_target")
                    .map_err(Error::driver_operation_failed)?;
                let target_val = document_to_record(target_doc, target_columns.iter().copied())?;

                Ok(stmt::Value::from(stmt::ValueRecord::from_vec(vec![
                    link_val, target_val,
                ])))
            })
            .collect::<Result<_>>()?;

        Ok(rows_response(rows))
    }
}

/// Translates a via-join filter expression into a MongoDB `$match` document.
///
/// Via-join filters are always `col(slot, col_idx) = Value(parent_id)`.
/// The slot is ignored — `col_idx` directly indexes into `root_table.columns`.
fn build_match_filter(root_table: &db::Table, filter: &stmt::Expr) -> Result<Document> {
    let stmt::Expr::BinaryOp(bin) = filter else {
        return Err(Error::unsupported_feature(format!(
            "lookup_join: unexpected filter shape: {filter:#?}"
        )));
    };

    let (col_expr, val_expr) = if matches!(&*bin.lhs, stmt::Expr::Reference(_)) {
        (&bin.lhs, &bin.rhs)
    } else {
        (&bin.rhs, &bin.lhs)
    };

    let stmt::Expr::Reference(stmt::ExprReference::Column(col)) = &**col_expr else {
        return Err(Error::unsupported_feature(format!(
            "lookup_join: filter must be a column reference; got {col_expr:#?}"
        )));
    };

    let col_name = &root_table.columns[col.column].name;

    let stmt::Expr::Value(v) = &**val_expr else {
        return Err(Error::unsupported_feature(format!(
            "lookup_join: filter value must be a literal; got {val_expr:#?}"
        )));
    };

    let bson_val = Value::from(v.clone()).to_bson()?;

    let mut doc = Document::new();
    doc.insert(col_name.clone(), bson_val);
    Ok(doc)
}

/// Runs an aggregation pipeline against `collection`, sharing the active session.
async fn run_aggregate(
    collection: mongodb::Collection<Document>,
    pipeline: Vec<Document>,
    session: Option<&mut mongodb::ClientSession>,
) -> Result<Vec<Document>> {
    tracing::trace!(?pipeline, "lookup_join aggregate");

    let pipeline: Vec<Bson> = pipeline.into_iter().map(Bson::Document).collect();

    if let Some(sess) = session {
        let mut cursor = collection
            .aggregate(pipeline)
            .session(&mut *sess)
            .await
            .map_err(Error::driver_operation_failed)?;
        let mut docs = Vec::new();
        while cursor
            .advance(sess)
            .await
            .map_err(Error::driver_operation_failed)?
        {
            docs.push(
                cursor
                    .deserialize_current()
                    .map_err(Error::driver_operation_failed)?,
            );
        }
        Ok(docs)
    } else {
        let mut cursor = collection
            .aggregate(pipeline)
            .await
            .map_err(Error::driver_operation_failed)?;
        let mut docs = Vec::new();
        while cursor
            .advance()
            .await
            .map_err(Error::driver_operation_failed)?
        {
            docs.push(
                cursor
                    .deserialize_current()
                    .map_err(Error::driver_operation_failed)?,
            );
        }
        Ok(docs)
    }
}
