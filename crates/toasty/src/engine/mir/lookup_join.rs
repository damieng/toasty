use indexmap::IndexSet;
use toasty_core::{schema::db, stmt};

use crate::engine::{exec, mir};

/// Cross-collection join (via `$lookup`) for NoSQL drivers with `native_join`.
#[derive(Debug)]
pub(crate) struct LookupJoin {
    /// Optional single input node whose output provides runtime arg values for
    /// the filter (the parent record ID).
    pub(crate) input: Option<mir::NodeId>,

    /// Root-adjacent collection (start of the join chain).
    pub(crate) root_table: db::TableId,

    /// Filter on `root_table` (contains `Arg::Ref` placeholders for parent values).
    pub(crate) filter: stmt::Expr,

    /// Index of the link column in `root_table`.
    pub(crate) link_column: usize,

    /// Lookup steps from root toward the target collection (root-adjacent first).
    pub(crate) steps: Vec<LookupStep>,

    /// Whether to deduplicate (mirrors `SELECT DISTINCT`).
    pub(crate) distinct: bool,

    /// Return type: `Type::List(Type::Record([link_type, target_record_type]))`.
    pub(crate) ty: stmt::Type,
}

#[derive(Debug, Clone)]
pub(crate) struct LookupStep {
    pub(crate) local_column: usize,
    pub(crate) foreign_table: db::TableId,
    pub(crate) foreign_column: usize,
}

impl LookupJoin {
    pub(crate) fn to_exec(
        &self,
        logical_plan: &mir::LogicalPlan,
        node: &mir::Node,
        var_table: &mut exec::VarDecls,
    ) -> exec::LookupJoin {
        let output = var_table.register_var(node.ty().clone());
        node.var.set(Some(output));

        let input = self.input.map(|n| {
            logical_plan[n]
                .var
                .get()
                .expect("LookupJoin input node has no var assigned")
        });

        exec::LookupJoin {
            output: exec::Output {
                var: output,
                num_uses: node.num_uses.get(),
            },
            input,
            root_table: self.root_table,
            filter: self.filter.clone(),
            link_column: self.link_column,
            steps: self
                .steps
                .iter()
                .map(|s| exec::LookupStep {
                    local_column: s.local_column,
                    foreign_table: s.foreign_table,
                    foreign_column: s.foreign_column,
                })
                .collect(),
            distinct: self.distinct,
        }
    }
}

impl From<LookupJoin> for mir::Node {
    fn from(value: LookupJoin) -> Self {
        mir::Operation::LookupJoin(value).into()
    }
}

impl From<LookupJoin> for mir::Operation {
    fn from(value: LookupJoin) -> Self {
        mir::Operation::LookupJoin(value)
    }
}

impl From<&LookupJoin> for IndexSet<mir::NodeId> {
    fn from(value: &LookupJoin) -> Self {
        value.input.into_iter().collect()
    }
}
