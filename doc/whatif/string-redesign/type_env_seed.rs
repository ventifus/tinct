// doc/whatif/string-redesign/type_env_seed.rs
//
// Proposed Rust changes for de-primitisation.
// This file is a design stub — not compiled, not exhaustive.
// Shows what changes in src/type_def.rs and src/imports.rs (or equivalent).
//
// Changes from current code:
//   - Type::Str         deleted  (String is gone)
//   - Type::Bool        deleted  (Boolean is a tinct type in prelude)
//   - Type::Number      deleted  (Number is a typeclass in prelude)
//   - Type::Seq         deleted  (Seq is a tinct type in prelude)
//   - Type::Top renamed Type::Any (user-facing name; prevents leakage in diagnostics)
//   - Type::Unknown     kept     (sole gradual-typing compiler sentinel)
//   - Type::Never       kept     (bottom type — could move to prelude but not urgent)
//   - Type::DirCap      kept as TyConDef entry (see seed below)
//   - Type::NetCap      kept as TyConDef entry
//   - Type::ClockCap    kept as TyConDef entry
//   - Type::Handle      kept as TyConDef entry
//   - Type::Int         kept as TyConDef entry
//   - Type::Float       kept as TyConDef entry
//   - Type::Bytes       kept as TyConDef entry (raw bytes; distinct from Graphemes)

// ─── src/type_def.rs — Type enum (after changes) ────────────────────────────

pub enum Type {
    // Lattice sentinels — not nominal types, cannot be TyConDef entries
    Any,         // formerly Type::Top; sound supertype (τ <: Any for all τ)
    Unknown,     // gradual "?" type; consistency ~, not subtyping
    Never,       // bottom type; return type of raise/diverging functions

    // Resolved named types (TyCon lookup)
    TyCon(String),   // all other types: Int, Float, Bytes, DirCap, Handle, etc.

    // Structural / parametric (unchanged from current)
    TypeVar(String, u32),
    App(Box<Type>, Box<Type>),
    Function { params: Vec<(Option<String>, Type)>, ret: Box<Type>, variadic: bool, required_count: usize },
    Record(Row),
    Union(Vec<Type>),
    // ... etc.
}

// ─── Root TypeEnv seeding (replaces bypass list) ─────────────────────────────
//
// Called at startup to populate the root TypeEnv with TyConDef entries for
// every type name that was previously handled by the bypass list or by
// Type::* primitive enum variants.
//
// After this, annotation resolution routes ALL @Name lookups through the
// type-stage env or TypeEnv.tycon_defs — no bypass list, no hardcoded table.

pub fn seed_root_type_env(env: &mut TypeEnv) {
    // All Rust-backed types: primitives, capability types, and lattice sentinels.
    // Special behavior (subtyping rules for Any, consistency ~ for Unknown, etc.)
    // is enforced by is_subtype/is_consistent, not by name resolution.
    // Any and Unknown are TyConDef entries like everything else — no bypass list.
    for name in [
        "Int", "Float", "Bytes",                          // primitives
        "DirCap", "NetCap", "ClockCap", "Handle", "Url",  // capability types
        "Any", "Unknown",                                   // lattice sentinels
    ] {
        env.insert_tycon_def(name, TyConDef::rust_backed(name));
    }

    // NOT seeded here (handled by prelude's tinct declarations):
    //   Boolean, Seq, Never, Number — declared in prelude as [type ...] or [class ...]
    //   Document, Program, Expression — declared in prelude runtime section
    //   DirCapFlag, OpenFlag, etc. — declared in prelude runtime section
}

// ─── builtin-eval: return type specification ─────────────────────────────────
//
// builtin-eval evaluates a sequence of expressions and returns the value of the
// last one. Its return type is determined the same way the type checker determines
// the return type of any tinct function or document: by type-checking the final
// expression in the sequence and reading its inferred type.
//
// The return type is the type of the final dict expression — a Record with
// specific fields. This is identical to how:
//   - The type checker infers a function's return type from its body
//   - The type checker infers % from the previous document
//   - The type checker infers the result of any sequential dict chain
//
// No special machinery needed. The three-phase model already ensures the type
// checker has processed each document's expressions before builtin-eval is called
// on them. The type flows naturally:
//
//   type-checker processes document D → infers last expr type T (a Record)
//   eval-document-runtime calls builtin-eval D.expressions → T
//   eval-document-pipeline returns T (the final document's result)
//   eval-file returns T
//   include returns T
//
// Current code has `ret: Box::new(Type::Unknown)` — WRONG. Unknown propagates
// via the consistency relation and disables type checking. The correct type is
// inferred from the expressions, same as any other expression in the type checker.
// When the expressions are not yet traversed (genuinely unknown), the fallback
// is Type::Any — sound, not gradual.
//
// Also fix: src/imports.rs registers `include` with ret: Type::Unknown.
// After this change include's return type flows from builtin-eval — no separate fix needed.

// ─── Bypass list after changes ────────────────────────────────────────────────
//
// DELETED ENTIRELY.
//
// Any and Unknown are also TyConDef entries seeded at startup. Their special
// behavior (Any: τ <: Any for all τ; Unknown: consistency relation ~) is enforced
// by is_subtype/is_consistent, not by name resolution. @Any and @Unknown both
// resolve through the type-stage env → typenode_value_to_type path like all
// other type names.
//
// After this change: resolve_type_name_with_guard is deleted. The annotation
// resolver has a single unified path — type-stage env lookup → TypeEnv.tycon_defs
// fallback → error. No bypass, no hardcoded table, no special cases.
