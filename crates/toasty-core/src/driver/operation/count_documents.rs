use super::Operation;
use crate::{schema::db::TableId, stmt};

/// Count the number of documents/rows matching a filter.
///
/// Sent to drivers with [`Capability::native_count`](crate::driver::Capability::native_count)
/// set to `true`. The driver counts matching documents and returns a single
/// row containing `Value::U64(count)`.
#[derive(Debug, Clone)]
pub struct CountDocuments {
    /// Table to count from.
    pub table: TableId,

    /// Optional filter expression. `None` means count all rows.
    pub filter: Option<stmt::Expr>,
}

impl From<CountDocuments> for Operation {
    fn from(value: CountDocuments) -> Self {
        Self::CountDocuments(value)
    }
}
