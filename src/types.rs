//! Type system façade — re-exports from focused submodules.
//!
//! All existing `use crate::types::*` imports continue to work.
//!
//! Post-S-1003: `Type`, `Row`, `Kind`, and `TypeScheme` Rust enums have been deleted.
//! Type representations now use `Arc<Value>` (TypeValue) with ctor tags from type_tags.rs.
//!
//! Module structure:
//! - `type_def`: TyConDef, TyConEnv, Variance (Type/Row/Kind enums deleted in S-1003)
//! - `type_class`: Type class declarations (ClassDecl, InstanceDecl), TypeValue alias
//! - `type_infer`: Inference machinery (InferState, InferenceContext, TypeStageData)
//! - `type_normalize`: TypeValue normalization and display helpers
//! - `type_env`: Type environments and scheme operations
//! - `type_unify`: Unification, constrain, and BAS integration

// Focused submodules (top-level for circular dependency avoidance)
pub use crate::type_class::*;
pub use crate::type_infer::*;

// Existing submodules (keep as-is — they use `super::*` internally)
#[path = "type_env.rs"]
mod type_env;
#[path = "type_unify.rs"]
mod type_unify;
pub use type_env::*;
pub use type_unify::*;
