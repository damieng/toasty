//! Behavioral tests for the `use_transactions` capability.
//!
//! These verify end-state correctness (all-or-nothing) without inspecting the
//! internal operation log. They complement `tx_atomic_stmt.rs`, which asserts
//! the SQL-level transaction operations but cannot run on non-SQL drivers.

use crate::prelude::*;

/// A multi-op create (user + associated todo) should atomically commit: after
/// success both the user and the todo are present and correctly associated.
#[driver_test(
    id(ID),
    requires(use_transactions),
    scenario(crate::scenarios::has_many_belongs_to)
)]
pub async fn multi_op_create_commits_all_records(t: &mut Test) -> Result<()> {
    let mut db = setup(t).await;

    let user = User::create()
        .name("Alice")
        .todos([Todo::create().title("task")])
        .exec(&mut db)
        .await?;

    let todos = user.todos().exec(&mut db).await?;
    assert_eq!(1, todos.len());
    assert_eq!("task", todos[0].title);

    Ok(())
}

/// When the second INSERT in a create plan fails, the transaction must be
/// rolled back — the first INSERT (todo, due to the UUID Const optimisation)
/// must not persist as an orphan.
///
/// With UUID IDs the engine generates the user's UUID client-side, so
/// `todo.user_id` is a known constant and the engine inserts the Todo first.
/// A `#[unique]` constraint on `User.name` forces the subsequent User INSERT
/// to fail, triggering a rollback that removes the transiently-inserted Todo.
#[driver_test(requires(use_transactions))]
pub async fn rollback_on_second_op_failure_leaves_no_orphans(t: &mut Test) -> Result<()> {
    #[derive(Debug, toasty::Model)]
    struct User {
        #[key]
        #[auto]
        id: uuid::Uuid,

        #[unique]
        name: String,

        #[has_many]
        todos: toasty::Deferred<Vec<Todo>>,
    }

    #[derive(Debug, toasty::Model)]
    struct Todo {
        #[key]
        #[auto]
        id: uuid::Uuid,

        #[index]
        user_id: uuid::Uuid,

        #[belongs_to(key = user_id, references = id)]
        user: toasty::Deferred<User>,

        title: String,
    }

    let mut db = t.setup_db(models!(User, Todo)).await;

    // Seed the name collision — this will cause the User INSERT to fail later.
    User::create()
        .name("taken")
        .todos([Todo::create().title("seed-todo")])
        .exec(&mut db)
        .await?;

    // The todo INSERT runs first (Const optimisation) and temporarily succeeds;
    // the user INSERT then fails on the unique name constraint, triggering a
    // rollback that removes the todo as well.
    assert_err!(
        User::create()
            .name("taken")
            .todos([Todo::create().title("orphan-todo")])
            .exec(&mut db)
            .await
    );

    // Only the seeded user and todo should exist — no orphaned records.
    let users = User::all().exec(&mut db).await?;
    assert_eq!(1, users.len());
    assert_eq!("taken", users[0].name);

    let all_todos = users[0].todos().exec(&mut db).await?;
    assert_eq!(1, all_todos.len());
    assert_eq!("seed-todo", all_todos[0].title);

    Ok(())
}
