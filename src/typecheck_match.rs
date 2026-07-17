//! Case arm and function literal type inference.
//!
//! Match arm and function inference are implemented iteratively in `typecheck_cek.rs`:
//! - Match arms: `setup_match_arm_env` + `AfterMatchArm` continuation
//! - Function literals: `infer_fn_push_cont` + `AfterFnBody` continuation
//! - Pattern elaboration: `setup_match_arm_env` (inline)
