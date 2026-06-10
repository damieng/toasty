//! Translation of Toasty filter expressions into MongoDB query documents.
//!
//! This covers the predicate shapes the query engine emits for the paths the
//! `orders` example exercises (equality on an indexed field, primary-key
//! lookups) plus the common comparison, boolean, and membership operators.
//! Anything outside that set hits a `todo!` so gaps surface loudly rather than
//! silently returning wrong results.

use mongodb::bson::{Bson, Document};
use toasty_core::{
    schema::db,
    stmt::{self, ExprContext},
};

use crate::value::Value;

/// Translates a filter expression into a MongoDB query document.
pub(crate) fn translate_filter(cx: &ExprContext<'_, db::Schema>, expr: &stmt::Expr) -> Document {
    match expr {
        stmt::Expr::And(and) => {
            let parts: Vec<Bson> = and
                .operands
                .iter()
                .map(|op| Bson::Document(translate_filter(cx, op)))
                .collect();
            let mut doc = Document::new();
            doc.insert("$and", parts);
            doc
        }
        stmt::Expr::Or(or) => {
            let parts: Vec<Bson> = or
                .operands
                .iter()
                .map(|op| Bson::Document(translate_filter(cx, op)))
                .collect();
            let mut doc = Document::new();
            doc.insert("$or", parts);
            doc
        }
        stmt::Expr::BinaryOp(bin) => {
            // Identify which side is the column reference; the other side is the
            // literal it is compared against.
            let (field, value_expr, op) = match (&*bin.lhs, &*bin.rhs) {
                (stmt::Expr::Reference(_), _) => (field_name(cx, &bin.lhs), &bin.rhs, bin.op),
                (_, stmt::Expr::Reference(_)) => (field_name(cx, &bin.rhs), &bin.lhs, flip(bin.op)),
                _ => todo!("binary op without a column reference: {bin:#?}"),
            };

            let mut inner = Document::new();
            inner.insert(mongo_operator(op), expr_to_bson(value_expr));

            let mut doc = Document::new();
            doc.insert(field, inner);
            doc
        }
        stmt::Expr::InList(in_list) => {
            let field = field_name(cx, &in_list.expr);
            // Drop null operands: a NULL in a SQL `IN` list matches nothing,
            // whereas MongoDB's `$in: [null]` would match missing/null fields.
            let items: Vec<Bson> = match expr_to_bson(&in_list.list) {
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
            doc
        }
        stmt::Expr::IsNull(is_null) => {
            // In MongoDB, `{ field: null }` matches both an explicit null and a
            // missing field, which is the behavior `.is_none()` expects.
            let field = field_name(cx, &is_null.expr);
            let mut doc = Document::new();
            doc.insert(field, Bson::Null);
            doc
        }
        stmt::Expr::Reference(_) => {
            // A bare column reference in predicate position is a boolean column
            // tested for truth (the result of `field = true` simplification).
            let field = field_name(cx, expr);
            let mut doc = Document::new();
            doc.insert(field, true);
            doc
        }
        stmt::Expr::StartsWith(starts_with) => {
            let field = field_name(cx, &starts_with.expr);
            let prefix = match expr_to_bson(&starts_with.prefix) {
                Bson::String(prefix) => prefix,
                other => todo!("starts_with prefix must be a string, got {other:?}"),
            };

            // Anchor a case-sensitive prefix match. The prefix is a literal, so
            // its regex metacharacters are escaped.
            let mut inner = Document::new();
            inner.insert("$regex", format!("^{}", regex_escape(&prefix)));

            let mut doc = Document::new();
            doc.insert(field, inner);
            doc
        }
        stmt::Expr::Between(between) => {
            let field = field_name(cx, &between.expr);

            let mut inner = Document::new();
            inner.insert("$gte", expr_to_bson(&between.low));
            inner.insert("$lte", expr_to_bson(&between.high));

            let mut doc = Document::new();
            doc.insert(field, inner);
            doc
        }
        stmt::Expr::Not(not) => {
            // MongoDB's `$not` only negates a single field's operator
            // expression, so it cannot wrap an arbitrary translated document.
            // `$nor` with one operand is the general top-level negation:
            // `$nor: [P]` matches exactly the documents that do not match `P`.
            let inner = translate_filter(cx, &not.expr);
            let mut doc = Document::new();
            doc.insert("$nor", vec![Bson::Document(inner)]);
            doc
        }
        _ => todo!("unsupported filter expr: {expr:#?}"),
    }
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
fn field_name(cx: &ExprContext<'_, db::Schema>, expr: &stmt::Expr) -> String {
    match expr {
        stmt::Expr::Reference(reference) => cx
            .resolve_expr_reference(reference)
            .as_column_unwrap()
            .name
            .clone(),
        _ => todo!("expected a column reference, got {expr:#?}"),
    }
}

/// Converts a literal value expression into a BSON value.
fn expr_to_bson(expr: &stmt::Expr) -> Bson {
    match expr {
        stmt::Expr::Value(value) => Value::from(value.clone()).to_bson(),
        _ => todo!("expected a literal value, got {expr:#?}"),
    }
}

/// Maps a Toasty binary operator to its MongoDB query operator keyword.
fn mongo_operator(op: stmt::BinaryOp) -> &'static str {
    match op {
        stmt::BinaryOp::Eq => "$eq",
        stmt::BinaryOp::Ne => "$ne",
        stmt::BinaryOp::Gt => "$gt",
        stmt::BinaryOp::Ge => "$gte",
        stmt::BinaryOp::Lt => "$lt",
        stmt::BinaryOp::Le => "$lte",
        other => todo!("unsupported binary operator in filter: {other:?}"),
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
