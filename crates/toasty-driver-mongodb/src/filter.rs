//! Translation of Toasty filter expressions into MongoDB query documents.
//!
//! This covers the predicate shapes the query engine emits for the paths the
//! `orders` example exercises (equality on an indexed field, primary-key
//! lookups) plus the common comparison, boolean, and membership operators.
//! Anything outside that set returns [`Error::unsupported_feature`] so a gap
//! surfaces as a clean error rather than a panic or silently wrong results.

use mongodb::bson::{Bson, Document};
use toasty_core::{
    Error, Result,
    schema::db,
    stmt::{self, ExprContext},
};

use crate::value::Value;

/// Translates a filter expression into a MongoDB query document.
pub(crate) fn translate_filter(
    cx: &ExprContext<'_, db::Schema>,
    expr: &stmt::Expr,
) -> Result<Document> {
    match expr {
        stmt::Expr::And(and) => {
            let parts: Vec<Bson> = and
                .operands
                .iter()
                .map(|op| translate_filter(cx, op).map(Bson::Document))
                .collect::<Result<_>>()?;
            let mut doc = Document::new();
            doc.insert("$and", parts);
            Ok(doc)
        }
        stmt::Expr::Or(or) => {
            let parts: Vec<Bson> = or
                .operands
                .iter()
                .map(|op| translate_filter(cx, op).map(Bson::Document))
                .collect::<Result<_>>()?;
            let mut doc = Document::new();
            doc.insert("$or", parts);
            Ok(doc)
        }
        stmt::Expr::BinaryOp(bin) => {
            // Identify the column-bearing side; the other side is the literal it
            // is compared against. The subject is a column reference or the
            // length of an array column.
            let (subject, value_expr, op) = match (&*bin.lhs, &*bin.rhs) {
                (stmt::Expr::Reference(_) | stmt::Expr::Length(_), _) => {
                    (&bin.lhs, &bin.rhs, bin.op)
                }
                (_, stmt::Expr::Reference(_) | stmt::Expr::Length(_)) => {
                    (&bin.rhs, &bin.lhs, flip(bin.op))
                }
                _ => {
                    return Err(Error::unsupported_feature(format!(
                        "the MongoDB driver does not support this binary predicate: {bin:#?}"
                    )));
                }
            };

            if let stmt::Expr::Length(length) = &**subject {
                return length_filter(cx, &length.expr, op, value_expr);
            }

            let mut inner = Document::new();
            inner.insert(mongo_operator(op)?, expr_to_bson(value_expr)?);

            let mut doc = Document::new();
            doc.insert(field_name(cx, subject)?, inner);
            Ok(doc)
        }
        stmt::Expr::InList(in_list) => {
            let field = field_name(cx, &in_list.expr)?;
            // Drop null operands: a NULL in a SQL `IN` list matches nothing,
            // whereas MongoDB's `$in: [null]` would match missing/null fields.
            let items: Vec<Bson> = match expr_to_bson(&in_list.list)? {
                Bson::Array(items) => items,
                other => vec![other],
            }
            .into_iter()
            .filter(|item| !matches!(item, Bson::Null))
            .collect();

            let mut inner = Document::new();
            inner.insert("$in", items);

            let mut doc = Document::new();
            doc.insert(field, inner);
            Ok(doc)
        }
        stmt::Expr::IsNull(is_null) => {
            // In MongoDB, `{ field: null }` matches both an explicit null and a
            // missing field, which is the behavior `.is_none()` expects.
            let field = field_name(cx, &is_null.expr)?;
            let mut doc = Document::new();
            doc.insert(field, Bson::Null);
            Ok(doc)
        }
        stmt::Expr::Reference(_) => {
            // A bare column reference in predicate position is a boolean column
            // tested for truth (the result of `field = true` simplification).
            let field = field_name(cx, expr)?;
            let mut doc = Document::new();
            doc.insert(field, true);
            Ok(doc)
        }
        stmt::Expr::AnyOp(any_op) if any_op.op == stmt::BinaryOp::Eq => {
            // `lhs = ANY(rhs)`. Two orientations reach the driver:
            match (&*any_op.lhs, &*any_op.rhs) {
                // `value = ANY(arrayColumn)` — does the array column contain
                // the value? Matching a MongoDB array field against a scalar
                // tests element membership. `contains`/`intersects`/
                // `is_superset` decompose into these, combined by `$or`/`$and`.
                (value, stmt::Expr::Reference(_)) => {
                    let field = field_name(cx, &any_op.rhs)?;
                    let mut doc = Document::new();
                    doc.insert(field, expr_to_bson(value)?);
                    Ok(doc)
                }
                // `scalarColumn = ANY(literalArray)` — column IN list.
                (stmt::Expr::Reference(_), list) => {
                    let field = field_name(cx, &any_op.lhs)?;
                    let items = match expr_to_bson(list)? {
                        Bson::Array(items) => items,
                        other => vec![other],
                    };
                    let mut inner = Document::new();
                    inner.insert("$in", items);
                    let mut doc = Document::new();
                    doc.insert(field, inner);
                    Ok(doc)
                }
                _ => Err(Error::unsupported_feature(format!(
                    "the MongoDB driver does not support these ANY operands: {any_op:#?}"
                ))),
            }
        }
        stmt::Expr::StartsWith(starts_with) => {
            let field = field_name(cx, &starts_with.expr)?;
            let prefix = match expr_to_bson(&starts_with.prefix)? {
                Bson::String(prefix) => prefix,
                other => {
                    return Err(Error::unsupported_feature(format!(
                        "the MongoDB driver requires a string starts_with prefix, got {other:?}"
                    )));
                }
            };

            // Anchor a case-sensitive prefix match. The prefix is a literal, so
            // its regex metacharacters are escaped.
            let mut inner = Document::new();
            inner.insert("$regex", format!("^{}", regex_escape(&prefix)));

            let mut doc = Document::new();
            doc.insert(field, inner);
            Ok(doc)
        }
        stmt::Expr::Between(between) => {
            let field = field_name(cx, &between.expr)?;

            let mut inner = Document::new();
            inner.insert("$gte", expr_to_bson(&between.low)?);
            inner.insert("$lte", expr_to_bson(&between.high)?);

            let mut doc = Document::new();
            doc.insert(field, inner);
            Ok(doc)
        }
        stmt::Expr::Not(not) => {
            // MongoDB's `$not` only negates a single field's operator
            // expression, so it cannot wrap an arbitrary translated document.
            // `$nor` with one operand is the general top-level negation:
            // `$nor: [P]` matches exactly the documents that do not match `P`.
            let inner = translate_filter(cx, &not.expr)?;
            let mut doc = Document::new();
            doc.insert("$nor", vec![Bson::Document(inner)]);
            Ok(doc)
        }
        _ => Err(Error::unsupported_feature(format!(
            "the MongoDB driver does not support this filter expression: {expr:#?}"
        ))),
    }
}

/// Builds a filter on the length of an array column (`LEN(field) <op> n`).
///
/// Equality uses MongoDB's `$size`, which matches an exact element count. Other
/// comparisons need an `$expr` evaluating `$size` against the bound value.
fn length_filter(
    cx: &ExprContext<'_, db::Schema>,
    field_expr: &stmt::Expr,
    op: stmt::BinaryOp,
    value_expr: &stmt::Expr,
) -> Result<Document> {
    let field = field_name(cx, field_expr)?;
    let value = expr_to_bson(value_expr)?;

    let mut doc = Document::new();
    if op == stmt::BinaryOp::Eq {
        let mut inner = Document::new();
        inner.insert("$size", value);
        doc.insert(field, inner);
    } else {
        let mut size = Document::new();
        size.insert("$size", format!("${field}"));
        let mut cmp = Document::new();
        cmp.insert(mongo_operator(op)?, vec![Bson::Document(size), value]);
        doc.insert("$expr", cmp);
    }
    Ok(doc)
}

/// Escapes regex metacharacters so a literal string can be embedded in a
/// `$regex` pattern.
fn regex_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(
            ch,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Resolves a column reference expression to its stored field name.
fn field_name(cx: &ExprContext<'_, db::Schema>, expr: &stmt::Expr) -> Result<String> {
    match expr {
        stmt::Expr::Reference(reference) => Ok(cx
            .resolve_expr_reference(reference)
            .as_column_unwrap()
            .name
            .clone()),
        _ => Err(Error::unsupported_feature(format!(
            "the MongoDB driver expected a column reference, got {expr:#?}"
        ))),
    }
}

/// Converts a literal value expression into a BSON value.
fn expr_to_bson(expr: &stmt::Expr) -> Result<Bson> {
    match expr {
        stmt::Expr::Value(value) => Value::from(value.clone()).to_bson(),
        _ => Err(Error::unsupported_feature(format!(
            "the MongoDB driver expected a literal value, got {expr:#?}"
        ))),
    }
}

/// Maps a Toasty binary operator to its MongoDB query operator keyword.
fn mongo_operator(op: stmt::BinaryOp) -> Result<&'static str> {
    match op {
        stmt::BinaryOp::Eq => Ok("$eq"),
        stmt::BinaryOp::Ne => Ok("$ne"),
        stmt::BinaryOp::Gt => Ok("$gt"),
        stmt::BinaryOp::Ge => Ok("$gte"),
        stmt::BinaryOp::Lt => Ok("$lt"),
        stmt::BinaryOp::Le => Ok("$lte"),
        other => Err(Error::unsupported_feature(format!(
            "the MongoDB driver does not support this binary operator in a filter: {other:?}"
        ))),
    }
}

/// Flips a comparison operator for the case where the column reference is on
/// the right-hand side (e.g. `5 < col` becomes `col > 5`).
fn flip(op: stmt::BinaryOp) -> stmt::BinaryOp {
    match op {
        stmt::BinaryOp::Gt => stmt::BinaryOp::Lt,
        stmt::BinaryOp::Ge => stmt::BinaryOp::Le,
        stmt::BinaryOp::Lt => stmt::BinaryOp::Gt,
        stmt::BinaryOp::Le => stmt::BinaryOp::Ge,
        same => same,
    }
}
