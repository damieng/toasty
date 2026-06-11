//! Behavioral tests for the `native_exists` capability.
//!
//! When `native_exists` is true the driver receives EXISTS predicates as part
//! of the filter sent to the database, rather than the engine evaluating them
//! with a separate read.  These tests verify the two key outcomes:
//!
//! 1. When the EXISTS condition passes the operation runs normally.
//! 2. When the EXISTS condition fails the operation is a no-op and returns an
//!    error — no unrelated records are modified.

use crate::prelude::*;

/// Removing a todo from the user who owns it nullifies the FK — the todo
/// continues to exist but is no longer associated with any user.
#[driver_test(
    id(ID),
    requires(native_exists),
    scenario(crate::scenarios::has_many_nullable_fk)
)]
pub async fn unlink_from_owning_user_nullifies_fk(t: &mut Test) -> Result<()> {
    let mut db = setup(t).await;

    let user = User::create()
        .todos([Todo::create().title("task")])
        .exec(&mut db)
        .await?;
    let todos: Vec<_> = user.todos().exec(&mut db).await?;
    assert_eq!(1, todos.len());

    user.todos().remove(&mut db, &todos[0]).await?;

    // The todo still exists but is no longer owned by anyone.
    let reloaded = Todo::get_by_id(&mut db, todos[0].id).await?;
    assert_none!(reloaded.user_id);

    // The user's association list is now empty.
    let remaining = user.todos().exec(&mut db).await?;
    assert!(remaining.is_empty());

    Ok(())
}

/// Attempting to unlink a todo that belongs to a different user must fail —
/// the EXISTS condition (todo.user_id == caller's id) does not hold, so the
/// operation short-circuits and the todo's FK is left unchanged.
#[driver_test(
    id(ID),
    requires(native_exists),
    scenario(crate::scenarios::has_many_nullable_fk)
)]
pub async fn unlink_from_unrelated_user_returns_error(t: &mut Test) -> Result<()> {
    let mut db = setup(t).await;

    let user1 = User::create().exec(&mut db).await?;
    let user2 = User::create()
        .todos([Todo::create().title("task")])
        .exec(&mut db)
        .await?;
    let u2_todos: Vec<_> = user2.todos().exec(&mut db).await?;

    // user1 does not own u2's todo — this must return an error.
    assert_err!(user1.todos().remove(&mut db, &u2_todos[0]).await);

    // The todo must still belong to user2 — no partial mutation occurred.
    let reloaded = Todo::get_by_id(&mut db, u2_todos[0].id).await?;
    assert_eq!(reloaded.user_id.unwrap(), user2.id);

    Ok(())
}
