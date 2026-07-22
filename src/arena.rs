//! Arena allocation for lexical scope frames.
//!
//! This module previously contained `ScopeArena`, `ScopeId`, `ThunkId`, and associated
//! migration helpers (`migrate_flat_env`, `migrate_value`, `migrate_thunk_id`). Those
//! types have been removed as part of the Arc<Thunk>-direct ownership migration.
