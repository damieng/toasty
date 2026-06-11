//! Per-operation `exec_*` implementations for the MongoDB [`Connection`].
//!
//! Each submodule translates one [`Operation`](toasty_core::driver::operation)
//! into MongoDB calls. The shared read primitives (`find`, `find_paginated`)
//! and the translation helpers (`pk_in_filter`, `build_update_doc`,
//! `document_to_record`, …) live in the crate root.

mod count_documents;
mod delete_by_key;
pub(crate) mod exists;
mod find_pk_by_index;
mod get_by_key;
mod insert;
mod query_pk;
mod scan;
mod update_by_key;
