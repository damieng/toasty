use crate::{
    Result,
    engine::exec::{Action, Exec, Output},
};
use toasty_core::{driver::operation, schema::db::TableId, stmt};

#[derive(Debug)]
pub(crate) struct CountDocuments {
    /// Where to store the count result.
    pub output: Output,

    /// Table to count from.
    pub table: TableId,

    /// Optional filter expression.
    pub filter: Option<stmt::Expr>,
}

impl Exec<'_> {
    pub(super) async fn action_count_documents(&mut self, action: &CountDocuments) -> Result<()> {
        let res = self
            .connection
            .exec(
                &self.engine.schema,
                operation::CountDocuments {
                    table: action.table,
                    filter: action.filter.clone(),
                }
                .into(),
            )
            .await?;

        self.vars
            .store(action.output.var, action.output.num_uses, res);

        Ok(())
    }
}

impl From<CountDocuments> for Action {
    fn from(value: CountDocuments) -> Self {
        Action::CountDocuments(value)
    }
}
