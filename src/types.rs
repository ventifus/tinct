//! Type system façade — re-exports from focused submodules.
//!
//! All existing `use crate::types::*` imports continue to work.
//!
//! Module structure:
//! - `type_def`: Core type representations (Type enum, Row, Kind, structural operations)
//! - `type_class`: Type class declarations (ClassDecl, Constraint, ClassEnv, InstanceEnv)
//! - `type_infer`: Inference machinery (InferState, TypeVarEntry, generalization)
//! - `type_normalize`: Normalization and Display implementations
//! - `type_env`: Type environments and TypeError alias (submodule via #[path])
//! - `type_unify`: Unification and substitution (submodule via #[path])

// Focused submodules (top-level for circular dependency avoidance)
pub use crate::type_class::*;
pub use crate::type_def::*;
// type_errors re-export deferred until T-1107 migration completes
// pub use crate::type_errors::*;
pub use crate::type_infer::*;

// Existing submodules (keep as-is — they use `super::*` internally)
#[path = "type_env.rs"]
mod type_env;
#[path = "type_unify.rs"]
mod type_unify;
pub use type_env::*;
pub use type_unify::*;
