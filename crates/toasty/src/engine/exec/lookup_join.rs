use crate::{
    Result,
    engine::exec::{Action, Exec, Output, VarId},
};
use toasty_core::{driver::operation, schema::db::TableId, stmt};

#[derive(Debug)]
pub(crate) struct LookupJoin {
    /// Optional input variable that provides parent-record arg values.
    pub input: Option<VarId>,

    /// Where to store the result.
    pub output: Output,

    /// Root-adjacent collection.
    pub root_table: TableId,

    /// Filter on `root_table` (contains `Arg::Ref` placeholders pre-substitution).
    pub filter: stmt::Expr,

    /// Index of the link column in `root_table`.
    pub link_column: usize,

    /// Lookup steps ordered root → target.
    pub steps: Vec<LookupStep>,

    /// Whether to deduplicate.
    pub distinct: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LookupStep {
    pub local_column: usize,
    pub foreign_table: TableId,
    pub foreign_column: usize,
}

impl Exec<'_> {
    pub(super) async fn action_lookup_join(&mut self, action: &LookupJoin) -> Result<()> {
        let mut filter = action.filter.clone();

        if let Some(input_var) = action.input {
            let input = self.collect_input(&[input_var]).await?;
            filter.substitute(&input);
        }

        let res = self
            .connection
            .exec(
                &self.engine.schema,
                operation::LookupJoin {
                    root_table: action.root_table,
                    filter,
                    link_column: action.link_column,
                    steps: action
                        .steps
                        .iter()
                        .map(|s| operation::LookupStep {
                            local_column: s.local_column,
                            foreign_table: s.foreign_table,
                            foreign_column: s.foreign_column,
                        })
                        .collect(),
                    distinct: action.distinct,
                }
                .into(),
            )
            .await?;

        self.vars
            .store(action.output.var, action.output.num_uses, res);

        Ok(())
    }
}

impl From<LookupJoin> for Action {
    fn from(value: LookupJoin) -> Self {
        Action::LookupJoin(value)
    }
}
