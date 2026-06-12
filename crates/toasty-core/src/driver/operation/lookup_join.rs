use super::Operation;
use crate::{schema::db, stmt};

/// Cross-collection join executed as an aggregation pipeline (`$lookup`).
///
/// Sent to drivers with [`Capability::native_join`](crate::driver::Capability::native_join)
/// set to `true`. The driver starts from `root_table`, applies `filter` as a
/// `$match`, performs one `$lookup` per step toward the target collection, and
/// returns one `[link_key, target_record]` pair per matched (parent, target)
/// combination. When `distinct` is `true` the driver deduplicates by
/// `(link_key, target_pk)` before returning.
#[derive(Debug, Clone)]
pub struct LookupJoin {
    /// The collection to start the aggregation from (root-adjacent in the via chain).
    pub root_table: db::TableId,

    /// Filter on `root_table` with all `Arg::Ref` values already substituted.
    ///
    /// For via joins this is always a single equality predicate
    /// `link_column = parent_id`. Column references have any slot index;
    /// `link_column` identifies the referenced column by index.
    pub filter: stmt::Expr,

    /// Index of the link column in `root_table`.
    ///
    /// The link column's value is returned as the first element of each result
    /// row, letting `NestedMerge` group children by their parent.
    pub link_column: usize,

    /// Lookup steps from `root_table` toward the target collection.
    ///
    /// `steps[0]` is the first `$lookup` from `root_table`; each subsequent
    /// step looks up from the previous step's foreign collection. The final
    /// step's `foreign_table` is the join target.
    pub steps: Vec<LookupStep>,

    /// Whether to deduplicate (mirrors `SELECT DISTINCT`).
    ///
    /// `true` for all via-include joins: the same target may be reachable
    /// through several intermediates but should appear only once per parent.
    pub distinct: bool,
}

/// A single `$lookup` step in a [`LookupJoin`].
#[derive(Debug, Clone)]
pub struct LookupStep {
    /// Column index of the FK column in the "local" collection.
    ///
    /// For `steps[0]` this is a column index in `root_table`; for `steps[N]`
    /// it is a column index in `steps[N-1].foreign_table`.
    pub local_column: usize,

    /// The collection to look up into.
    pub foreign_table: db::TableId,

    /// Column index of the reference column in `foreign_table`
    /// (typically the primary key).
    pub foreign_column: usize,
}

impl From<LookupJoin> for Operation {
    fn from(value: LookupJoin) -> Self {
        Self::LookupJoin(value)
    }
}
