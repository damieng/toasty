use toasty_core::{schema::db::TableId, stmt};

use crate::engine::{exec, mir};

/// Count documents matching an optional filter.
///
/// Emitted by the planner when a `COUNT(*)` query targets a driver with
/// [`Capability::native_count`](toasty_core::driver::Capability::native_count).
#[derive(Debug)]
pub(crate) struct CountDocuments {
    /// The table to count from.
    pub(crate) table: TableId,

    /// Optional filter expression.
    pub(crate) filter: Option<stmt::Expr>,

    /// The return type: `Type::List(Type::Record([Type::U64]))`.
    pub(crate) ty: stmt::Type,
}

impl CountDocuments {
    pub(crate) fn to_exec(
        &self,
        _logical_plan: &mir::LogicalPlan,
        node: &mir::Node,
        var_table: &mut exec::VarDecls,
    ) -> exec::CountDocuments {
        let output = var_table.register_var(node.ty().clone());
        node.var.set(Some(output));

        exec::CountDocuments {
            output: exec::Output {
                var: output,
                num_uses: node.num_uses.get(),
            },
            table: self.table,
            filter: self.filter.clone(),
        }
    }
}

impl From<CountDocuments> for mir::Node {
    fn from(value: CountDocuments) -> Self {
        mir::Operation::CountDocuments(value).into()
    }
}
