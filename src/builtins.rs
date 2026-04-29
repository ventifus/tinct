//! Rust-native builtin functions for the LLT language.
//!
//! All builtins follow the `BuiltinFn` signature:
//! `fn(BuiltinArgs) -> EvalResult<Rc<Thunk>>`
//!
//! ## Builtin groups
//!
//! **Arithmetic:** `+`, `-`, `*`, `/` (with auto-promotion table)
//! **Comparison:** `=`, `<` (cross-type Int/Float comparison allowed)
//! **Control:** `if` (selective materialization -- only the chosen branch is forced)
//! **Dict primitives:** `keys`, `length`, `merge`, `append`
//! **Strings:** `str`, `split`, `replace`, `upper`, `lower`, `trim`
//! **Numeric:** `floor`, `round`
//! **Parsing:** `to-int`, `to-float`
//! **Evaluation control:** `eval`, `error`, `try`, `apply`
//! **Type introspection:** `type-of`
//! **I/O:** `from-json`, `include`
//! **Sequences:** `seq`, `head`, `tail`, `collect`, `seq?`, `range`, `repeat`, `cycle`, `iterate`, `unfold`, `take`, `map`, `filter`, `drop`, `reduce`, `join`, `concat`

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::error::{ArityBound, ErrorKind, EvalError, EvalResult};
// Circular module dependency: this module imports `invoke_function` and `materialize` from eval.rs.
// eval.rs calls builtins via function pointers stored in `Value::Builtin`.
// This bidirectional dependency is safe because neither module's initialization depends on the other.
use crate::eval::{invoke_function, materialize, CallContext, MAX_EVAL_DEPTH};
use crate::value::{BuiltinArgs, BuiltinFn, Environment, Key, Thunk, Value};

/// Maximum collection size for $collect (1,000,000 elements).
/// Prevents memory exhaustion from infinite sequences without $take.
const MAX_COLLECT_SIZE: usize = 1_000_000;

/// Maximum string output size for string output builtins (`$replace`, `$upper`, `$lower`, `$join`) (64 MB).
/// Prevents memory exhaustion from adversarial inputs or replacement patterns.
const MAX_STRING_SIZE: usize = 64 * 1024 * 1024;

/// Maximum number of parts produced by `$split` (1,000,000 elements).
/// Prevents heap exhaustion from splitting by empty separator or small patterns.
const MAX_SPLIT_PARTS: usize = 1_000_000;

fn ok_val(v: Value) -> EvalResult<Rc<Thunk>> {
    Ok(Rc::new(Thunk::new_materialized(v, Span::origin())))
}

/// Maximum file size for reading LLT files: 10 MB.
pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Helper: materialize a single positional argument, enforcing exact arity of 1
/// and rejecting named arguments. Used by many single-arg builtins.
fn expect_one_arg(
    name: &str,
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    ctx: &Rc<crate::eval::EvalContext>,
    depth: usize,
    call_span: Span,
) -> EvalResult<Value> {
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    if !named.is_empty() {
        return Err(EvalError::named_arg_rejected(name.to_string(), call_span).into());
    }
    materialize(&args[0], None, ctx, depth)
}

/// Helper: check that an f64 value is within the representable range of i64
/// before casting. Returns an error if the value is non-finite or would saturate.
fn checked_f64_to_i64(name: &str, f: f64, call_span: Span) -> EvalResult<i64> {
    if !f.is_finite() {
        return Err(EvalError::float_not_finite(name.to_string(), f, call_span).into());
    }
    if f < (i64::MIN as f64) || f >= (i64::MAX as f64) {
        return Err(EvalError::integer_overflow(
            format!("{name}: {f} is out of i64 range"),
            call_span,
        )
        .into());
    }
    Ok(f as i64)
}

/// Two-operand numeric pair after auto-promotion.
///
/// Used by arithmetic builtins to implement the promotion table:
/// - Int op Int   -> Ints(a, b)
/// - Int op Float -> Floats(a as f64, b)
/// - Float op Int -> Floats(a, b as f64)
/// - Float op Float -> Floats(a, b)
enum NumPair {
    Ints(i64, i64),
    Floats(f64, f64),
}

/// Extract two numeric operands with auto-promotion, enforcing arity == 2.
fn extract_num_pair(
    args: &[Rc<Thunk>],
    ctx: &Rc<crate::eval::EvalContext>,
    depth: usize,
    call_span: Span,
) -> EvalResult<NumPair> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    let left = materialize(&args[0], None, ctx, depth)?;
    let right = materialize(&args[1], None, ctx, depth)?;
    match (&left, &right) {
        (Value::Int(a), Value::Int(b)) => Ok(NumPair::Ints(*a, *b)),
        (Value::Int(a), Value::Float(b)) => Ok(NumPair::Floats(*a as f64, *b)),
        (Value::Float(a), Value::Int(b)) => Ok(NumPair::Floats(*a, *b as f64)),
        (Value::Float(a), Value::Float(b)) => Ok(NumPair::Floats(*a, *b)),
        _ => Err(EvalError::type_mismatch(
            "Int or Float",
            &format!("{} and {}", left.type_name(), right.type_name()),
            call_span,
        )
        .into()),
    }
}

/// Stringify a single materialized value for `str` builtin.
///
/// - Int -> decimal representation (e.g. `42`)
/// - Float -> decimal representation (e.g. `3.14`)
/// - String -> the string itself (no quotes)
/// - Bool -> `"true"` / `"false"`
/// - Dict, Function, Builtin -> delegated to `Value::Display`
fn stringify(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => format!("{other}"),
    }
}

/// Helper: require that a materialized value is a Dict, returning the inner IndexMap.
fn require_dict(name: &str, value: Value, call_span: Span) -> EvalResult<IndexMap<Key, Rc<Thunk>>> {
    match value {
        Value::Dict(map) => Ok(map),
        other => Err(EvalError::type_mismatch_ctx(
            name.to_string(),
            "Dict",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// Helper: require that a materialized value is a String, returning the inner String.
fn require_string(name: &str, value: Value, call_span: Span) -> EvalResult<String> {
    match value {
        Value::String(s) => Ok(s),
        other => Err(EvalError::type_mismatch_ctx(
            name.to_string(),
            "String",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// Helper: reject named arguments for multi-arg builtins that don't accept them.
fn reject_named(
    name: &str,
    named: &IndexMap<String, Rc<Thunk>>,
    call_span: Span,
) -> EvalResult<()> {
    if !named.is_empty() {
        return Err(EvalError::named_arg_rejected(name.to_string(), call_span).into());
    }
    Ok(())
}

/// `+`: Addition with auto-promotion. Int + Int -> Int, any Float operand -> Float.
/// Inherently materializing: must extract numeric values to compute sum.
fn builtin_add(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("+", named, call_span)?;
    match extract_num_pair(args, &ctx, depth, call_span)? {
        NumPair::Ints(a, b) => a
            .checked_add(b)
            .map(|n| ok_val(Value::Int(n)))
            .unwrap_or_else(|| Err(EvalError::integer_overflow("+".to_string(), call_span).into())),
        NumPair::Floats(a, b) => ok_val(Value::Float(a + b)),
    }
}

/// `-`: Subtraction with auto-promotion. Int - Int -> Int, any Float operand -> Float.
/// Inherently materializing: must extract numeric values to compute difference.
fn builtin_sub(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("-", named, call_span)?;
    match extract_num_pair(args, &ctx, depth, call_span)? {
        NumPair::Ints(a, b) => a
            .checked_sub(b)
            .map(|n| ok_val(Value::Int(n)))
            .unwrap_or_else(|| Err(EvalError::integer_overflow("-".to_string(), call_span).into())),
        NumPair::Floats(a, b) => ok_val(Value::Float(a - b)),
    }
}

/// `*`: Multiplication with auto-promotion. Int * Int -> Int, any Float operand -> Float.
/// Inherently materializing: must extract numeric values to compute product.
fn builtin_mul(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("*", named, call_span)?;
    match extract_num_pair(args, &ctx, depth, call_span)? {
        NumPair::Ints(a, b) => a
            .checked_mul(b)
            .map(|n| ok_val(Value::Int(n)))
            .unwrap_or_else(|| Err(EvalError::integer_overflow("*".to_string(), call_span).into())),
        NumPair::Floats(a, b) => ok_val(Value::Float(a * b)),
    }
}

/// `/`: Float division. ALWAYS returns Float, even for Int / Int. Division by zero produces an error.
/// Inherently materializing: must extract numeric values to compute quotient.
fn builtin_div_float(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("/", named, call_span)?;
    match extract_num_pair(args, &ctx, depth, call_span)? {
        NumPair::Ints(a, b) => {
            if b == 0 {
                Err(EvalError::division_by_zero("/".to_string(), call_span).into())
            } else {
                ok_val(Value::Float(a as f64 / b as f64))
            }
        }
        NumPair::Floats(a, b) => {
            if b == 0.0 {
                Err(EvalError::division_by_zero("/".to_string(), call_span).into())
            } else {
                ok_val(Value::Float(a / b))
            }
        }
    }
}

/// `=`: Equality comparison.
/// Works on Int, Float, String, Bool. Cross-type Int/Float comparison
/// promotes Int to Float. Dict/Function/Builtin are never equal (returns false,
/// not an error).
/// Inherently materializing: must inspect values to determine equality.
fn builtin_eq(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("=", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    let left = materialize(&args[0], None, &ctx, depth)?;
    let right = materialize(&args[1], None, &ctx, depth)?;

    let result = match (&left, &right) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        // Cross-type: Int/Float promotion
        (Value::Int(a), Value::Float(b)) => (*a as f64) == *b,
        (Value::Float(a), Value::Int(b)) => *a == (*b as f64),
        // Dict, Function, Builtin are never equal
        _ => false,
    };
    ok_val(Value::Bool(result))
}

/// `<`: Less-than comparison.
/// Works on Int, Float, String, Bool. Cross-type Int/Float comparison promotes
/// Int to Float. String comparison is lexicographic. Bool: false < true.
/// Incompatible types (e.g. Int vs String) produce a type error.
/// Inherently materializing: must inspect values to determine ordering.
fn builtin_lt(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("<", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    let left = materialize(&args[0], None, &ctx, depth)?;
    let right = materialize(&args[1], None, &ctx, depth)?;

    let result = match (&left, &right) {
        (Value::Int(a), Value::Int(b)) => a < b,
        (Value::Float(a), Value::Float(b)) => a < b,
        (Value::String(a), Value::String(b)) => a < b,
        (Value::Bool(a), Value::Bool(b)) => !a && *b, // false < true
        // Cross-type: Int/Float promotion
        (Value::Int(a), Value::Float(b)) => (*a as f64) < *b,
        (Value::Float(a), Value::Int(b)) => *a < (*b as f64),
        _ => {
            return Err(EvalError::type_mismatch(
                "Int, Float, String, or Bool (same or compatible types)",
                &format!("{} and {}", left.type_name(), right.type_name()),
                call_span,
            )
            .into());
        }
    };
    ok_val(Value::Bool(result))
}

/// `if`: Conditional with selective materialization.
///
/// Takes 3 positional args: condition, then-branch, else-branch.
/// Materializes ONLY the condition, then materializes ONLY the chosen branch.
/// The unchosen branch's thunk is never materialized -- this preserves lazy
/// semantics because `eval_call` wraps each arg as a thunk before calling.
fn builtin_if(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("if", named, call_span)?;
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }

    // Materialize only the condition
    let condition = materialize(&args[0], None, &ctx, depth)?;

    match condition {
        Value::Bool(true) => Ok(Rc::clone(&args[1])),
        Value::Bool(false) => Ok(Rc::clone(&args[2])),
        _ => Err(EvalError::type_mismatch("Bool", condition.type_name(), call_span).into()),
    }
}

/// `keys`: Takes 1 arg (a Dict). Returns a Dict with integer keys `0..n`
/// mapping to the key values (Int keys become Int values, String keys become
/// String values). Insertion order is preserved.
/// Inherently materializing: must access IndexMap to enumerate keys.
fn builtin_keys(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("keys", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let val = materialize(&args[0], None, &ctx, depth)?;
    let map = require_dict("keys", val, call_span)?;

    let origin = call_span;
    let mut result = IndexMap::with_capacity(map.len());
    for (i, (key, _)) in map.iter().enumerate() {
        let key_value = match key {
            Key::Int(n) => Value::Int(*n),
            Key::String(s) => Value::String(s.clone()),
        };
        result.insert(
            Key::Int(i64::try_from(i).expect("collection too large")),
            Rc::new(Thunk::new_materialized(key_value, origin)),
        );
    }
    ok_val(Value::Dict(result))
}

/// `length`: Takes 1 arg (a Dict). Returns an Int with the number of entries.
/// Inherently materializing: must access IndexMap to count entries.
fn builtin_length(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("length", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let val = materialize(&args[0], None, &ctx, depth)?;
    let map = require_dict("length", val, call_span)?;
    ok_val(Value::Int(map.len() as i64))
}

/// `merge`: Takes 2 args (both Dicts). Returns a right-biased merge: all
/// entries from the left dict, then all entries from the right dict. If both
/// have the same key, right wins. Values remain as thunks (no materialization
/// of values).
fn builtin_merge(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("merge", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    let left_val = materialize(&args[0], None, &ctx, depth)?;
    let right_val = materialize(&args[1], None, &ctx, depth)?;
    let left = require_dict("merge", left_val, call_span)?;
    let right = require_dict("merge", right_val, call_span)?;

    // TODO: left+right over-allocates when keys overlap; max under-allocates when they don't.
    // Investigate a better heuristic (e.g., left + right/2) if merge becomes a hot path.
    let mut result = IndexMap::with_capacity(left.len() + right.len());
    // Insert all left entries
    for (key, thunk) in &left {
        result.insert(key.clone(), Rc::clone(thunk));
    }
    // Insert all right entries (overwrites on collision)
    for (key, thunk) in &right {
        result.insert(key.clone(), Rc::clone(thunk));
    }
    ok_val(Value::Dict(result))
}

/// `append`: Takes 2 args: a Dict and any value. Returns a new dict with the
/// value inserted at the next integer key (one past the current maximum integer
/// key, or 0 for empty dicts / dicts with no integer keys).
///
/// This is O(n) for the clone but O(1) amortized for the insert itself,
/// compared to the old LLT `append` which did a full `merge` (copying the
/// entire accumulator into a new dict via two-dict iteration).
fn builtin_append(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("append", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    let dict_val = materialize(&args[0], None, &ctx, depth)?;
    let mut map = require_dict("append", dict_val, call_span)?;

    // Compute the next integer key: max existing int key + 1, or 0 if none.
    let next_key = map
        .keys()
        .filter_map(|k| match k {
            Key::Int(n) => Some(*n),
            _ => None,
        })
        .max()
        .map(|max| {
            max.checked_add(1)
                .ok_or_else(|| EvalError::integer_overflow("append".to_string(), call_span))
        })
        .transpose()?
        .unwrap_or(0);

    map.insert(Key::Int(next_key), Rc::clone(&args[1]));
    ok_val(Value::Dict(map))
}

/// `str`: Variadic string concatenation and toString.
///
/// Materializes each argument and concatenates their string representations.
/// With zero args, returns an empty string.
/// Inherently materializing: must inspect values to convert to string representation.
fn builtin_str(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("str", named, call_span)?;
    let mut result = String::new();
    for arg in args {
        let val = materialize(arg, None, &ctx, depth)?;
        result.push_str(&stringify(&val));
    }
    ok_val(Value::String(result))
}

/// `split`: Split a string by a separator.
///
/// Takes 2 args: `separator` (String), `input` (String).
/// Returns a Dict with integer keys `0..n` mapping to the split substrings.
/// Inherently materializing: must inspect string content to split into substrings.
fn builtin_split(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("split", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    let sep_val = materialize(&args[0], None, &ctx, depth)?;
    let input_val = materialize(&args[1], None, &ctx, depth)?;

    let sep = require_string("split", sep_val, call_span)?;
    let input = require_string("split", input_val, call_span)?;

    // Bound allocation before the guard fires: take at most MAX_SPLIT_PARTS + 1 entries
    // so that adversarial input (e.g., splitting a large string by empty separator) cannot
    // heap-exhaust the process before the check.
    let parts: Vec<&str> = input
        .split(sep.as_str())
        .take(MAX_SPLIT_PARTS + 1)
        .collect();
    if parts.len() > MAX_SPLIT_PARTS {
        return Err(EvalError::resource_limit_exceeded(
            format!("$split: input produces more than {MAX_SPLIT_PARTS} parts"),
            call_span,
        )
        .into());
    }
    let mut map = IndexMap::with_capacity(parts.len());
    for (i, part) in parts.into_iter().enumerate() {
        map.insert(
            Key::Int(i64::try_from(i).map_err(|_| {
                EvalError::resource_limit_exceeded(
                    "$split: result index too large".to_string(),
                    call_span,
                )
            })?),
            Rc::new(Thunk::new_materialized(
                Value::String(part.to_string()),
                call_span,
            )),
        );
    }
    ok_val(Value::Dict(map))
}

/// `replace`: Replace all occurrences of a pattern in a string.
///
/// Takes 3 args: `pattern` (String), `replacement` (String), `input` (String).
/// Returns a new String with all occurrences of `pattern` replaced by `replacement`.
/// Inherently materializing: must inspect string content to find and replace patterns.
fn builtin_replace(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("replace", named, call_span)?;
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    let pattern_val = materialize(&args[0], None, &ctx, depth)?;
    let replacement_val = materialize(&args[1], None, &ctx, depth)?;
    let input_val = materialize(&args[2], None, &ctx, depth)?;

    let pattern = require_string("replace", pattern_val, call_span)?;
    let replacement = require_string("replace", replacement_val, call_span)?;
    let input = require_string("replace", input_val, call_span)?;

    // Pre-check output size to prevent memory exhaustion.
    // Empty pattern inserts replacement between every character.
    let match_count = if pattern.is_empty() {
        input.chars().count() + 1
    } else {
        input.matches(pattern.as_str()).count()
    };

    // output_len = input.len() - (match_count * pattern.len()) + (match_count * replacement.len())
    let removed_bytes = match_count.saturating_mul(pattern.len());
    let added_bytes = match_count.saturating_mul(replacement.len());
    let output_len = input
        .len()
        .saturating_sub(removed_bytes)
        .saturating_add(added_bytes);

    if output_len > MAX_STRING_SIZE {
        return Err(EvalError::resource_limit_exceeded(
            format!(
                "replace: output would exceed {} MB limit ({} bytes)",
                MAX_STRING_SIZE / (1024 * 1024),
                output_len
            ),
            call_span,
        )
        .into());
    }

    // Fast-path: if there are no matches, return the input unchanged
    if match_count == 0 {
        return ok_val(Value::String(input.into()));
    }

    ok_val(Value::String(input.replace(pattern.as_str(), &replacement)))
}

/// `upper`: Convert a string to uppercase. Takes 1 arg (String).
/// Inherently materializing: must inspect string content to convert case.
fn builtin_upper(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("upper", args, named, &ctx, depth, call_span)?;
    let s = require_string("upper", val, call_span)?;
    // Fast-path: if input already exceeds the limit, output cannot be smaller (Unicode expansion only grows).
    if s.len() > MAX_STRING_SIZE {
        return Err(EvalError::resource_limit_exceeded(
            format!(
                "upper: input exceeds {} MB limit ({} bytes)",
                MAX_STRING_SIZE / (1024 * 1024),
                s.len()
            ),
            call_span,
        )
        .into());
    }
    let result = s.to_uppercase();
    // Post-conversion guard for Unicode expansion (e.g., ß → SS).
    if result.len() > MAX_STRING_SIZE {
        return Err(EvalError::resource_limit_exceeded(
            format!(
                "upper: output would exceed {} MB limit ({} bytes)",
                MAX_STRING_SIZE / (1024 * 1024),
                result.len()
            ),
            call_span,
        )
        .into());
    }

    ok_val(Value::String(result))
}

/// `lower`: Convert a string to lowercase. Takes 1 arg (String).
/// Inherently materializing: must inspect string content to convert case.
fn builtin_lower(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("lower", args, named, &ctx, depth, call_span)?;
    let s = require_string("lower", val, call_span)?;
    // Fast-path: if input already exceeds the limit, output cannot be smaller (Unicode expansion only grows).
    if s.len() > MAX_STRING_SIZE {
        return Err(EvalError::resource_limit_exceeded(
            format!(
                "lower: input exceeds {} MB limit ({} bytes)",
                MAX_STRING_SIZE / (1024 * 1024),
                s.len()
            ),
            call_span,
        )
        .into());
    }
    let result = s.to_lowercase();
    // Post-conversion guard for Unicode expansion (e.g., İ → i\u{307}).
    if result.len() > MAX_STRING_SIZE {
        return Err(EvalError::resource_limit_exceeded(
            format!(
                "lower: output would exceed {} MB limit ({} bytes)",
                MAX_STRING_SIZE / (1024 * 1024),
                result.len()
            ),
            call_span,
        )
        .into());
    }

    ok_val(Value::String(result))
}

/// `trim`: Remove leading and trailing whitespace from a string.
///
/// Takes 1 arg (String). Returns the trimmed string.
/// Inherently materializing: must inspect string content to identify and remove whitespace.
fn builtin_trim(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("trim", args, named, &ctx, depth, call_span)?;
    let s = require_string("trim", val, call_span)?;
    ok_val(Value::String(s.trim().to_string()))
}

/// Shared helper for `floor` and `round`: takes a builtin name and an f64->f64
/// operation, materializes one numeric arg, and applies the operation to floats.
///
/// - Int input: returned unchanged.
/// - Float input: checks for NaN/Infinity, applies `op`, converts to `i64`.
/// - Non-numeric input: type error.
fn float_to_int_builtin(
    name: &str,
    op: fn(f64) -> f64,
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    ctx: &Rc<crate::eval::EvalContext>,
    depth: usize,
    call_span: Span,
) -> EvalResult<Rc<Thunk>> {
    let val = expect_one_arg(name, args, named, ctx, depth, call_span)?;
    match val {
        Value::Int(n) => ok_val(Value::Int(n)),
        Value::Float(f) => {
            if !f.is_finite() {
                return Err(EvalError::float_not_finite(name.to_string(), f, call_span).into());
            }
            ok_val(Value::Int(checked_f64_to_i64(name, op(f), call_span)?))
        }
        other => Err(EvalError::type_mismatch("Int or Float", other.type_name(), call_span).into()),
    }
}

/// `floor`: Takes 1 numeric arg (Int or Float). Returns Int.
///
/// - Int input: returned unchanged.
/// - Float input: applies `f64::floor()` then converts to `i64`.
/// - NaN or Infinity: errors (cannot convert to Int).
/// - Non-numeric input: type error.
/// Inherently materializing: must inspect numeric value to convert/round.
fn builtin_floor(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    float_to_int_builtin("floor", f64::floor, args, named, &ctx, depth, call_span)
}

/// `round`: Takes 1 numeric arg (Int or Float). Returns Int.
///
/// - Int input: returned unchanged.
/// - Float input: applies `f64::round()` (half-away-from-zero) then converts to `i64`.
/// - NaN or Infinity: errors (cannot convert to Int).
/// - Non-numeric input: type error.
/// Inherently materializing: must inspect numeric value to convert/round.
fn builtin_round(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    float_to_int_builtin("round", f64::round, args, named, &ctx, depth, call_span)
}

/// `to-int`: STRING-TO-NUMBER PARSING ONLY. Takes 1 String arg.
///
/// Parses the string as an integer via `str::parse::<i64>()`. Returns Int.
/// Does NOT accept numeric inputs -- it is a string parser, not a type converter.
/// Inherently materializing: must inspect string content to parse integer value.
fn builtin_to_int(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("to-int", args, named, &ctx, depth, call_span)?;
    let s = require_string("to-int", val, call_span)?;
    match s.parse::<i64>() {
        Ok(n) => ok_val(Value::Int(n)),
        Err(_) => {
            Err(
                EvalError::parse_conversion("to-int".to_string(), s.clone(), "Int", call_span)
                    .into(),
            )
        }
    }
}

/// `to-float`: STRING-TO-NUMBER PARSING ONLY. Takes 1 String arg.
///
/// Parses the string as a float via `str::parse::<f64>()`. Returns Float.
/// Does NOT accept numeric inputs -- it is a string parser, not a type converter.
/// Inherently materializing: must inspect string content to parse float value.
fn builtin_to_float(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("to-float", args, named, &ctx, depth, call_span)?;
    let s = require_string("to-float", val, call_span)?;
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() => ok_val(Value::Float(f)),
        Ok(f) => Err(EvalError::float_not_finite("to-float".to_string(), f, call_span).into()),
        Err(_) => {
            Err(
                EvalError::parse_conversion("to-float".to_string(), s.clone(), "Float", call_span)
                    .into(),
            )
        }
    }
}

/// Recursively materialize a value: if it is a Dict, materialize every entry
/// value and recurse into nested dicts.
/// `eval`: takes 1 arg, deep-forces all thunks recursively.
/// Delegates to [`crate::eval::deep_materialize`].
/// Inherently materializing: deep-forces all thunks by definition.
fn builtin_eval(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("eval", args, named, &ctx, depth, call_span)?;
    let deep = crate::eval::deep_materialize(&val, &ctx, depth)?;
    ok_val(deep)
}

/// `error`: takes 1 arg (String message), always raises.
/// Inherently materializing: constructs concrete error value.
fn builtin_error(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("error", args, named, &ctx, depth, call_span)?;
    let msg = require_string("error", val, call_span)?;
    Err(EvalError::user_error(msg.to_string(), call_span).into())
}

/// `try`: takes 1 arg (a zero-arg Function). Calls it. Returns `[ok: value]`
/// on success or `[err: message]` on failure.
/// Inherently materializing: must materialize body to catch errors.
fn builtin_try(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("try", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let func_val = materialize(&args[0], None, &ctx, depth)?;

    let call_result = match func_val {
        Value::Function {
            params,
            body,
            env: closure_env,
            ..
        } => {
            if !params.is_empty() {
                return Err(EvalError::arity_mismatch(0, params.len(), call_span).into());
            }
            // Evaluate the body in the closure's environment
            let body_thunk = Rc::new(Thunk::new_unevaluated(
                Rc::clone(&body),
                Rc::clone(&closure_env),
                Rc::clone(&ctx),
                body.span,
            ));
            materialize(&body_thunk, None, &ctx, depth)
        }
        Value::Builtin { func, .. } => {
            let builtin_args = BuiltinArgs {
                args: &[],
                named: &IndexMap::new(),
                depth,
                call_span,
                ctx: Rc::clone(&ctx),
            };
            match func(builtin_args) {
                Ok(result_thunk) => materialize(&result_thunk, None, &ctx, depth),
                Err(e) => Err(e),
            }
        }
        _ => {
            return Err(
                EvalError::type_mismatch("Function", func_val.type_name(), call_span).into(),
            )
        }
    };

    match call_result {
        Ok(value) => {
            let mut result = IndexMap::with_capacity(1);
            result.insert(
                Key::String("ok".to_string()),
                Rc::new(Thunk::new_materialized(value, call_span)),
            );
            ok_val(Value::Dict(result))
        }
        Err(e) => {
            // Resource limit errors (DepthExceeded, ResourceLimitExceeded) must not be catchable by user code.
            // Re-raise instead of converting to err dict.
            if !e.kind.is_catchable() {
                return Err(e);
            }
            let mut result = IndexMap::with_capacity(1);
            result.insert(
                Key::String("err".to_string()),
                Rc::new(Thunk::new_materialized(
                    Value::String(e.message()),
                    call_span,
                )),
            );
            ok_val(Value::Dict(result))
        }
    }
}

/// `apply`: takes 2 args (function, dict/list). Spreads the dict's values as
/// positional arguments to the function call.
///
/// For user-defined functions, delegates to `eval::invoke_function` so that
/// default values, named args, and variadics are handled identically to `call`.
fn builtin_apply(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("apply", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    let func_val = materialize(&args[0], None, &ctx, depth)?;
    let args_val = materialize(&args[1], None, &ctx, depth)?;

    let arg_dict = match args_val {
        Value::Dict(map) => map,
        _ => return Err(EvalError::type_mismatch("Dict", args_val.type_name(), call_span).into()),
    };

    // Split dict entries: integer-keyed → positional, string-keyed → named
    let mut int_entries: Vec<(i64, Rc<Thunk>)> = Vec::with_capacity(arg_dict.len());
    let mut named_args: IndexMap<String, Rc<Thunk>> = IndexMap::with_capacity(arg_dict.len());
    for (key, thunk) in &arg_dict {
        match key {
            Key::Int(n) => int_entries.push((*n, Rc::clone(thunk))),
            Key::String(s) => {
                named_args.insert(s.clone(), Rc::clone(thunk));
            }
        }
    }
    int_entries.sort_by_key(|(k, _)| *k);
    let positional: Vec<Rc<Thunk>> = int_entries.into_iter().map(|(_, v)| v).collect();

    match func_val {
        Value::Function {
            params,
            body,
            env: closure_env,
            ..
        } => invoke_function(&CallContext {
            params: &params,
            body: &body,
            closure_env: &closure_env,
            positional: &positional,
            named: &named_args,
            default_env: &closure_env,
            ctx: &ctx,
            call_span,
            depth,
            origin: Cow::Borrowed("call $apply"),
        }),
        Value::Builtin { func, .. } => {
            let builtin_args = BuiltinArgs {
                args: &positional,
                named: &named_args,
                depth,
                call_span,
                ctx: Rc::clone(&ctx),
            };
            func(builtin_args)
        }
        _ => Err(EvalError::type_mismatch("Function", func_val.type_name(), call_span).into()),
    }
}

/// `type-of`: takes 1 arg, materializes it, returns the type name.
/// Both `Function` and `Builtin` return "Function" (from the user's perspective).
/// Inherently materializing: must inspect value variant to determine type.
fn builtin_type_of(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("type-of", args, named, &ctx, depth, call_span)?;
    let name = match val.type_name() {
        "Builtin" => "Function",
        other => other,
    };
    ok_val(Value::String(name.to_string()))
}

/// Convert a `serde_json::Value` into an LLT `Value`.
///
/// JSON null maps to an empty dict, arrays map to integer-keyed dicts,
/// and objects map to string-keyed dicts. Numbers are converted to `Int`
/// when they fit in i64, otherwise `Float`.
pub fn json_to_value(json: &serde_json::Value, depth: usize, span: Span) -> EvalResult<Rc<Thunk>> {
    if depth > MAX_EVAL_DEPTH {
        return Err(EvalError::json_depth_exceeded(MAX_EVAL_DEPTH, span).into());
    }
    match json {
        serde_json::Value::Null => ok_val(Value::Dict(IndexMap::new())),
        serde_json::Value::Bool(b) => ok_val(Value::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ok_val(Value::Int(i))
            } else if let Some(f) = n.as_f64() {
                ok_val(Value::Float(f))
            } else {
                // Unreachable with default serde_json: as_f64() covers all
                // non-i64 numbers. Return error instead of panicking.
                Err(EvalError::json_range(span).into())
            }
        }
        serde_json::Value::String(s) => ok_val(Value::String(s.clone())),
        serde_json::Value::Array(arr) => {
            if arr.len() > MAX_COLLECT_SIZE {
                return Err(EvalError::resource_limit_exceeded(
                    format!(
                        "from-json: array exceeds maximum collection size ({})",
                        MAX_COLLECT_SIZE
                    ),
                    span,
                )
                .into());
            }
            let mut map = IndexMap::with_capacity(arr.len());
            for (i, item) in arr.iter().enumerate() {
                let thunk = json_to_value(item, depth + 1, span)?;
                map.insert(
                    Key::Int(i64::try_from(i).expect("collection too large")),
                    thunk,
                );
            }
            ok_val(Value::Dict(map))
        }
        serde_json::Value::Object(obj) => {
            if obj.len() > MAX_COLLECT_SIZE {
                return Err(EvalError::resource_limit_exceeded(
                    format!(
                        "from-json: object exceeds maximum collection size ({})",
                        MAX_COLLECT_SIZE
                    ),
                    span,
                )
                .into());
            }
            let mut map = IndexMap::with_capacity(obj.len());
            for (k, v) in obj {
                let thunk = json_to_value(v, depth + 1, span)?;
                map.insert(Key::String(k.clone()), thunk);
            }
            ok_val(Value::Dict(map))
        }
    }
}

/// `from-json`: takes 1 arg (String containing JSON), parses into LLT value.
/// Inherently materializing: must parse entire JSON string to construct value.
fn builtin_from_json(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("from-json", args, named, &ctx, depth, call_span)?;
    let json_str = require_string("from-json", val, call_span)?;
    let parsed: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| EvalError::json_parse(e.to_string(), call_span))?;
    json_to_value(&parsed, depth, call_span)
}

/// `include`: takes 1 arg (String file path), evaluates the file, returns its result.
///
/// Path resolution: relative paths are resolved against the including file's
/// directory. Absolute paths are used as-is. Cycle detection prevents A→B→A
/// circular includes. The included file gets an empty `$$` and sees the stdlib
/// environment but NOT the caller's scope.
fn builtin_include(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Check if filesystem access is disabled before doing anything else.
    if ctx.config.no_fs {
        return Err(EvalError::include_forbidden(call_span).into());
    }

    let val = expect_one_arg("include", args, named, &ctx, depth, call_span)?;
    let file_path_str = require_string("include", val, call_span)?;

    // Resolve the path: relative to base_dir, or absolute as-is.
    let raw_path = std::path::Path::new(&file_path_str);
    let base_dir = &ctx.config.base_dir;
    let resolved = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        base_dir.join(raw_path)
    };

    // Canonicalize to detect cycles and normalize the path.
    let canonical = resolved.canonicalize().map_err(|e| {
        EvalError::include_io_error(resolved.display().to_string(), e.to_string(), call_span)
    })?;

    // Check cache first: if we've already evaluated this file, return the cached thunk.
    if let Some(cached) = ctx.state.borrow().include_cache.get(&canonical) {
        return Ok(Rc::clone(cached));
    }

    // Cycle detection.
    if ctx.state.borrow().include_guard.contains(&canonical) {
        return Err(EvalError::include_cycle(canonical.display().to_string(), call_span).into());
    }

    // Check file size.
    let metadata = std::fs::metadata(&canonical).map_err(|e| {
        EvalError::include_io_error(canonical.display().to_string(), e.to_string(), call_span)
    })?;
    if metadata.len() > MAX_FILE_SIZE {
        return Err(EvalError::include_file_too_large(
            canonical.display().to_string(),
            metadata.len(),
            MAX_FILE_SIZE,
            call_span,
        )
        .into());
    }

    // Read the file.
    let source = std::fs::read_to_string(&canonical).map_err(|e| {
        EvalError::include_io_error(canonical.display().to_string(), e.to_string(), call_span)
    })?;

    // Parse.
    let mut file = crate::parser::parse(&source).map_err(|e| {
        EvalError::include_parse_failed(canonical.display().to_string(), e.to_string(), call_span)
    })?;

    // Desugar $_ implicit lambdas (pre-typecheck and pre-eval AST transformation).
    crate::desugar::desugar_file(&mut file.node);

    // Add to include guard before recursing.
    ctx.state
        .borrow_mut()
        .include_guard
        .insert(canonical.clone());

    // Create new context for the included file with its directory as base_dir.
    let included_file_dir = canonical
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/"))
        .to_path_buf();

    // Create a new EvalContext with the included file's directory.
    let included_ctx = ctx.with_base_dir(included_file_dir);

    let stdlib_env = Rc::clone(&ctx.config.stdlib_env);

    // Evaluate the included file with empty $$ and the stdlib env.
    let eval_result = crate::eval::eval_file(&file.node, stdlib_env, &included_ctx, depth + 1);

    // Restore the base_dir and remove from include guard regardless of success/failure.
    // Note: base_dir is in config which is immutable, so we don't need to restore it.
    // We only need to remove from include_guard.
    let cleanup = || {
        ctx.state.borrow_mut().include_guard.remove(&canonical);
    };

    match eval_result {
        Ok(thunk) => {
            // Eagerly materialize: the include guard is only valid while
            // the current file's canonical path is in the set. Returning
            // a lazy thunk would defer evaluation past the guard removal.
            let val = match crate::eval::materialize(&thunk, None, &included_ctx, depth + 1) {
                Ok(v) => {
                    cleanup();
                    v
                }
                Err(e) => {
                    cleanup();
                    return Err(e);
                }
            };
            // Preserve the span from the included file's root expression
            let result_thunk = Rc::new(Thunk::new_materialized(val, thunk.span));

            // Cache the result thunk for future includes of this file.
            ctx.state
                .borrow_mut()
                .include_cache
                .insert(canonical.clone(), Rc::clone(&result_thunk));

            Ok(result_thunk)
        }
        Err(e) => {
            cleanup();
            Err(e)
        }
    }
}

/// `seq`: Low-level cons constructor for lazy linked-list sequences.
///
/// Creates a `Seq` with the given head and tail. Both args remain as thunks
/// (fully lazy, no materialization). The tail is NOT validated eagerly -- if it
/// eventually materializes to a non-Seq/non-empty-dict, that's an error at
/// materialization time, not construction time.
fn builtin_seq(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ..
    } = ctx_arg;
    reject_named("seq", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    ok_val(Value::Seq {
        head: Rc::clone(&args[0]),
        tail: Rc::clone(&args[1]),
    })
}

/// `head`: Extract the first element of a sequence.
///
/// Materializes the argument to verify it's a Seq, then returns the head thunk
/// directly (lazy -- the head is not materialized). Empty dict (terminal value)
/// produces a specific error message.
fn builtin_head(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("head", args, named, &ctx, depth, call_span)?;
    match val {
        Value::Seq { head, .. } => Ok(Rc::clone(&head)),
        Value::Dict(ref map) if map.is_empty() => {
            Err(EvalError::empty_collection("head".to_string(), call_span).into())
        }
        other => Err(EvalError::type_mismatch_ctx(
            "head".to_string(),
            "Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// `tail`: Extract the rest of a sequence.
///
/// Materializes the argument to verify it's a Seq, then returns the tail thunk
/// directly (lazy -- the tail is not materialized). Empty dict (terminal value)
/// produces a specific error message.
fn builtin_tail(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("tail", args, named, &ctx, depth, call_span)?;
    match val {
        Value::Seq { tail, .. } => Ok(Rc::clone(&tail)),
        Value::Dict(ref map) if map.is_empty() => {
            Err(EvalError::empty_collection("tail".to_string(), call_span).into())
        }
        other => Err(EvalError::type_mismatch_ctx(
            "tail".to_string(),
            "Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// `collect`: Materialize a Seq into a dict with integer keys.
///
/// Iterates through the sequence spine, collecting head thunks into an IndexMap
/// with keys 0, 1, 2, ... Head elements remain as thunks (lazy). Each tail is materialized to check
/// if it's another Seq or the terminal value (empty dict). Terminal condition:
/// tail materializes to an empty dict (Dict with 0 entries). If tail is anything
/// other than Seq or empty dict, error. Empty dict as input returns empty dict.
fn builtin_collect(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("collect", args, named, &ctx, depth, call_span)?;

    // Handle empty dict (terminal value) as input
    if let Value::Dict(ref d) = val {
        if d.is_empty() {
            return ok_val(Value::Dict(IndexMap::new()));
        }
    }

    if !matches!(val, Value::Seq { .. }) {
        return Err(EvalError::type_mismatch_ctx(
            "collect".to_string(),
            "Seq",
            val.type_name(),
            call_span,
        )
        .into());
    }

    let mut map = IndexMap::new();
    let mut index = 0i64;
    let mut current = val;

    loop {
        match current {
            Value::Seq { head, tail } => {
                // Insert head thunk (not materialized -- stay lazy)
                map.insert(Key::Int(index), Rc::clone(&head));
                index = index
                    .checked_add(1)
                    .ok_or_else(|| EvalError::integer_overflow("collect".to_string(), call_span))?;

                // Check collection size limit
                if index as usize >= MAX_COLLECT_SIZE {
                    return Err(EvalError::resource_limit_exceeded(
                        format!(
                            "collect: exceeded maximum collection size ({}). Use $take to limit infinite sequences before collecting.",
                            MAX_COLLECT_SIZE
                        ),
                        call_span,
                    )
                    .into());
                }

                // Materialize tail to check if we should continue
                current = materialize(&tail, None, &ctx, depth)?;
            }
            Value::Dict(ref d) if d.is_empty() => {
                // Terminal: empty dict
                break;
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "collect".to_string(),
                    "Seq or empty dict",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        }
    }

    ok_val(Value::Dict(map))
}

/// `seq?`: Type predicate for sequences.
///
/// Returns true if the argument materializes to a Seq, false otherwise.
fn builtin_seq_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("seq?", args, named, &ctx, depth, call_span)?;
    ok_val(Value::Bool(matches!(val, Value::Seq { .. })))
}

/// `range`: Sequence of integers from start to end (exclusive), or infinite.
///
/// - `[call $range start]` → infinite Seq: start, start+1, start+2, ...
/// - `[call $range start end]` → finite Seq: start, start+1, ..., end-1
///   (empty if start >= end)
///
/// Both args must be Int. Uses checked_add for overflow detection.
fn builtin_range(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("range", named, call_span)?;
    if args.len() != 1 && args.len() != 2 {
        return Err(EvalError {
            kind: ErrorKind::ArityMismatch {
                expected: ArityBound::Range(1, 2),
                got: args.len(),
            },
            definition_span: call_span,
            materialization_span: None,
            stack: Vec::new(),
        }
        .into());
    }

    let start = materialize(&args[0], None, &ctx, depth)?;
    let start_int = match start {
        Value::Int(n) => n,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "range".to_string(),
                "Int",
                other.type_name(),
                call_span,
            )
            .into())
        }
    };

    if args.len() == 1 {
        // Infinite range: [start, start+1, start+2, ...]
        let next_start = start_int
            .checked_add(1)
            .ok_or_else(|| EvalError::integer_overflow("range".to_string(), call_span))?;
        let head = ok_val(Value::Int(start_int))?;
        let tail_args = vec![ok_val(Value::Int(next_start))?];
        let tail = Rc::new(Thunk::new_pending_builtin(
            builtin_range,
            tail_args,
            IndexMap::new(),
            depth + 1,
            call_span,
            Cow::Borrowed("call $range"),
            Rc::clone(&ctx),
        ));
        ok_val(Value::Seq { head, tail })
    } else {
        // Finite range: [start, start+1, ..., end-1]
        let end = materialize(&args[1], None, &ctx, depth)?;
        let end_int = match end {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "range".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        if start_int >= end_int {
            // Empty range
            ok_val(Value::Dict(IndexMap::new()))
        } else {
            let next_start = start_int
                .checked_add(1)
                .ok_or_else(|| EvalError::integer_overflow("range".to_string(), call_span))?;
            let head = ok_val(Value::Int(start_int))?;
            let tail_args = vec![
                ok_val(Value::Int(next_start))?,
                ok_val(Value::Int(end_int))?,
            ];
            let tail = Rc::new(Thunk::new_pending_builtin(
                builtin_range,
                tail_args,
                IndexMap::new(),
                depth + 1,
                call_span,
                Cow::Borrowed("call $range"),
                Rc::clone(&ctx),
            ));
            ok_val(Value::Seq { head, tail })
        }
    }
}

/// `repeat`: Infinite sequence of a repeated value.
///
/// `[call $repeat val]` → infinite Seq: val, val, val, ...
///
/// The value is kept as a thunk (fully lazy — never materialized).
fn builtin_repeat(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    reject_named("repeat", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }

    let head = Rc::clone(&args[0]);
    let tail_args = vec![Rc::clone(&args[0])];
    let tail = Rc::new(Thunk::new_pending_builtin(
        builtin_repeat,
        tail_args,
        IndexMap::new(),
        depth + 1,
        call_span,
        Cow::Borrowed("call $repeat"),
        Rc::clone(&ctx),
    ));
    ok_val(Value::Seq { head, tail })
}

/// Internal helper for `cycle`: produces the next element in the cycle.
///
/// Takes (dict_thunk, index_thunk) where dict is the original collection to cycle
/// through and index is the current position (wrapped modulo length).
fn builtin_cycle_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("cycle_step", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let dict = materialize(&args[0], None, &ctx, depth)?;
    let map = match &dict {
        Value::Dict(m) => m,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "cycle_step".to_string(),
                "Dict",
                other.type_name(),
                call_span,
            )
            .into())
        }
    };

    let idx = materialize(&args[1], None, &ctx, depth)?;
    let idx_int = match idx {
        Value::Int(i) => i,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "cycle_step".to_string(),
                "Int",
                other.type_name(),
                call_span,
            )
            .into())
        }
    };

    if map.is_empty() {
        return Err(EvalError::empty_collection("cycle".to_string(), call_span).into());
    }

    let len = map.len() as i64;
    let current_idx = idx_int % len;
    let next_idx = (idx_int + 1) % len;

    // Get the value at current_idx
    let head = map
        .get_index(current_idx as usize)
        .map(|(_, v)| Rc::clone(v))
        .ok_or_else(|| {
            EvalError::internal("cycle_step: index out of bounds".to_string(), call_span)
        })?;

    // Create tail as PendingBuiltin for next step
    let tail_args = vec![Rc::clone(&args[0]), ok_val(Value::Int(next_idx))?];
    let tail = Rc::new(Thunk::new_pending_builtin(
        builtin_cycle_step,
        tail_args,
        IndexMap::new(),
        depth + 1,
        call_span,
        Cow::Borrowed("call $cycle"),
        Rc::clone(&ctx),
    ));

    ok_val(Value::Seq { head, tail })
}

/// `cycle`: Infinite sequence cycling through entries of a dict.
///
/// `[call $cycle xs]` → infinite Seq cycling through entries of xs by position.
///
/// Materializes xs to verify it's a non-empty Dict, then delegates to
/// `cycle_step` helper for lazy iteration.
fn builtin_cycle(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("cycle", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }

    let val = materialize(&args[0], None, &ctx, depth)?;
    match val {
        Value::Dict(ref map) => {
            if map.is_empty() {
                return Err(EvalError::empty_collection("cycle".to_string(), call_span).into());
            }
            // Start cycling from index 0
            builtin_cycle_step(BuiltinArgs {
                args: &[Rc::clone(&args[0]), ok_val(Value::Int(0))?],
                named: &IndexMap::new(),
                depth,
                call_span,
                ctx: Rc::clone(&ctx),
            })
        }
        other => Err(EvalError::type_mismatch_ctx(
            "cycle".to_string(),
            "Dict",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// `iterate`: Infinite sequence of iterated function applications.
///
/// `[call $iterate $f $x]` → infinite Seq: x, f(x), f(f(x)), ...
///
/// Both f and x are kept as thunks (fully lazy). The tail contains a PendingCall
/// for f(x), wrapped in a PendingBuiltin for the next iterate step.
fn builtin_iterate(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("iterate", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let f = Rc::clone(&args[0]);
    let x = Rc::clone(&args[1]);

    // head = x (lazy)
    let head = Rc::clone(&x);

    // Create f(x) as PendingCall
    let f_of_x = Rc::new(Thunk::new_pending_call(
        Rc::clone(&f),
        vec![Rc::clone(&x)],
        IndexMap::new(),
        call_span,
        call_span,
        Cow::Borrowed("iterate"),
        Rc::clone(&ctx),
    ));

    // tail = iterate(f, f(x))
    let tail_args = vec![Rc::clone(&f), f_of_x];
    let tail = Rc::new(Thunk::new_pending_builtin(
        builtin_iterate,
        tail_args,
        IndexMap::new(),
        depth + 1,
        call_span,
        Cow::Borrowed("call $iterate"),
        Rc::clone(&ctx),
    ));

    ok_val(Value::Seq { head, tail })
}

/// Internal helper for `unfold`: performs one unfold step.
///
/// Takes (step_function, seed) and calls step(seed), which should return either:
/// - A 2-element dict [value next_seed] to continue
/// - An empty dict [] to terminate
fn builtin_unfold_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("unfold_step", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let step = Rc::clone(&args[0]);
    let seed = Rc::clone(&args[1]);

    // Call step(seed) as PendingCall, then materialize it
    let step_result_thunk = Rc::new(Thunk::new_pending_call(
        step.clone(),
        vec![seed],
        IndexMap::new(),
        call_span,
        call_span,
        Cow::Borrowed("unfold"),
        Rc::clone(&ctx),
    ));
    let step_result = materialize(&step_result_thunk, None, &ctx, depth)?;

    match step_result {
        Value::Dict(ref map) if map.is_empty() => {
            // Termination: return empty dict
            ok_val(Value::Dict(IndexMap::new()))
        }
        Value::Dict(ref map) if map.len() >= 2 => {
            // Extract first two values (ignore keys)
            let mut iter = map.values();
            let value = Rc::clone(iter.next().unwrap());
            let next_seed = Rc::clone(iter.next().unwrap());

            // head = value (lazy)
            let head = value;

            // tail = unfold_step(step, next_seed)
            let tail_args = vec![step, next_seed];
            let tail = Rc::new(Thunk::new_pending_builtin(
                builtin_unfold_step,
                tail_args,
                IndexMap::new(),
                depth + 1,
                call_span,
                Cow::Borrowed("call $unfold"),
                Rc::clone(&ctx),
            ));

            ok_val(Value::Seq { head, tail })
        }
        Value::Dict(ref map) => Err(EvalError::internal(
            format!(
                "unfold: step function must return dict with 2+ entries or empty dict, got {} entries",
                map.len()
            ),
            call_span,
        )
        .into()),
        other => Err(
            EvalError::type_mismatch_ctx("unfold".to_string(), "Dict", other.type_name(), call_span).into(),
        ),
    }
}

/// `unfold`: Generate a sequence from a step function and seed.
///
/// `[call $unfold $step $seed]` → Seq where step(seed) returns [value next_seed]
/// or [] to stop.
///
/// Fully lazy — the step function is not called until the result is materialized.
fn builtin_unfold(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("unfold", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    // Return PendingBuiltin wrapping unfold_step — fully lazy
    let tail_args = vec![Rc::clone(&args[0]), Rc::clone(&args[1])];
    let result = Rc::new(Thunk::new_pending_builtin(
        builtin_unfold_step,
        tail_args,
        IndexMap::new(),
        depth,
        call_span,
        Cow::Borrowed("call $unfold"),
        Rc::clone(&ctx),
    ));
    Ok(result)
}

/// `map`: Apply a function to every element of a dict or sequence.
///
/// - For Dict: applies f to each value, preserving keys. Values are lazy (PendingCall thunks).
/// - For Seq: applies f to each element, returning a lazy Seq.
///
/// Args: (f, xs)
fn builtin_map(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("map", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let f_thunk = Rc::clone(&args[0]);
    let xs = materialize(&args[1], None, &ctx, depth)?;

    match xs {
        Value::Dict(ref map) => {
            // Dict path: create PendingCall thunks for each value
            let mut new_map = IndexMap::with_capacity(map.len());
            for (key, value_thunk) in map {
                let pending_call = Rc::new(Thunk::new_pending_call(
                    Rc::clone(&f_thunk),
                    vec![Rc::clone(value_thunk)],
                    IndexMap::new(),
                    call_span,
                    value_thunk.span,
                    Cow::Owned(format!("map {}", key)),
                    Rc::clone(&ctx),
                ));
                new_map.insert(key.clone(), pending_call);
            }
            ok_val(Value::Dict(new_map))
        }
        Value::Seq { head, tail } => {
            // Seq path: head = f(head), tail = map(f, tail)
            let new_head = Rc::new(Thunk::new_pending_call(
                Rc::clone(&f_thunk),
                vec![Rc::clone(&head)],
                IndexMap::new(),
                call_span,
                head.span,
                Cow::Borrowed("map head"),
                Rc::clone(&ctx),
            ));
            let tail_args = vec![Rc::clone(&f_thunk), Rc::clone(&tail)];
            let new_tail = Rc::new(Thunk::new_pending_builtin(
                builtin_map,
                tail_args,
                IndexMap::new(),
                depth + 1,
                call_span,
                Cow::Borrowed("call $map"),
                Rc::clone(&ctx),
            ));
            ok_val(Value::Seq {
                head: new_head,
                tail: new_tail,
            })
        }
        other => Err(EvalError::type_mismatch_ctx(
            "map".to_string(),
            "Dict or Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// `filter`: Keep only elements where the predicate returns true.
///
/// - For Dict: evaluates pred for each value, returns Seq of values that pass.
/// - For Seq: evaluates pred for each element, returns lazy Seq of passing elements.
///
/// Args: (pred, xs)
fn builtin_filter(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("filter", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let pred_thunk = Rc::clone(&args[0]);
    let xs = materialize(&args[1], None, &ctx, depth)?;

    match xs {
        Value::Dict(ref map) => {
            // Dict path: iterate entries by key order, building a Seq of values
            // that pass the predicate
            let keys: Vec<Key> = map.keys().cloned().collect();

            if keys.is_empty() {
                return ok_val(Value::Dict(IndexMap::new()));
            }

            // Wrap materialized values in Materialized-state thunks so the
            // step helper's materialize() calls are O(1) by construction
            let dict_thunk = Rc::new(Thunk::new_materialized(Value::Dict(map.clone()), call_span));
            let mut keys_map = IndexMap::with_capacity(keys.len());
            for (i, k) in keys.into_iter().enumerate() {
                let key_value = match k {
                    Key::Int(n) => Value::Int(n),
                    Key::String(s) => Value::String(s),
                };
                keys_map.insert(
                    Key::Int(i64::try_from(i).expect("collection too large")),
                    Rc::new(Thunk::new_materialized(key_value, call_span)),
                );
            }
            let keys_thunk = Rc::new(Thunk::new_materialized(Value::Dict(keys_map), call_span));
            let idx_thunk = ok_val(Value::Int(0))?;

            let filter_args = vec![Rc::clone(&pred_thunk), dict_thunk, keys_thunk, idx_thunk];

            let result_thunk = Rc::new(Thunk::new_pending_builtin(
                builtin_filter_dict_step,
                filter_args,
                IndexMap::new(),
                depth,
                call_span,
                Cow::Borrowed("call $filter"),
                Rc::clone(&ctx),
            ));
            Ok(result_thunk)
        }
        Value::Seq { head: _, tail: _ } => {
            // Seq path: lazy filter
            let filter_args = vec![Rc::clone(&pred_thunk), Rc::clone(&args[1])];
            let result_thunk = Rc::new(Thunk::new_pending_builtin(
                builtin_filter_seq_step,
                filter_args,
                IndexMap::new(),
                depth + 1,
                call_span,
                Cow::Borrowed("call $filter"),
                Rc::clone(&ctx),
            ));
            Ok(result_thunk)
        }
        other => Err(EvalError::type_mismatch_ctx(
            "filter".to_string(),
            "Dict or Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// Helper for filter on Dict: iterates through dict entries, building a Seq.
///
/// Args: (pred, dict, keys, idx)
fn builtin_filter_dict_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        depth,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    let pred_thunk = Rc::clone(&args[0]);
    let dict_thunk = Rc::clone(&args[1]);
    let keys_thunk = Rc::clone(&args[2]);
    let idx = materialize(&args[3], None, &ctx, depth)?;

    let idx_int = match idx {
        Value::Int(i) => i,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "filter-dict-step".to_string(),
                "Int",
                other.type_name(),
                call_span,
            )
            .into())
        }
    };

    let keys = materialize(&keys_thunk, None, &ctx, depth)?;
    let keys_map = match keys {
        Value::Dict(ref m) => m,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "filter-dict-step".to_string(),
                "Dict",
                other.type_name(),
                call_span,
            )
            .into())
        }
    };

    // Check if we've reached the end
    if idx_int >= keys_map.len() as i64 {
        return ok_val(Value::Dict(IndexMap::new()));
    }

    // Get the current key
    let key_value = match keys_map.get(&Key::Int(idx_int)) {
        Some(thunk) => materialize(thunk, None, &ctx, depth)?,
        None => {
            return Err(EvalError::internal(
                format!("filter-dict-step: key at index {} not found", idx_int),
                call_span,
            )
            .into())
        }
    };

    let current_key = match key_value {
        Value::Int(n) => Key::Int(n),
        Value::String(s) => Key::String(s),
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "filter-dict-step".to_string(),
                "Int or String",
                other.type_name(),
                call_span,
            )
            .into())
        }
    };

    // Get the value from the dict
    let dict = materialize(&dict_thunk, None, &ctx, depth)?;
    let dict_map = match dict {
        Value::Dict(ref m) => m,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "filter-dict-step".to_string(),
                "Dict",
                other.type_name(),
                call_span,
            )
            .into())
        }
    };

    let value_thunk = match dict_map.get(&current_key) {
        Some(v) => Rc::clone(v),
        None => {
            return Err(EvalError::internal(
                format!("filter-dict-step: key {} not found in dict", current_key),
                call_span,
            )
            .into())
        }
    };

    // Apply predicate
    let pred_call = Rc::new(Thunk::new_pending_call(
        Rc::clone(&pred_thunk),
        vec![Rc::clone(&value_thunk)],
        IndexMap::new(),
        call_span,
        value_thunk.span,
        Cow::Owned(format!("filter-dict pred {}", current_key)),
        Rc::clone(&ctx),
    ));
    let pred_result = materialize(&pred_call, None, &ctx, depth)?;

    let passes = match pred_result {
        Value::Bool(b) => b,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "filter".to_string(),
                "Bool",
                other.type_name(),
                call_span,
            )
            .into())
        }
    };

    // Build tail: filter-dict-step with idx+1
    let next_idx_thunk = ok_val(Value::Int(idx_int + 1))?;
    let tail_args = vec![
        Rc::clone(&pred_thunk),
        dict_thunk,
        keys_thunk,
        next_idx_thunk,
    ];
    let tail = Rc::new(Thunk::new_pending_builtin(
        builtin_filter_dict_step,
        tail_args,
        IndexMap::new(),
        depth,
        call_span,
        Cow::Borrowed("call $filter"),
        Rc::clone(&ctx),
    ));

    if passes {
        // Include this value in the result
        ok_val(Value::Seq {
            head: value_thunk,
            tail,
        })
    } else {
        // Skip this value, continue to next
        Ok(tail)
    }
}

/// Helper for filter on Seq: lazily filters sequence elements.
///
/// Args: (pred, seq)
fn builtin_filter_seq_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        depth,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    let pred_thunk = Rc::clone(&args[0]);
    let seq_thunk = Rc::clone(&args[1]);
    let seq = materialize(&seq_thunk, None, &ctx, depth)?;

    match seq {
        Value::Dict(_) => {
            // End of sequence
            ok_val(Value::Dict(IndexMap::new()))
        }
        Value::Seq { head, tail } => {
            // Apply predicate to head
            let pred_call = Rc::new(Thunk::new_pending_call(
                Rc::clone(&pred_thunk),
                vec![Rc::clone(&head)],
                IndexMap::new(),
                call_span,
                head.span,
                Cow::Borrowed("filter-seq pred"),
                Rc::clone(&ctx),
            ));
            let pred_result = materialize(&pred_call, None, &ctx, depth)?;

            let passes = match pred_result {
                Value::Bool(b) => b,
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "filter".to_string(),
                        "Bool",
                        other.type_name(),
                        call_span,
                    )
                    .into())
                }
            };

            if passes {
                // Include this element
                let tail_args = vec![Rc::clone(&pred_thunk), tail];
                let new_tail = Rc::new(Thunk::new_pending_builtin(
                    builtin_filter_seq_step,
                    tail_args,
                    IndexMap::new(),
                    depth + 1,
                    call_span,
                    Cow::Borrowed("call $filter"),
                    Rc::clone(&ctx),
                ));
                ok_val(Value::Seq {
                    head,
                    tail: new_tail,
                })
            } else {
                // Skip this element, recurse to next
                let tail_args = vec![Rc::clone(&pred_thunk), tail];
                let next_thunk = Rc::new(Thunk::new_pending_builtin(
                    builtin_filter_seq_step,
                    tail_args,
                    IndexMap::new(),
                    depth + 1,
                    call_span,
                    Cow::Borrowed("call $filter"),
                    Rc::clone(&ctx),
                ));
                Ok(next_thunk)
            }
        }
        other => Err(EvalError::type_mismatch_ctx(
            "filter-seq-step".to_string(),
            "Dict or Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// `take`: Take the first n elements from a dict or sequence.
///
/// - For Dict: takes first n entries by position, preserving keys. Returns Dict.
/// - For Seq: takes first n elements, returning a Seq (or terminal empty dict).
/// - If n <= 0: returns empty dict (terminal for Seq).
fn builtin_take(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("take", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let n = materialize(&args[0], None, &ctx, depth)?;
    let n_int = match n {
        Value::Int(i) => i,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "take".to_string(),
                "Int",
                other.type_name(),
                call_span,
            )
            .into())
        }
    };

    if n_int <= 0 {
        // Return empty dict (terminal for Seq, empty for Dict)
        return ok_val(Value::Dict(IndexMap::new()));
    }

    let xs = materialize(&args[1], None, &ctx, depth)?;
    match xs {
        Value::Dict(ref map) => {
            // Dict: take first n entries by position
            let taken: IndexMap<Key, Rc<Thunk>> = map
                .iter()
                .take(n_int as usize)
                .map(|(k, v)| (k.clone(), Rc::clone(v)))
                .collect();
            ok_val(Value::Dict(taken))
        }
        Value::Seq { head, tail } => {
            // Seq: head = seq head, tail = take(n-1, seq tail)
            let new_head = Rc::clone(&head);
            let tail_args = vec![ok_val(Value::Int(n_int - 1))?, Rc::clone(&tail)];
            let new_tail = Rc::new(Thunk::new_pending_builtin(
                builtin_take,
                tail_args,
                IndexMap::new(),
                depth + 1,
                call_span,
                Cow::Borrowed("call $take"),
                Rc::clone(&ctx),
            ));
            ok_val(Value::Seq {
                head: new_head,
                tail: new_tail,
            })
        }
        other => Err(EvalError::type_mismatch_ctx(
            "take".to_string(),
            "Dict or Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// `drop`: Drop the first n elements from a Dict or Seq.
///
/// - For Dict: skip first n entries by position, return Dict with remaining entries
/// - For Seq: use lazy step function to drop elements one at a time
///
/// Args: (n, xs)
fn builtin_drop(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("drop", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let n = materialize(&args[0], None, &ctx, depth)?;
    let n_int = match n {
        Value::Int(i) => i,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "drop".to_string(),
                "Int",
                other.type_name(),
                call_span,
            )
            .into())
        }
    };

    if n_int <= 0 {
        // Return xs unchanged
        return Ok(Rc::clone(&args[1]));
    }

    let xs = materialize(&args[1], None, &ctx, depth)?;
    match xs {
        Value::Dict(ref map) => {
            // Dict: skip first n entries by position
            let dropped: IndexMap<Key, Rc<Thunk>> = map
                .iter()
                .skip(n_int as usize)
                .map(|(k, v)| (k.clone(), Rc::clone(v)))
                .collect();
            ok_val(Value::Dict(dropped))
        }
        Value::Seq { head: _, tail } => {
            // Seq: use lazy step function to drop remaining elements
            let n_minus_1 = Rc::new(Thunk::new_materialized(Value::Int(n_int - 1), call_span));
            let step_args = vec![n_minus_1, tail];
            Ok(Rc::new(Thunk::new_pending_builtin(
                builtin_drop_seq_step,
                step_args,
                IndexMap::new(),
                depth,
                call_span,
                Cow::Borrowed("call $drop"),
                Rc::clone(&ctx),
            )))
        }
        other => Err(EvalError::type_mismatch_ctx(
            "drop".to_string(),
            "Dict or Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// Helper for `drop` on Seq: lazily drop elements one at a time.
///
/// Args: (n_remaining, seq)
fn builtin_drop_seq_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        depth,
        call_span,
        ctx,
        ..
    } = ctx_arg;

    let n = materialize(&args[0], None, &ctx, depth)?;
    let n_int = match n {
        Value::Int(i) => i,
        _ => unreachable!("drop_seq_step: n_remaining must be Int"),
    };

    if n_int <= 0 {
        // Done dropping, return remaining seq
        return Ok(Rc::clone(&args[1]));
    }

    let seq = materialize(&args[1], None, &ctx, depth)?;
    match seq {
        Value::Dict(_) => {
            // End of sequence before we finished dropping
            ok_val(Value::Dict(IndexMap::new()))
        }
        Value::Seq { head: _, tail } => {
            // Drop this element, continue with tail
            let n_minus_1 = Rc::new(Thunk::new_materialized(Value::Int(n_int - 1), call_span));
            let step_args = vec![n_minus_1, tail];
            Ok(Rc::new(Thunk::new_pending_builtin(
                builtin_drop_seq_step,
                step_args,
                IndexMap::new(),
                depth + 1,
                call_span,
                Cow::Borrowed("call $drop"),
                Rc::clone(&ctx),
            )))
        }
        other => Err(EvalError::internal(
            format!("drop: invalid Seq tail, got {}", other.type_name()),
            call_span,
        )
        .into()),
    }
}

/// `reduce`: Fold a function over a Dict or Seq.
/// Inherently materializing: accumulator pattern requires sequential evaluation.
///
/// - For Dict: build a chain of PendingCall thunks, one per value
/// - For Seq: use recursive helper to build lazy chain
///
/// Args: (f, init, xs)
fn builtin_reduce(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("reduce", named, call_span)?;
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }

    let f_thunk = Rc::clone(&args[0]);
    let init_thunk = Rc::clone(&args[1]);
    let xs = materialize(&args[2], None, &ctx, depth)?;

    match xs {
        Value::Dict(ref map) => {
            // Dict path: build a chain of PendingCall thunks
            let mut acc = init_thunk;
            for (_key, value_thunk) in map.iter() {
                acc = Rc::new(Thunk::new_pending_call(
                    Rc::clone(&f_thunk),
                    vec![acc, Rc::clone(value_thunk)],
                    IndexMap::new(),
                    call_span,
                    value_thunk.span,
                    Cow::Borrowed("reduce"),
                    Rc::clone(&ctx),
                ));
            }
            Ok(acc)
        }
        Value::Seq { head, tail } => {
            // Seq path: use recursive step function
            let step_args = vec![
                Rc::clone(&f_thunk),
                init_thunk,
                Rc::clone(&head),
                Rc::clone(&tail),
            ];
            Ok(Rc::new(Thunk::new_pending_builtin(
                builtin_reduce_seq_step,
                step_args,
                IndexMap::new(),
                depth,
                call_span,
                Cow::Borrowed("call $reduce"),
                Rc::clone(&ctx),
            )))
        }
        other => Err(EvalError::type_mismatch_ctx(
            "reduce".to_string(),
            "Dict or Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// Helper for `reduce` on Seq: process one element and recurse.
///
/// Args: (f, acc, head, tail)
fn builtin_reduce_seq_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("reduce_seq_step", named, call_span)?;
    if args.len() != 4 {
        return Err(EvalError::arity_mismatch(4, args.len(), call_span).into());
    }

    let f_thunk = Rc::clone(&args[0]);
    let acc_thunk = Rc::clone(&args[1]);
    let head_thunk = Rc::clone(&args[2]);
    let tail_thunk = Rc::clone(&args[3]);

    // Create new accumulator: f(acc, head)
    let new_acc = Rc::new(Thunk::new_pending_call(
        Rc::clone(&f_thunk),
        vec![acc_thunk, head_thunk],
        IndexMap::new(),
        call_span,
        tail_thunk.span,
        Cow::Borrowed("reduce"),
        Rc::clone(&ctx),
    ));

    // Check if tail is empty (sequence end)
    let tail_val = materialize(&tail_thunk, None, &ctx, depth)?;
    match tail_val {
        Value::Dict(_) => {
            // Empty dict = end of sequence, return accumulator
            Ok(new_acc)
        }
        Value::Seq { head, tail } => {
            // Continue reducing
            let step_args = vec![
                Rc::clone(&f_thunk),
                new_acc,
                Rc::clone(&head),
                Rc::clone(&tail),
            ];
            Ok(Rc::new(Thunk::new_pending_builtin(
                builtin_reduce_seq_step,
                step_args,
                IndexMap::new(),
                depth + 1,
                call_span,
                Cow::Borrowed("call $reduce"),
                Rc::clone(&ctx),
            )))
        }
        other => Err(EvalError::internal(
            format!("reduce: invalid Seq tail, got {}", other.type_name()),
            call_span,
        )
        .into()),
    }
}

/// `join`: Join elements with a separator string.
///
/// - For Dict: materialize values, stringify each, join with separator
/// - For Seq: traverse head/tail chain, stringify each element, join
///
/// Args: (sep, xs)
/// Inherently materializing: must inspect and stringify all elements to concatenate.
fn builtin_join(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("join", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let sep = materialize(&args[0], None, &ctx, depth)?;
    let sep_str = require_string("join", sep, call_span)?;

    let xs = materialize(&args[1], None, &ctx, depth)?;
    match xs {
        Value::Dict(ref map) => {
            // Dict path: iterate values, materialize, stringify, join
            let mut parts = Vec::with_capacity(map.len());
            for (_key, value_thunk) in map.iter() {
                let val = materialize(value_thunk, None, &ctx, depth)?;
                parts.push(stringify(&val));
            }

            // Early return for empty collection
            if parts.is_empty() {
                return ok_val(Value::String(String::new()));
            }

            // Check output size before joining
            let total_parts_len: usize = parts.iter().map(|p| p.len()).sum();
            let sep_contribution = sep_str.len().saturating_mul(parts.len().saturating_sub(1));
            let total_output_len = total_parts_len.saturating_add(sep_contribution);

            if total_output_len > MAX_STRING_SIZE {
                return Err(EvalError::resource_limit_exceeded(
                    format!(
                        "join: output would exceed {} MB limit ({} bytes)",
                        MAX_STRING_SIZE / (1024 * 1024),
                        total_output_len
                    ),
                    call_span,
                )
                .into());
            }

            ok_val(Value::String(parts.join(&sep_str)))
        }
        Value::Seq { head, tail } => {
            // Seq path: traverse head/tail chain, collect strings
            let mut parts = Vec::new();
            let mut current_head = Rc::clone(&head);
            let mut current_tail = Rc::clone(&tail);

            loop {
                // Materialize and stringify current head
                let head_val = materialize(&current_head, None, &ctx, depth)?;
                parts.push(stringify(&head_val));

                // Check collection size limit
                if parts.len() >= MAX_COLLECT_SIZE {
                    return Err(EvalError::resource_limit_exceeded(
                        format!("join: sequence exceeds {} elements", MAX_COLLECT_SIZE),
                        call_span,
                    )
                    .into());
                }

                // Check tail
                let tail_val = materialize(&current_tail, None, &ctx, depth)?;
                match tail_val {
                    Value::Dict(_) => {
                        // End of sequence
                        break;
                    }
                    Value::Seq { head, tail } => {
                        current_head = Rc::clone(&head);
                        current_tail = Rc::clone(&tail);
                    }
                    other => {
                        return Err(EvalError::internal(
                            format!("join: invalid Seq tail, got {}", other.type_name()),
                            call_span,
                        )
                        .into());
                    }
                }
            }

            // Early return for empty collection
            if parts.is_empty() {
                return ok_val(Value::String(String::new()));
            }

            // Check output size before joining
            let total_parts_len: usize = parts.iter().map(|p| p.len()).sum();
            let sep_contribution = sep_str.len().saturating_mul(parts.len().saturating_sub(1));
            let total_output_len = total_parts_len.saturating_add(sep_contribution);

            if total_output_len > MAX_STRING_SIZE {
                return Err(EvalError::resource_limit_exceeded(
                    format!(
                        "join: output would exceed {} MB limit ({} bytes)",
                        MAX_STRING_SIZE / (1024 * 1024),
                        total_output_len
                    ),
                    call_span,
                )
                .into());
            }

            ok_val(Value::String(parts.join(&sep_str)))
        }
        other => Err(EvalError::type_mismatch_ctx(
            "join".to_string(),
            "Dict or Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// `concat`: Concatenate two collections.
///
/// - For Seq: lazily chain xs and ys (O(1) initial, O(n) on materialization).
/// - For Dict: eagerly materialize both dicts and merge them with integer reindexing.
///
/// Args: (xs, ys)
fn builtin_concat(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("concat", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let xs = materialize(&args[0], None, &ctx, depth)?;
    let ys_thunk = Rc::clone(&args[1]);

    match xs {
        Value::Seq { head, tail } => {
            // Seq path: lazy chaining via step function
            let step_args = vec![tail, ys_thunk];
            let result_thunk = Rc::new(Thunk::new_pending_builtin(
                builtin_concat_seq_step,
                step_args,
                IndexMap::new(),
                depth + 1,
                call_span,
                Cow::Borrowed("call $concat"),
                Rc::clone(&ctx),
            ));
            ok_val(Value::Seq {
                head,
                tail: result_thunk,
            })
        }
        Value::Dict(ref xs_map) => {
            // Dict path: eagerly merge both dicts with integer reindexing
            if xs_map.is_empty() {
                // Empty xs: return ys directly
                return Ok(ys_thunk);
            }

            let ys = materialize(&ys_thunk, None, &ctx, depth)?;
            match ys {
                Value::Dict(ref ys_map) => {
                    let mut result = IndexMap::with_capacity(xs_map.len() + ys_map.len());
                    let mut idx = 0i64;

                    // Add all values from xs
                    for (_key, value_thunk) in xs_map {
                        result.insert(Key::Int(idx), Rc::clone(value_thunk));
                        idx = idx.checked_add(1).ok_or_else(|| {
                            EvalError::integer_overflow("concat".to_string(), call_span)
                        })?;
                    }

                    // Add all values from ys
                    for (_key, value_thunk) in ys_map {
                        result.insert(Key::Int(idx), Rc::clone(value_thunk));
                        idx = idx.checked_add(1).ok_or_else(|| {
                            EvalError::integer_overflow("concat".to_string(), call_span)
                        })?;
                    }

                    ok_val(Value::Dict(result))
                }
                other => Err(EvalError::internal(
                    format!(
                        "concat: both arguments must be the same collection type, got Dict and {}",
                        other.type_name()
                    ),
                    call_span,
                )
                .into()),
            }
        }
        other => Err(EvalError::type_mismatch_ctx(
            "concat".to_string(),
            "Dict or Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// Helper for concat on Seq: lazily chains xs tail with ys.
///
/// Args: (xs_tail, ys)
fn builtin_concat_seq_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        depth,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    let xs_tail_thunk = Rc::clone(&args[0]);
    let ys_thunk = Rc::clone(&args[1]);
    let xs_tail = materialize(&xs_tail_thunk, None, &ctx, depth)?;

    match xs_tail {
        Value::Dict(_) => {
            // End of xs sequence: return ys
            Ok(ys_thunk)
        }
        Value::Seq { head, tail } => {
            // Continue chaining: head from xs, tail is concat(tail, ys)
            let step_args = vec![tail, ys_thunk];
            let new_tail = Rc::new(Thunk::new_pending_builtin(
                builtin_concat_seq_step,
                step_args,
                IndexMap::new(),
                depth + 1,
                call_span,
                Cow::Borrowed("call $concat"),
                Rc::clone(&ctx),
            ));
            ok_val(Value::Seq {
                head,
                tail: new_tail,
            })
        }
        other => Err(EvalError::type_mismatch_ctx(
            "concat-seq-step".to_string(),
            "Dict or Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

fn builtin_proxy(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ..
    } = ctx_arg;
    reject_named("proxy", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    Ok(Rc::new(Thunk::new_materialized(
        Value::Proxy {
            handler: Rc::clone(&args[0]),
        },
        call_span,
    )))
}

/// Returns all builtin definitions as (name, function) pairs.
///
/// All builtins conform to the standard `BuiltinFn` signature, including `if`
/// which materializes only the chosen branch (the unchosen branch's thunk is
/// never forced, preserving lazy semantics).
pub fn standard_builtins() -> Vec<(&'static str, BuiltinFn)> {
    vec![
        // Arithmetic
        ("+", builtin_add),
        ("-", builtin_sub),
        ("*", builtin_mul),
        ("/", builtin_div_float),
        // Comparison
        ("=", builtin_eq),
        ("<", builtin_lt),
        // Control
        ("if", builtin_if),
        // Dict primitives
        ("keys", builtin_keys),
        ("length", builtin_length),
        ("merge", builtin_merge),
        ("append", builtin_append),
        // Strings
        ("str", builtin_str),
        ("split", builtin_split),
        ("replace", builtin_replace),
        ("upper", builtin_upper),
        ("lower", builtin_lower),
        ("trim", builtin_trim),
        // Numeric
        ("floor", builtin_floor),
        ("round", builtin_round),
        // Parsing
        ("to-int", builtin_to_int),
        ("to-float", builtin_to_float),
        // Evaluation control
        ("eval", builtin_eval),
        ("error", builtin_error),
        ("try", builtin_try),
        ("apply", builtin_apply),
        // Type introspection
        ("type-of", builtin_type_of),
        // I/O
        ("from-json", builtin_from_json),
        ("include", builtin_include),
        // Sequences
        ("seq", builtin_seq),
        ("head", builtin_head),
        ("tail", builtin_tail),
        ("collect", builtin_collect),
        ("seq?", builtin_seq_check),
        ("range", builtin_range),
        ("repeat", builtin_repeat),
        ("cycle", builtin_cycle),
        ("iterate", builtin_iterate),
        ("unfold", builtin_unfold),
        ("map", builtin_map),
        ("filter", builtin_filter),
        ("take", builtin_take),
        ("drop", builtin_drop),
        ("reduce", builtin_reduce),
        ("join", builtin_join),
        ("concat", builtin_concat),
        // Proxy
        ("proxy", builtin_proxy),
    ]
}

/// Create the root environment with all builtins registered as `Value::Builtin`.
pub fn create_root_env() -> Rc<RefCell<Environment>> {
    let env = Rc::new(RefCell::new(Environment::new()));
    for (name, func) in standard_builtins() {
        let thunk = Rc::new(Thunk::new_materialized(
            Value::Builtin { name, func },
            Span::origin(),
        ));
        env.borrow_mut().insert(name.to_string(), thunk);
    }

    // Add stable "builtin-*" aliases for operators that will be shadowed by prelude wrappers.
    // These provide an escape hatch to the raw Rust implementations.
    let aliases: Vec<(&'static str, BuiltinFn)> = vec![
        ("builtin-lt", builtin_lt),
        ("builtin-eq", builtin_eq),
        ("builtin-add", builtin_add),
        ("builtin-sub", builtin_sub),
        ("builtin-mul", builtin_mul),
        ("builtin-div", builtin_div_float),
        ("builtin-if", builtin_if),
        ("builtin-filter", builtin_filter),
        ("builtin-map", builtin_map),
        ("builtin-reduce", builtin_reduce),
        ("builtin-take", builtin_take),
        ("builtin-drop", builtin_drop),
    ];

    for (name, func) in aliases {
        let thunk = Rc::new(Thunk::new_materialized(
            Value::Builtin { name, func },
            Span::origin(),
        ));
        env.borrow_mut().insert(name.to_string(), thunk);
    }

    env
}

/// Create the stdlib environment: root builtins + prelude functions.
///
/// Parses and evaluates `stdlib/prelude.llt` using the root env, then
/// layers the prelude dict entries as a child scope. User code should
/// use this as the parent environment.
pub fn create_stdlib_env() -> Result<Rc<RefCell<Environment>>, Box<crate::error::EvalError>> {
    let root_env = create_root_env();

    // Create a bootstrap EvalContext with just the root env (before stdlib is loaded)
    let bootstrap_ctx =
        crate::eval::EvalContext::new(std::path::PathBuf::from("."), Rc::clone(&root_env), false);

    let prelude_source = include_str!("../stdlib/prelude.llt");
    let mut file = crate::parser::parse(prelude_source).map_err(|e| {
        crate::error::EvalError::internal(format!("prelude parse error: {e}"), Span::origin())
    })?;

    crate::desugar::desugar_file(&mut file.node);

    let thunk = crate::eval::eval_file(&file.node, Rc::clone(&root_env), &bootstrap_ctx, 0)?;

    let val = crate::eval::materialize(&thunk, None, &bootstrap_ctx, 0)?;

    let dict = match val {
        Value::Dict(map) => map,
        other => {
            return Err(crate::error::EvalError::internal(
                format!("prelude must evaluate to a Dict, got {}", other.type_name()),
                Span::origin(),
            )
            .into())
        }
    };

    // Create a child env with the prelude entries
    let stdlib_env = Rc::new(RefCell::new(Environment::with_parent(root_env)));
    for (key, thunk) in dict {
        let name = match key {
            Key::String(s) => s,
            Key::Int(n) => n.to_string(),
        };
        stdlib_env.borrow_mut().insert(name, thunk);
    }

    Ok(stdlib_env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, Param, Spanned};
    use crate::test_util::test_span;

    /// Stack size for tests that exercise deep recursive evaluation chains.
    /// The default Rust test thread stack (8 MB) is too small for tests that push
    /// MAX_EVAL_DEPTH (256) levels of PendingBuiltin thunks; 16 MB provides headroom.
    const TEST_STACK_SIZE: usize = 16 * 1024 * 1024; // 16 MB

    /// Helper: wrap a Value in a materialized Thunk inside an Rc.
    fn thunk(val: Value) -> Rc<Thunk> {
        Rc::new(Thunk::new_materialized(val, test_span(1, 1, 1, 5)))
    }

    fn no_named() -> IndexMap<String, Rc<Thunk>> {
        IndexMap::new()
    }

    fn call_span() -> Span {
        test_span(1, 1, 1, 5)
    }

    fn test_ctx() -> Rc<crate::eval::EvalContext> {
        crate::eval::EvalContext::new(std::path::PathBuf::from("."), create_root_env(), false)
    }

    fn mat(result: EvalResult<Rc<Thunk>>) -> Value {
        crate::eval::materialize(&result.unwrap(), None, &test_ctx(), 0).unwrap()
    }

    /// Helper: make a zero-arg function whose body is a single expression.
    fn zero_arg_fn(body_expr: Expr) -> Value {
        Value::Function {
            params: Rc::new(vec![]),
            body: Rc::new(Spanned::new(body_expr, test_span(1, 1, 1, 10))),
            env: Rc::new(RefCell::new(Environment::new())),
        }
    }

    /// Helper: make an n-arg function whose body is a given expression.
    fn n_arg_fn(param_names: &[&str], body_expr: Expr) -> Value {
        Value::Function {
            params: Rc::new(
                param_names
                    .iter()
                    .map(|name| Param {
                        name: name.to_string(),
                        annotation: None,
                        variadic: false,
                    })
                    .collect(),
            ),
            body: Rc::new(Spanned::new(body_expr, test_span(1, 1, 1, 10))),
            env: Rc::new(RefCell::new(Environment::new())),
        }
    }

    fn thunk_dict(map: IndexMap<Key, Rc<Thunk>>) -> Rc<Thunk> {
        Rc::new(Thunk::new_materialized(
            Value::Dict(map),
            test_span(1, 1, 1, 5),
        ))
    }

    #[test]
    fn floor_int_passthrough() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn floor_negative_int_passthrough() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Int(-7))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-7));
    }

    #[test]
    fn floor_zero_int() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Int(0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn floor_positive_float() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(3.7))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn floor_negative_float() {
        // floor(-3.2) = -4, not -3
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(-3.2))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-4));
    }

    #[test]
    fn floor_float_exact_integer() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(5.0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn floor_float_just_below_integer() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(2.9999999))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn floor_nan_errors() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(f64::NAN))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("NaN"), "got: {}", err.message());
    }

    #[test]
    fn floor_positive_infinity_errors() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(f64::INFINITY))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite number"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_negative_infinity_errors() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(f64::NEG_INFINITY))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite number"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_string_type_error() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::String("3.5".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Int or Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_bool_type_error() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Int or Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_dict_type_error() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Int or Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_wrong_arity_zero() {
        let err = builtin_floor(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_wrong_arity_two() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(3.5))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_large_positive_float_out_of_range() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(1e19))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("integer overflow"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_large_negative_float_out_of_range() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(-1e19))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("integer overflow"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_int_passthrough() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn round_negative_int_passthrough() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Int(-7))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-7));
    }

    #[test]
    fn round_positive_half_rounds_up() {
        // 0.5 rounds to 1 (half-away-from-zero)
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(0.5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(1));
    }

    #[test]
    fn round_negative_half_rounds_down() {
        // -0.5 rounds to -1 (half-away-from-zero)
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(-0.5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-1));
    }

    #[test]
    fn round_positive_below_half() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(2.4))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn round_positive_above_half() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(2.6))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn round_negative_below_half() {
        // -2.4 rounds to -2
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(-2.4))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-2));
    }

    #[test]
    fn round_negative_above_half() {
        // -2.6 rounds to -3
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(-2.6))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-3));
    }

    #[test]
    fn round_1_5_rounds_to_2() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(1.5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn round_negative_1_5_rounds_to_negative_2() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(-1.5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-2));
    }

    #[test]
    fn round_float_exact_integer() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(5.0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn round_nan_errors() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(f64::NAN))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("NaN"), "got: {}", err.message());
    }

    #[test]
    fn round_positive_infinity_errors() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(f64::INFINITY))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite number"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_negative_infinity_errors() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(f64::NEG_INFINITY))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite number"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_string_type_error() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::String("3.5".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Int or Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_bool_type_error() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Bool(false))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Int or Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_wrong_arity_zero() {
        let err = builtin_round(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_wrong_arity_two() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_large_positive_float_out_of_range() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(1e19))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("integer overflow"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_large_negative_float_out_of_range() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(-1e19))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("integer overflow"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_valid_positive() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("42".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn to_int_valid_negative() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("-7".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-7));
    }

    #[test]
    fn to_int_valid_zero() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("0".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn to_int_valid_large() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("9223372036854775807".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(i64::MAX));
    }

    #[test]
    fn to_int_invalid_float_string() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("3.14".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_invalid_text() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_invalid_empty() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_invalid_with_spaces() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String(" 42 ".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_rejects_int_input() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("Int"),
            "should mention Int, got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_rejects_float_input() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_rejects_bool_input() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_rejects_dict_input() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_wrong_arity_zero() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_wrong_arity_two() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[
                thunk(Value::String("1".into())),
                thunk(Value::String("2".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_valid_decimal() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("3.14".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn to_float_valid_integer_string() {
        // "42" parses as 42.0
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("42".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(42.0));
    }

    #[test]
    fn to_float_valid_negative() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("-2.5".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(-2.5));
    }

    #[test]
    fn to_float_valid_scientific_notation() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("1.5e10".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(1.5e10));
    }

    #[test]
    fn to_float_valid_negative_exponent() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("2.5e-3".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(2.5e-3));
    }

    #[test]
    fn to_float_valid_zero() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("0.0".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(0.0));
    }

    #[test]
    fn to_float_valid_leading_dot() {
        // ".5" parses to 0.5
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String(".5".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(0.5));
    }

    #[test]
    fn to_float_invalid_text() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_invalid_empty() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_inf() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("inf".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite number"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_negative_inf() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("-inf".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite number"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_infinity() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("infinity".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite number"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_nan() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("NaN".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite number"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_int_input() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_float_input() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_bool_input() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_wrong_arity_zero() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_wrong_arity_two() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[
                thunk(Value::String("1.0".into())),
                thunk(Value::String("2.0".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::String("1.0".into())));
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("3.14".into()))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_overflow() {
        // One past i64::MAX
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("9223372036854775808".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn eval_primitive_int() {
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn eval_primitive_string() {
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn eval_primitive_float() {
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn eval_primitive_bool() {
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn eval_empty_dict() {
        let dict = Value::Dict(IndexMap::new());
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(dict)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn eval_flat_dict() {
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        map.insert(Key::String("b".into()), thunk(Value::Int(2)));
        let dict = Value::Dict(map);
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(dict)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let a = materialize(&map[&Key::String("a".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(a, Value::Int(1));
                let b = materialize(&map[&Key::String("b".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(b, Value::Int(2));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn eval_nested_dict() {
        // Build [x: [y: 42]]
        let mut inner = IndexMap::new();
        inner.insert(Key::String("y".into()), thunk(Value::Int(42)));
        let inner_dict = Value::Dict(inner);

        let mut outer = IndexMap::new();
        outer.insert(Key::String("x".into()), thunk(inner_dict));
        let outer_dict = Value::Dict(outer);

        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(outer_dict)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(outer_map) => {
                let x_val = materialize(&outer_map[&Key::String("x".into())], None, &test_ctx(), 0)
                    .unwrap();
                match x_val {
                    Value::Dict(inner_map) => {
                        let y_val =
                            materialize(&inner_map[&Key::String("y".into())], None, &test_ctx(), 0)
                                .unwrap();
                        assert_eq!(y_val, Value::Int(42));
                    }
                    _ => panic!("expected inner Dict"),
                }
            }
            _ => panic!("expected outer Dict"),
        }
    }

    #[test]
    fn eval_with_unevaluated_thunk() {
        // Create an unevaluated thunk wrapping a literal -- eval should force it
        let expr = Rc::new(Spanned::new(Expr::Int(99), test_span(1, 1, 1, 5)));
        let env = Rc::new(RefCell::new(Environment::new()));
        let unevaluated = Rc::new(Thunk::new_unevaluated(
            expr,
            env,
            test_ctx(),
            test_span(1, 1, 1, 5),
        ));

        let mut map = IndexMap::new();
        map.insert(Key::String("val".into()), unevaluated);
        let dict = Value::Dict(map);

        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(dict)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                let v =
                    materialize(&map[&Key::String("val".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(v, Value::Int(99));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn eval_arity_error() {
        let err = builtin_eval(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn error_raises_with_message() {
        let err = builtin_error(BuiltinArgs {
            args: &[thunk(Value::String("boom".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert_eq!(err.message(), "boom");
    }

    #[test]
    fn error_custom_message() {
        let err = builtin_error(BuiltinArgs {
            args: &[thunk(Value::String("division by zero".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert_eq!(err.message(), "division by zero");
    }

    #[test]
    fn error_type_mismatch_on_non_string() {
        let err = builtin_error(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("String"), "got: {}", err.message());
    }

    #[test]
    fn error_arity_check() {
        let err = builtin_error(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn try_success_returns_ok_dict() {
        // [fn [] 42]
        let func = zero_arg_fn(Expr::Int(42));
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert!(map.contains_key(&Key::String("ok".into())));
                let ok_val =
                    materialize(&map[&Key::String("ok".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(ok_val, Value::Int(42));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn try_success_with_string_body() {
        let func = zero_arg_fn(Expr::Str("hello".into()));
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                let ok_val =
                    materialize(&map[&Key::String("ok".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(ok_val, Value::String("hello".into()));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn try_failure_returns_err_dict() {
        // [fn [] $nonexistent] -- references an undefined variable
        let func = zero_arg_fn(Expr::VarRef("nonexistent".into()));
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert!(map.contains_key(&Key::String("err".into())));
                let err_val =
                    materialize(&map[&Key::String("err".into())], None, &test_ctx(), 0).unwrap();
                match err_val {
                    Value::String(msg) => {
                        assert!(
                            msg.contains("undefined variable"),
                            "expected 'undefined variable' in error message, got: {msg}"
                        );
                    }
                    _ => panic!("expected String error message"),
                }
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn try_non_function_type_error() {
        let err = builtin_try(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Function"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn try_non_zero_arg_function_error() {
        let func = n_arg_fn(&["x"], Expr::VarRef("x".into()));
        let err = builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected 0 arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn try_arity_check() {
        let err = builtin_try(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn try_with_builtin_success() {
        fn ok_builtin(_ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            ok_val(Value::Int(99))
        }
        let b = Value::Builtin {
            name: "ok",
            func: ok_builtin,
        };
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(b)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                let ok_val =
                    materialize(&map[&Key::String("ok".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(ok_val, Value::Int(99));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn try_with_builtin_failure() {
        fn err_builtin(ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let BuiltinArgs { call_span, .. } = ctx;
            Err(EvalError::new("builtin error".to_string(), call_span).into())
        }
        let b = Value::Builtin {
            name: "fail",
            func: err_builtin,
        };
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(b)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                let err_val =
                    materialize(&map[&Key::String("err".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(err_val, Value::String("builtin error".into()));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn try_depth_exceeded_not_catchable() {
        // DepthExceeded errors should NOT be caught by $try - they should propagate
        // NOTE: No corpus test exists for this because triggering DepthExceeded
        // reliably requires either a custom builtin (not available in corpus tests)
        // or recursive thunk forcing with 16MB stack (impractical in corpus format).
        fn depth_exceeded_builtin(ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let BuiltinArgs { call_span, .. } = ctx;
            Err(EvalError::depth_exceeded(256, call_span).into())
        }
        let b = Value::Builtin {
            name: "depth_fail",
            func: depth_exceeded_builtin,
        };
        let err = builtin_try(BuiltinArgs {
            args: &[thunk(b)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        // Should propagate as error, not return err dict
        assert!(
            err.message().contains("maximum evaluation depth exceeded"),
            "expected depth error to propagate, got: {}",
            err.message()
        );
        assert_eq!(err.kind.code(), "E040");
    }

    #[test]
    fn try_resource_limit_exceeded_not_catchable() {
        // ResourceLimitExceeded errors should NOT be caught by $try - they should propagate
        fn resource_limit_builtin(ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let BuiltinArgs { call_span, .. } = ctx;
            Err(EvalError::resource_limit_exceeded(
                "test: exceeded resource limit (1000000)".to_string(),
                call_span,
            )
            .into())
        }
        let b = Value::Builtin {
            name: "resource_fail",
            func: resource_limit_builtin,
        };
        let err = builtin_try(BuiltinArgs {
            args: &[thunk(b)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        // Should propagate as error, not return err dict
        assert!(
            err.message().contains("exceeded resource limit"),
            "expected resource limit error to propagate, got: {}",
            err.message()
        );
        assert_eq!(err.kind.code(), "E043");
    }

    #[test]
    fn apply_single_arg() {
        // [fn [x] $x] applied to [42]
        let func = n_arg_fn(&["x"], Expr::VarRef("x".into()));
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(42)));
        let args_val = Value::Dict(arg_dict);

        let result = mat(builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(args_val)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn apply_multiple_args_returns_first() {
        // [fn [a b] $a] applied to [10, 20]
        let func = n_arg_fn(&["a", "b"], Expr::VarRef("a".into()));
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(10)));
        arg_dict.insert(Key::Int(1), thunk(Value::Int(20)));
        let args_val = Value::Dict(arg_dict);

        let result = mat(builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(args_val)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(10));
    }

    #[test]
    fn apply_multiple_args_returns_second() {
        // [fn [a b] $b] applied to [10, 20]
        let func = n_arg_fn(&["a", "b"], Expr::VarRef("b".into()));
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(10)));
        arg_dict.insert(Key::Int(1), thunk(Value::Int(20)));
        let args_val = Value::Dict(arg_dict);

        let result = mat(builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(args_val)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(20));
    }

    #[test]
    fn apply_with_builtin() {
        fn add_builtin(builtin_ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let BuiltinArgs {
                args,
                call_span,
                ctx,
                ..
            } = builtin_ctx;
            let a = materialize(&args[0], None, &ctx, 0)?;
            let b = materialize(&args[1], None, &ctx, 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => ok_val(Value::Int(x + y)),
                _ => Err(EvalError::new("expected ints".to_string(), call_span).into()),
            }
        }
        let func = Value::Builtin {
            name: "add",
            func: add_builtin,
        };
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(3)));
        arg_dict.insert(Key::Int(1), thunk(Value::Int(4)));
        let args_val = Value::Dict(arg_dict);

        let result = mat(builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(args_val)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(7));
    }

    #[test]
    fn apply_arity_mismatch() {
        let func = n_arg_fn(&["x", "y"], Expr::VarRef("x".into()));
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(1)));
        let args_val = Value::Dict(arg_dict);

        let err = builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(args_val)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message()
                .contains("missing argument for required parameter"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn apply_non_function_type_error() {
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(1)));
        let args_val = Value::Dict(arg_dict);

        let err = builtin_apply(BuiltinArgs {
            args: &[thunk(Value::Int(42)), thunk(args_val)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Function"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn apply_non_dict_args_type_error() {
        let func = n_arg_fn(&["x"], Expr::VarRef("x".into()));
        let err = builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn apply_wrong_arity() {
        let err = builtin_apply(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn type_of_int() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Int".into()));
    }

    #[test]
    fn type_of_float() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Float".into()));
    }

    #[test]
    fn type_of_string() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::String("hi".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("String".into()));
    }

    #[test]
    fn type_of_bool() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::Bool(false))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Bool".into()));
    }

    #[test]
    fn type_of_dict() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Dict".into()));
    }

    #[test]
    fn type_of_function() {
        let func = zero_arg_fn(Expr::Int(0));
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(func)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Function".into()));
    }

    #[test]
    fn type_of_builtin_returns_function() {
        fn dummy(_ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            ok_val(Value::Int(0))
        }
        let builtin = Value::Builtin {
            name: "dummy",
            func: dummy,
        };
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(builtin)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Function".into()));
    }

    #[test]
    fn type_of_arity_check() {
        let err = builtin_type_of(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn from_json_int() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("42".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn from_json_float() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("3.14".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn from_json_string() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String(r#""hello""#.into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn from_json_bool_true() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("true".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn from_json_bool_false() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("false".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(false));
    }

    #[test]
    fn from_json_null_becomes_empty_dict() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("null".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            _ => panic!("expected empty Dict for null"),
        }
    }

    #[test]
    fn from_json_array() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("[1, 2, 3]".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v0 = materialize(&map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v0, Value::Int(1));
                let v1 = materialize(&map[&Key::Int(1)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v1, Value::Int(2));
                let v2 = materialize(&map[&Key::Int(2)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v2, Value::Int(3));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn from_json_object() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String(
                r#"{"name": "Alice", "age": 30}"#.into(),
            ))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let name =
                    materialize(&map[&Key::String("name".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(name, Value::String("Alice".into()));
                let age =
                    materialize(&map[&Key::String("age".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(age, Value::Int(30));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn from_json_nested_structure() {
        let json = r#"{"users": [{"name": "Bob"}, {"name": "Eve"}]}"#;
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String(json.into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                let users =
                    materialize(&map[&Key::String("users".into())], None, &test_ctx(), 0).unwrap();
                match users {
                    Value::Dict(arr) => {
                        assert_eq!(arr.len(), 2);
                        let user0 = materialize(&arr[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                        match user0 {
                            Value::Dict(u) => {
                                let name = materialize(
                                    &u[&Key::String("name".into())],
                                    None,
                                    &test_ctx(),
                                    0,
                                )
                                .unwrap();
                                assert_eq!(name, Value::String("Bob".into()));
                            }
                            _ => panic!("expected Dict for user"),
                        }
                    }
                    _ => panic!("expected Dict for users array"),
                }
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn from_json_invalid_json() {
        let err = builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("{bad json".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("invalid JSON"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn from_json_non_string_type_error() {
        let err = builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn from_json_arity_check() {
        let err = builtin_from_json(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn from_json_empty_object() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("{}".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            _ => panic!("expected empty Dict"),
        }
    }

    #[test]
    fn from_json_empty_array() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("[]".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            _ => panic!("expected empty Dict"),
        }
    }

    #[test]
    fn from_json_mixed_array() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String(r#"[1, "two", true, null]"#.into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 4);
                let v0 = materialize(&map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v0, Value::Int(1));
                let v1 = materialize(&map[&Key::Int(1)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v1, Value::String("two".into()));
                let v2 = materialize(&map[&Key::Int(2)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v2, Value::Bool(true));
                let v3 = materialize(&map[&Key::Int(3)], None, &test_ctx(), 0).unwrap();
                match v3 {
                    Value::Dict(m) => assert!(m.is_empty()),
                    _ => panic!("expected empty Dict for null"),
                }
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn from_json_depth_guard() {
        // Build JSON nested beyond MAX_EVAL_DEPTH: {"a":{"a":{...}}}
        // serde_json's default recursion limit is 128, so we test json_to_value
        // directly with a pre-parsed serde_json::Value.
        fn build_deep(depth: usize) -> serde_json::Value {
            let mut val = serde_json::Value::Object(serde_json::Map::new());
            for _ in 0..depth {
                let mut obj = serde_json::Map::new();
                obj.insert("a".into(), val);
                val = serde_json::Value::Object(obj);
            }
            val
        }
        let deep = build_deep(MAX_EVAL_DEPTH + 1);
        let err = json_to_value(&deep, 0, call_span()).unwrap_err();
        assert!(
            err.message()
                .contains("maximum JSON nesting depth exceeded"),
            "expected depth error, got: {}",
            err.message()
        );
    }

    #[test]
    fn keys_empty_dict() {
        let dict = thunk_dict(IndexMap::new());
        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn keys_int_keyed_dict() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::String("a".into())));
        map.insert(Key::Int(1), thunk(Value::String("b".into())));
        map.insert(Key::Int(2), thunk(Value::String("c".into())));
        let dict = thunk_dict(map);

        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 3);
                for i in 0..3 {
                    let val = materialize(&keys_map[&Key::Int(i)], None, &test_ctx(), 0).unwrap();
                    assert_eq!(val, Value::Int(i));
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn keys_string_keyed_dict() {
        let mut map = IndexMap::new();
        map.insert(
            Key::String("name".into()),
            thunk(Value::String("Alice".into())),
        );
        map.insert(Key::String("age".into()), thunk(Value::Int(30)));
        let dict = thunk_dict(map);

        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 2);
                let k0 = materialize(&keys_map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                assert_eq!(k0, Value::String("name".into()));
                let k1 = materialize(&keys_map[&Key::Int(1)], None, &test_ctx(), 0).unwrap();
                assert_eq!(k1, Value::String("age".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn keys_mixed_key_dict() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::String("first".into())));
        map.insert(
            Key::String("label".into()),
            thunk(Value::String("second".into())),
        );
        map.insert(Key::Int(5), thunk(Value::String("third".into())));
        let dict = thunk_dict(map);

        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 3);
                let k0 = materialize(&keys_map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                assert_eq!(k0, Value::Int(0));
                let k1 = materialize(&keys_map[&Key::Int(1)], None, &test_ctx(), 0).unwrap();
                assert_eq!(k1, Value::String("label".into()));
                let k2 = materialize(&keys_map[&Key::Int(2)], None, &test_ctx(), 0).unwrap();
                assert_eq!(k2, Value::Int(5));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn keys_preserves_insertion_order() {
        let mut map = IndexMap::new();
        map.insert(Key::String("z".into()), thunk(Value::Int(1)));
        map.insert(Key::String("a".into()), thunk(Value::Int(2)));
        map.insert(Key::String("m".into()), thunk(Value::Int(3)));
        let dict = thunk_dict(map);

        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(keys_map) => {
                let k0 = materialize(&keys_map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                let k1 = materialize(&keys_map[&Key::Int(1)], None, &test_ctx(), 0).unwrap();
                let k2 = materialize(&keys_map[&Key::Int(2)], None, &test_ctx(), 0).unwrap();
                assert_eq!(k0, Value::String("z".into()));
                assert_eq!(k1, Value::String("a".into()));
                assert_eq!(k2, Value::String("m".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn length_empty_dict() {
        let dict = thunk_dict(IndexMap::new());
        let result = mat(builtin_length(BuiltinArgs {
            args: &[dict],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn length_non_empty_dict() {
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        map.insert(Key::String("b".into()), thunk(Value::Int(2)));
        map.insert(Key::String("c".into()), thunk(Value::Int(3)));
        let dict = thunk_dict(map);
        let result = mat(builtin_length(BuiltinArgs {
            args: &[dict],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn length_int_keyed_dict() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::String("x".into())));
        map.insert(Key::Int(1), thunk(Value::String("y".into())));
        let dict = thunk_dict(map);
        let result = mat(builtin_length(BuiltinArgs {
            args: &[dict],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn merge_disjoint_keys() {
        let mut left = IndexMap::new();
        left.insert(Key::String("a".into()), thunk(Value::Int(1)));
        left.insert(Key::String("b".into()), thunk(Value::Int(2)));
        let mut right = IndexMap::new();
        right.insert(Key::String("c".into()), thunk(Value::Int(3)));
        right.insert(Key::String("d".into()), thunk(Value::Int(4)));

        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left), thunk_dict(right)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 4);
                assert!(map.contains_key(&Key::String("a".into())));
                assert!(map.contains_key(&Key::String("b".into())));
                assert!(map.contains_key(&Key::String("c".into())));
                assert!(map.contains_key(&Key::String("d".into())));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn merge_overlapping_keys_right_wins() {
        let mut left = IndexMap::new();
        left.insert(Key::String("x".into()), thunk(Value::Int(1)));
        left.insert(Key::String("y".into()), thunk(Value::Int(2)));
        let mut right = IndexMap::new();
        right.insert(Key::String("y".into()), thunk(Value::Int(99)));
        right.insert(Key::String("z".into()), thunk(Value::Int(3)));

        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left), thunk_dict(right)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let x = materialize(&map[&Key::String("x".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(x, Value::Int(1));
                let y = materialize(&map[&Key::String("y".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(y, Value::Int(99));
                let z = materialize(&map[&Key::String("z".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(z, Value::Int(3));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn merge_empty_dicts() {
        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(IndexMap::new()), thunk_dict(IndexMap::new())],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn merge_left_empty() {
        let mut right = IndexMap::new();
        right.insert(Key::Int(0), thunk(Value::String("only".into())));
        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(IndexMap::new()), thunk_dict(right)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let v = materialize(&map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v, Value::String("only".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn merge_right_empty() {
        let mut left = IndexMap::new();
        left.insert(Key::Int(0), thunk(Value::String("only".into())));
        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left), thunk_dict(IndexMap::new())],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let v = materialize(&map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v, Value::String("only".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn merge_preserves_thunks() {
        let span = test_span(1, 1, 1, 5);
        let left_thunk = Rc::new(Thunk::new_materialized(Value::Int(42), span));
        let right_thunk = Rc::new(Thunk::new_materialized(Value::Int(99), span));

        let mut left = IndexMap::new();
        left.insert(Key::String("a".into()), Rc::clone(&left_thunk));
        let mut right = IndexMap::new();
        right.insert(Key::String("b".into()), Rc::clone(&right_thunk));

        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left), thunk_dict(right)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert!(Rc::ptr_eq(&map[&Key::String("a".into())], &left_thunk));
                assert!(Rc::ptr_eq(&map[&Key::String("b".into())], &right_thunk));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn merge_preserves_left_order() {
        let mut left = IndexMap::new();
        left.insert(Key::String("b".into()), thunk(Value::Int(1)));
        left.insert(Key::String("a".into()), thunk(Value::Int(2)));
        let mut right = IndexMap::new();
        right.insert(Key::String("d".into()), thunk(Value::Int(3)));
        right.insert(Key::String("c".into()), thunk(Value::Int(4)));

        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left), thunk_dict(right)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                let keys: Vec<&Key> = map.keys().collect();
                assert_eq!(
                    keys,
                    vec![
                        &Key::String("b".into()),
                        &Key::String("a".into()),
                        &Key::String("d".into()),
                        &Key::String("c".into()),
                    ]
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn keys_wrong_arity_zero() {
        let err = builtin_keys(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn keys_wrong_arity_two() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_keys(BuiltinArgs {
            args: &[d.clone(), d],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn length_wrong_arity_zero() {
        let err = builtin_length(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn length_wrong_arity_two() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_length(BuiltinArgs {
            args: &[d.clone(), d],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn merge_wrong_arity_one() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_merge(BuiltinArgs {
            args: &[d],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn merge_wrong_arity_three() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_merge(BuiltinArgs {
            args: &[d.clone(), d.clone(), d],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn keys_non_dict_int() {
        let err = builtin_keys(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("keys"), "got: {}", err.message());
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Int"), "got: {}", err.message());
    }

    #[test]
    fn keys_non_dict_string() {
        let err = builtin_keys(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("keys"), "got: {}", err.message());
        assert!(
            err.message().contains("got String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn keys_non_dict_bool() {
        let err = builtin_keys(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("keys"), "got: {}", err.message());
        assert!(err.message().contains("got Bool"), "got: {}", err.message());
    }

    #[test]
    fn length_non_dict() {
        let err = builtin_length(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("length"), "got: {}", err.message());
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("got String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn merge_first_arg_non_dict() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_merge(BuiltinArgs {
            args: &[thunk(Value::Int(1)), d],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("merge"), "got: {}", err.message());
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Int"), "got: {}", err.message());
    }

    #[test]
    fn merge_second_arg_non_dict() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_merge(BuiltinArgs {
            args: &[d, thunk(Value::String("nope".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("merge"), "got: {}", err.message());
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("got String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn append_to_empty_dict() {
        let empty = thunk_dict(IndexMap::new());
        let result = mat(builtin_append(BuiltinArgs {
            args: &[empty, thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let val =
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(val, Value::Int(42));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_to_existing_list() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::String("a".into())));
        map.insert(Key::Int(1), thunk(Value::String("b".into())));
        let dict = thunk_dict(map);
        let result = mat(builtin_append(BuiltinArgs {
            args: &[dict, thunk(Value::String("c".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let val =
                    materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(val, Value::String("c".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_to_dict_with_string_keys_only() {
        // Dict with only string keys -- next int key should be 0
        let mut map = IndexMap::new();
        map.insert(Key::String("x".into()), thunk(Value::Int(1)));
        let dict = thunk_dict(map);
        let result = mat(builtin_append(BuiltinArgs {
            args: &[dict, thunk(Value::Int(99))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let val =
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(val, Value::Int(99));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_to_dict_with_gap_in_int_keys() {
        // Dict with keys 0, 5 -- next key should be 6 (max + 1)
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::Int(10)));
        map.insert(Key::Int(5), thunk(Value::Int(50)));
        let dict = thunk_dict(map);
        let result = mat(builtin_append(BuiltinArgs {
            args: &[dict, thunk(Value::Int(60))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let val =
                    materialize(map.get(&Key::Int(6)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(val, Value::Int(60));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_preserves_existing_entries() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::String("first".into())));
        let dict = thunk_dict(map);
        let result = mat(builtin_append(BuiltinArgs {
            args: &[dict, thunk(Value::String("second".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let first =
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(first, Value::String("first".into()));
                let second =
                    materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(second, Value::String("second".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_value_stays_as_thunk() {
        // The value arg is not materialized -- it's inserted as a thunk
        let empty = thunk_dict(IndexMap::new());
        let val_thunk = thunk(Value::Int(7));
        let result = mat(builtin_append(BuiltinArgs {
            args: &[empty, Rc::clone(&val_thunk)],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                // The inserted thunk should be the same Rc (not a copy)
                assert!(Rc::ptr_eq(map.get(&Key::Int(0)).unwrap(), &val_thunk));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_wrong_arity_zero() {
        let err = builtin_append(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("2"), "got: {}", err.message());
    }

    #[test]
    fn append_wrong_arity_three() {
        let err = builtin_append(BuiltinArgs {
            args: &[
                thunk_dict(IndexMap::new()),
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("2"), "got: {}", err.message());
    }

    #[test]
    fn append_first_arg_non_dict() {
        let err = builtin_append(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("append"), "got: {}", err.message());
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn append_key_overflow_at_i64_max() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(i64::MAX), thunk(Value::Int(1)));
        let dict = thunk_dict(map);
        let err = builtin_append(BuiltinArgs {
            args: &[dict, thunk(Value::Int(2))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("integer overflow"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn str_no_args() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn str_single_int() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("42".into()));
    }

    #[test]
    fn str_single_negative_int() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Int(-7))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("-7".into()));
    }

    #[test]
    fn str_single_float() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("3.14".into()));
    }

    #[test]
    fn str_single_string() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn str_single_bool_true() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("true".into()));
    }

    #[test]
    fn str_single_bool_false() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Bool(false))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("false".into()));
    }

    #[test]
    fn str_single_dict() {
        let mut map = IndexMap::new();
        map.insert(
            Key::String("x".into()),
            Rc::new(Thunk::new_materialized(
                Value::Int(1),
                test_span(1, 1, 1, 5),
            )),
        );
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Dict(map))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("[x: <thunk>]".into()));
    }

    #[test]
    fn str_single_empty_string() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::String("".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn str_concat_multiple_strings() {
        let args = vec![
            thunk(Value::String("Hello".into())),
            thunk(Value::String(" ".into())),
            thunk(Value::String("World".into())),
        ];
        let result = mat(builtin_str(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Hello World".into()));
    }

    #[test]
    fn str_concat_mixed_types() {
        let args = vec![
            thunk(Value::String("count: ".into())),
            thunk(Value::Int(42)),
            thunk(Value::String(", ratio: ".into())),
            thunk(Value::Float(3.14)),
            thunk(Value::String(", ok: ".into())),
            thunk(Value::Bool(true)),
        ];
        let result = mat(builtin_str(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(
            result,
            Value::String("count: 42, ratio: 3.14, ok: true".into())
        );
    }

    #[test]
    fn split_basic() {
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String("a,b,c".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v0 = materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                let v1 = materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap();
                let v2 = materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(v0, Value::String("a".into()));
                assert_eq!(v1, Value::String("b".into()));
                assert_eq!(v2, Value::String("c".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_empty_parts() {
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String("a,,b".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v1 = materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(v1, Value::String("".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_single_char_separator() {
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String("/".into())),
                thunk(Value::String("a/b/c/d".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => assert_eq!(map.len(), 4),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_no_match() {
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String("hello".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let v0 = materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(v0, Value::String("hello".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_multi_char_separator() {
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String("::".into())),
                thunk(Value::String("a::b::c".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v0 = materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                let v1 = materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap();
                let v2 = materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(v0, Value::String("a".into()));
                assert_eq!(v1, Value::String("b".into()));
                assert_eq!(v2, Value::String("c".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_empty_input() {
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String("".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let v0 = materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(v0, Value::String("".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_parts_limit_exceeded() {
        // Splitting "a" repeated MAX_SPLIT_PARTS+1 times by empty separator produces
        // MAX_SPLIT_PARTS+2 parts, which exceeds the limit.
        // Verifies that ResourceLimitExceeded is returned and that the error fires
        // after at most MAX_SPLIT_PARTS+1 allocations (not after the full split).
        let input = "a".repeat(MAX_SPLIT_PARTS + 1);
        let result = builtin_split(BuiltinArgs {
            args: &[thunk(Value::String("".into())), thunk(Value::String(input))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err(), "expected Err for > MAX_SPLIT_PARTS parts");
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::ResourceLimitExceeded { .. }),
            "expected ResourceLimitExceeded, got {:?}",
            err.kind
        );
    }

    #[test]
    fn split_parts_at_limit_succeeds() {
        // Splitting a string that produces exactly MAX_SPLIT_PARTS parts must succeed
        // (guard is `>`, not `>=`). Construct "a,a,a,...,a" with MAX_SPLIT_PARTS items
        // separated by commas, then split by "," — produces exactly MAX_SPLIT_PARTS parts.
        let input = vec!["a"; MAX_SPLIT_PARTS].join(",");
        let result = builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String(input)),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        let val = match result {
            Ok(t) => mat(Ok(t)),
            Err(e) => panic!("expected Ok for exactly MAX_SPLIT_PARTS parts, got Err: {e:?}"),
        };
        match val {
            Value::Dict(map) => assert_eq!(map.len(), MAX_SPLIT_PARTS),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn replace_basic() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("world".into())),
                thunk(Value::String("Rust".into())),
                thunk(Value::String("hello world".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello Rust".into()));
    }

    #[test]
    fn replace_multiple_occurrences() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("o".into())),
                thunk(Value::String("banana".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("bonono".into()));
    }

    #[test]
    fn replace_no_match() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("xyz".into())),
                thunk(Value::String("abc".into())),
                thunk(Value::String("hello".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn replace_empty_pattern() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("".into())),
                thunk(Value::String("-".into())),
                thunk(Value::String("abc".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("-a-b-c-".into()));
    }

    #[test]
    fn replace_to_empty() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("l".into())),
                thunk(Value::String("".into())),
                thunk(Value::String("hello".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("heo".into()));
    }

    #[test]
    fn replace_output_size_limit_empty_pattern() {
        // Empty pattern with large replacement should error.
        // 1000 chars input, 100k chars replacement -> output would be ~100MB.
        let input = "a".repeat(1000);
        let replacement = "x".repeat(100_000);
        let result = builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("".into())),
                thunk(Value::String(replacement)),
                thunk(Value::String(input)),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("replace: output would exceed"));
    }

    #[test]
    fn replace_output_size_ok_normal_pattern() {
        // Normal pattern replacement should succeed even with moderate sizes.
        let input = "a".repeat(1000);
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("bb".into())),
                thunk(Value::String(input)),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        // 1000 'a' replaced with 'bb' -> 2000 'b'
        assert_eq!(result, Value::String("b".repeat(2000)));
    }

    #[test]
    fn upper_basic() {
        let result = mat(builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("HELLO".into()));
    }

    #[test]
    fn upper_mixed_case() {
        let result = mat(builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String("Hello World".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("HELLO WORLD".into()));
    }

    #[test]
    fn upper_already_upper() {
        let result = mat(builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String("ABC".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("ABC".into()));
    }

    #[test]
    fn upper_empty() {
        let result = mat(builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String("".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn upper_with_numbers() {
        let result = mat(builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String("abc123".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("ABC123".into()));
    }

    #[test]
    fn lower_basic() {
        let result = mat(builtin_lower(BuiltinArgs {
            args: &[thunk(Value::String("HELLO".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn lower_mixed_case() {
        let result = mat(builtin_lower(BuiltinArgs {
            args: &[thunk(Value::String("Hello World".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello world".into()));
    }

    #[test]
    fn lower_already_lower() {
        let result = mat(builtin_lower(BuiltinArgs {
            args: &[thunk(Value::String("abc".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("abc".into()));
    }

    #[test]
    fn lower_empty() {
        let result = mat(builtin_lower(BuiltinArgs {
            args: &[thunk(Value::String("".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn trim_basic() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("  hello  ".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_leading_only() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("   hello".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_trailing_only() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("hello   ".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_no_whitespace() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_all_whitespace() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("   ".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn trim_tabs_and_newlines() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("\t\nhello\n\t".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_empty() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn split_wrong_arity_too_few() {
        let err = builtin_split(BuiltinArgs {
            args: &[thunk(Value::String(",".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected 2"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn split_wrong_arity_too_many() {
        let err = builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String("a,b".into())),
                thunk(Value::String("extra".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn replace_wrong_arity() {
        let err = builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected 3"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn upper_wrong_arity_zero() {
        let err = builtin_upper(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn upper_wrong_arity_two() {
        let err = builtin_upper(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn lower_wrong_arity() {
        let err = builtin_lower(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn trim_wrong_arity() {
        let err = builtin_trim(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn split_wrong_type_separator() {
        let err = builtin_split(BuiltinArgs {
            args: &[thunk(Value::Int(42)), thunk(Value::String("hello".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Int"), "got: {}", err.message());
    }

    #[test]
    fn split_wrong_type_input() {
        let err = builtin_split(BuiltinArgs {
            args: &[thunk(Value::String(",".into())), thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn replace_wrong_type_pattern() {
        let err = builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::Int(1)),
                thunk(Value::String("b".into())),
                thunk(Value::String("abc".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Int"), "got: {}", err.message());
    }

    #[test]
    fn replace_wrong_type_replacement() {
        let err = builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::Bool(true)),
                thunk(Value::String("abc".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Bool"), "got: {}", err.message());
    }

    #[test]
    fn replace_wrong_type_input() {
        let err = builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
                thunk(Value::Float(3.14)),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("got Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn upper_wrong_type() {
        let err = builtin_upper(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Int"), "got: {}", err.message());
    }

    #[test]
    fn lower_wrong_type() {
        let err = builtin_lower(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Bool"), "got: {}", err.message());
    }

    #[test]
    fn trim_wrong_type() {
        let err = builtin_trim(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("got Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn upper_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::String("hi".into())));
        let err = builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn lower_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::String("hi".into())));
        let err = builtin_lower(BuiltinArgs {
            args: &[thunk(Value::String("HELLO".into()))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn trim_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::String("hi".into())));
        let err = builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("  hello  ".into()))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn eval_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_eval(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn error_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_error(BuiltinArgs {
            args: &[thunk(Value::String("boom".into()))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn type_of_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn from_json_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("42".into()))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("42".into()))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn split_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String("a,b".into())),
            ],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn replace_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
                thunk(Value::String("abc".into())),
            ],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn add_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(99)));
        let err = builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn sub_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(3)), thunk(Value::Int(1))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn mul_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(2)), thunk(Value::Int(3))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn div_float_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Int(3))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn eq_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn lt_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn if_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_if(BuiltinArgs {
            args: &[
                thunk(Value::Bool(true)),
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
            ],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn keys_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        let err = builtin_keys(BuiltinArgs {
            args: &[thunk(Value::Dict(map))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn length_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let map = IndexMap::new();
        let err = builtin_length(BuiltinArgs {
            args: &[thunk(Value::Dict(map))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn merge_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_merge(BuiltinArgs {
            args: &[
                thunk(Value::Dict(IndexMap::new())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn append_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_append(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new())), thunk(Value::Int(42))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn str_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_str(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn try_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let func = zero_arg_fn(Expr::Int(42));
        let err = builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn apply_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let func = zero_arg_fn(Expr::Int(42));
        let err = builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(Value::Dict(IndexMap::new()))],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn standard_builtins_contains_all() {
        let builtins = standard_builtins();
        let names: Vec<&str> = builtins.iter().map(|(name, _)| *name).collect();
        // Arithmetic
        assert!(names.contains(&"+"), "missing +");
        assert!(names.contains(&"-"), "missing -");
        assert!(names.contains(&"*"), "missing *");
        assert!(names.contains(&"/"), "missing /");
        // Comparison
        assert!(names.contains(&"="), "missing =");
        assert!(names.contains(&"<"), "missing <");
        // Control
        assert!(names.contains(&"if"), "missing if");
        // Dict primitives
        assert!(names.contains(&"keys"), "missing keys");
        assert!(names.contains(&"length"), "missing length");
        assert!(names.contains(&"merge"), "missing merge");
        assert!(names.contains(&"append"), "missing append");
        // Strings
        assert!(names.contains(&"str"), "missing str");
        assert!(names.contains(&"split"), "missing split");
        assert!(names.contains(&"replace"), "missing replace");
        assert!(names.contains(&"upper"), "missing upper");
        assert!(names.contains(&"lower"), "missing lower");
        assert!(names.contains(&"trim"), "missing trim");
        // Numeric
        assert!(names.contains(&"floor"), "missing floor");
        assert!(names.contains(&"round"), "missing round");
        // Parsing
        assert!(names.contains(&"to-int"), "missing to-int");
        assert!(names.contains(&"to-float"), "missing to-float");
        // Evaluation control
        assert!(names.contains(&"eval"), "missing eval");
        assert!(names.contains(&"error"), "missing error");
        assert!(names.contains(&"try"), "missing try");
        assert!(names.contains(&"apply"), "missing apply");
        // Type introspection
        assert!(names.contains(&"type-of"), "missing type-of");
        // I/O
        assert!(names.contains(&"from-json"), "missing from-json");
        assert!(names.contains(&"include"), "missing include");
        // Sequences
        assert!(names.contains(&"seq"), "missing seq");
        assert!(names.contains(&"head"), "missing head");
        assert!(names.contains(&"tail"), "missing tail");
        assert!(names.contains(&"collect"), "missing collect");
        assert!(names.contains(&"seq?"), "missing seq?");
        assert!(names.contains(&"range"), "missing range");
        assert!(names.contains(&"repeat"), "missing repeat");
        assert!(names.contains(&"cycle"), "missing cycle");
        assert!(names.contains(&"iterate"), "missing iterate");
        assert!(names.contains(&"unfold"), "missing unfold");
        assert!(names.contains(&"map"), "missing map");
        assert!(names.contains(&"filter"), "missing filter");
        assert!(names.contains(&"take"), "missing take");
        assert!(names.contains(&"drop"), "missing drop");
        assert!(names.contains(&"reduce"), "missing reduce");
        assert!(names.contains(&"join"), "missing join");
        assert!(names.contains(&"concat"), "missing concat");
        // Total count
        assert_eq!(names.len(), 46, "expected 46 builtins, got {}", names.len());
    }

    #[test]
    fn add_int_int() {
        let r = mat(builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(3)), thunk(Value::Int(5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(8));
    }

    #[test]
    fn add_int_float() {
        let r = mat(builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(3)), thunk(Value::Float(2.5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(5.5));
    }

    #[test]
    fn add_float_int() {
        let r = mat(builtin_add(BuiltinArgs {
            args: &[thunk(Value::Float(2.5)), thunk(Value::Int(3))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(5.5));
    }

    #[test]
    fn add_float_float() {
        let r = mat(builtin_add(BuiltinArgs {
            args: &[thunk(Value::Float(1.5)), thunk(Value::Float(2.5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(4.0));
    }

    #[test]
    fn add_negative_ints() {
        let r = mat(builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(-10)), thunk(Value::Int(3))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(-7));
    }

    #[test]
    fn add_zeros() {
        let r = mat(builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(Value::Int(0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn add_type_error_string() {
        let e = builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::String("hello".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("type mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn add_arity_one_arg() {
        let e = builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn add_arity_three_args() {
        let e = builtin_add(BuiltinArgs {
            args: &[
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn add_overflow_error() {
        let err = builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(i64::MAX)), thunk(Value::Int(1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("integer overflow"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn sub_int_int() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Int(3))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(7));
    }

    #[test]
    fn sub_int_float() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Float(3.5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(6.5));
    }

    #[test]
    fn sub_float_int() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Float(10.5)), thunk(Value::Int(3))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(7.5));
    }

    #[test]
    fn sub_float_float() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Float(10.5)), thunk(Value::Float(3.5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(7.0));
    }

    #[test]
    fn sub_result_negative() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(3)), thunk(Value::Int(10))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(-7));
    }

    #[test]
    fn sub_to_zero() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn sub_arity_zero_args() {
        let e = builtin_sub(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn sub_arity_one_arg() {
        let e = builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn sub_arity_three_args() {
        let e = builtin_sub(BuiltinArgs {
            args: &[
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn sub_type_error_string() {
        let e = builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::String("hello".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("type mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn mul_int_int() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(4)), thunk(Value::Int(5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(20));
    }

    #[test]
    fn mul_int_float() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(4)), thunk(Value::Float(2.5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(10.0));
    }

    #[test]
    fn mul_float_int() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Float(2.5)), thunk(Value::Int(4))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(10.0));
    }

    #[test]
    fn mul_float_float() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Float(2.5)), thunk(Value::Float(3.0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(7.5));
    }

    #[test]
    fn mul_by_zero() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(42)), thunk(Value::Int(0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn mul_negative() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(-3)), thunk(Value::Int(4))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(-12));
    }

    #[test]
    fn mul_by_negative_one() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(42)), thunk(Value::Int(-1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(-42));
    }

    #[test]
    fn mul_overflow_error() {
        let err = builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(i64::MAX)), thunk(Value::Int(2))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("integer overflow"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn div_float_int_int_returns_float() {
        let r = mat(builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Int(3))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match r {
            Value::Float(f) => {
                assert!((f - 10.0 / 3.0).abs() < 1e-10, "expected ~3.333, got {f}")
            }
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn div_float_int_int_exact_returns_float() {
        let r = mat(builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Int(2))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match r {
            Value::Float(f) => assert_eq!(f, 5.0),
            other => panic!("expected Float(5.0), got {other:?}"),
        }
    }

    #[test]
    fn div_float_int_float() {
        let r = mat(builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Float(3.0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match r {
            Value::Float(f) => {
                assert!((f - 10.0 / 3.0).abs() < 1e-10, "expected ~3.333, got {f}")
            }
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn div_float_float_float() {
        let r = mat(builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Float(7.5)), thunk(Value::Float(2.5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(3.0));
    }

    #[test]
    fn div_float_by_zero_int() {
        let e = builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Int(0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("division by zero"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn div_float_by_zero_float() {
        let e = builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Float(10.0)), thunk(Value::Float(0.0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("division by zero"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn div_float_by_zero_mixed() {
        let e = builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Float(0.0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("division by zero"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn div_float_negative_zero() {
        let r = mat(builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Float(-0.0)), thunk(Value::Float(1.0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(0.0));
    }

    #[test]
    fn eq_int_int_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_int_int_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Int(6))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_float_float_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Float(3.14)), thunk(Value::Float(3.14))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_float_float_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Float(3.14)), thunk(Value::Float(2.71))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_string_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[
                thunk(Value::String("hello".into())),
                thunk(Value::String("hello".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_string_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[
                thunk(Value::String("hello".into())),
                thunk(Value::String("world".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_bool_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Bool(true)), thunk(Value::Bool(true))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_bool_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Bool(true)), thunk(Value::Bool(false))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_cross_type_int_float_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Float(5.0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_cross_type_float_int_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Float(5.0)), thunk(Value::Int(5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_cross_type_int_float_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Float(5.1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_dict_never_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[
                thunk(Value::Dict(IndexMap::new())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_different_types_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::String("1".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_bool_vs_int_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Bool(true)), thunk(Value::Int(1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_nan_not_equal_to_self() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Float(f64::NAN)), thunk(Value::Float(f64::NAN))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_negative_zero_float() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Float(-0.0)), thunk(Value::Float(0.0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_arity_error() {
        let e = builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn lt_int_int_true() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(3)), thunk(Value::Int(5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_int_int_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Int(3))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_int_int_equal_is_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_float_float() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Float(2.5)), thunk(Value::Float(3.5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_string_lexicographic() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[
                thunk(Value::String("apple".into())),
                thunk(Value::String("banana".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_string_lexicographic_reverse() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[
                thunk(Value::String("banana".into())),
                thunk(Value::String("apple".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_string_equal_is_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[
                thunk(Value::String("same".into())),
                thunk(Value::String("same".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_string_prefix() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[
                thunk(Value::String("ab".into())),
                thunk(Value::String("abc".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_cross_type_int_float() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(3)), thunk(Value::Float(3.5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_cross_type_float_int() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Float(2.5)), thunk(Value::Int(3))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_cross_type_equal_values() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Float(5.0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_incompatible_types_error() {
        let e = builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::String("hello".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("type mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn lt_bool_false_lt_true() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Bool(false)), thunk(Value::Bool(true))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_bool_true_lt_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Bool(true)), thunk(Value::Bool(false))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_bool_false_lt_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Bool(false)), thunk(Value::Bool(false))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_bool_true_lt_true() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Bool(true)), thunk(Value::Bool(true))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_dict_error() {
        let e = builtin_lt(BuiltinArgs {
            args: &[
                thunk(Value::Dict(IndexMap::new())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("type mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn lt_arity_error() {
        let e = builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn lt_negative_numbers() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(-10)), thunk(Value::Int(-5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_nan_float() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Float(f64::NAN)), thunk(Value::Float(1.0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn if_true_returns_then_branch() {
        let args = vec![
            thunk(Value::Bool(true)),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let result = mat(builtin_if(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn if_false_returns_else_branch() {
        let args = vec![
            thunk(Value::Bool(false)),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let result = mat(builtin_if(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(99));
    }

    #[test]
    fn if_does_not_materialize_unchosen_else_branch() {
        let error_expr = Rc::new(Spanned::new(
            Expr::VarRef("nonexistent".to_string()),
            test_span(1, 1, 1, 10),
        ));
        let env = Rc::new(RefCell::new(Environment::new()));
        let error_thunk = Rc::new(Thunk::new_unevaluated(
            error_expr,
            env,
            test_ctx(),
            test_span(1, 1, 1, 10),
        ));

        let args = vec![thunk(Value::Bool(true)), thunk(Value::Int(42)), error_thunk];
        let result = mat(builtin_if(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn if_does_not_materialize_unchosen_then_branch() {
        let error_expr = Rc::new(Spanned::new(
            Expr::VarRef("nonexistent".to_string()),
            test_span(1, 1, 1, 10),
        ));
        let env = Rc::new(RefCell::new(Environment::new()));
        let error_thunk = Rc::new(Thunk::new_unevaluated(
            error_expr,
            env,
            test_ctx(),
            test_span(1, 1, 1, 10),
        ));

        let args = vec![
            thunk(Value::Bool(false)),
            error_thunk,
            thunk(Value::Int(99)),
        ];
        let result = mat(builtin_if(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(99));
    }

    #[test]
    fn if_non_bool_condition_error() {
        let args = vec![
            thunk(Value::Int(1)),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let e = builtin_if(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("type mismatch"),
            "got: {}",
            e.message()
        );
        assert!(
            e.message().contains("Bool"),
            "expected Bool mentioned, got: {}",
            e.message()
        );
    }

    #[test]
    fn if_string_condition_error() {
        let args = vec![
            thunk(Value::String("true".into())),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let e = builtin_if(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("type mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn if_arity_too_few() {
        let args = vec![thunk(Value::Bool(true)), thunk(Value::Int(42))];
        let e = builtin_if(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn if_arity_too_many() {
        let args = vec![
            thunk(Value::Bool(true)),
            thunk(Value::Int(1)),
            thunk(Value::Int(2)),
            thunk(Value::Int(3)),
        ];
        let e = builtin_if(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn create_root_env_has_all_builtins() {
        let env = create_root_env();
        let env_ref = env.borrow();
        for (name, _) in standard_builtins() {
            assert!(
                env_ref.get(name).is_some(),
                "root env missing builtin: {name}"
            );
        }
    }

    #[test]
    fn create_stdlib_env_has_builtins_and_prelude() {
        let env = create_stdlib_env().expect("stdlib env creation failed");
        let env_ref = env.borrow();
        // Should have builtins (via parent chain)
        assert!(env_ref.get("+").is_some(), "missing builtin +");
        assert!(env_ref.get("if").is_some(), "missing builtin if");
        // Should have prelude functions
        assert!(env_ref.get("not").is_some(), "missing prelude function not");
        assert!(env_ref.get("map").is_some(), "missing prelude function map");
        assert!(
            env_ref.get("filter").is_some(),
            "missing prelude function filter"
        );
        assert!(
            env_ref.get("identity").is_some(),
            "missing prelude function identity"
        );
    }

    /// Helper: create an EvalContext pointing at the given base directory.
    fn include_ctx(base_dir: &std::path::Path) -> Rc<crate::eval::EvalContext> {
        let stdlib_env = create_stdlib_env().expect("stdlib env");
        crate::eval::EvalContext::new(base_dir.to_path_buf(), stdlib_env, false)
    }

    /// Helper: write a temp file and return its path.
    fn write_temp_file(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write temp file");
        path
    }

    #[test]
    fn include_wrong_type_error() {
        let dir = std::env::temp_dir().join("llt_test_include_type");
        std::fs::create_dir_all(&dir).ok();
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::Int(42))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_file_not_found() {
        let dir = std::env::temp_dir().join("llt_test_include_notfound");
        std::fs::create_dir_all(&dir).ok();
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("nonexistent.llt".into()))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot access"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_simple_dict() {
        let dir = std::env::temp_dir().join("llt_test_include_simple");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "lib.llt", "[x: 42 y: hello]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("lib.llt".into()))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let x = materialize(
                    map.get(&Key::String("x".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(x, Value::Int(42));
                let y = materialize(
                    map.get(&Key::String("y".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(y, Value::String("hello".into()));
            }
            other => panic!("expected Dict, got {:?}", other),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_scalar_value() {
        let dir = std::env::temp_dir().join("llt_test_include_scalar");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "num.llt", "42");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("num.llt".into()))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));
        assert_eq!(result, Value::Int(42));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_parse_error() {
        let dir = std::env::temp_dir().join("llt_test_include_parse_err");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "bad.llt", "[x: ]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("bad.llt".into()))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("parse error"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_circular_detection() {
        let dir = std::env::temp_dir().join("llt_test_include_circular");
        std::fs::create_dir_all(&dir).ok();
        // File A includes file B at top level (not inside a dict entry, so
        // the include is evaluated eagerly during eval_file). File B includes
        // file A the same way, triggering the cycle.
        write_temp_file(&dir, "a.llt", "[call $include \"b.llt\"]");
        write_temp_file(&dir, "b.llt", "[call $include \"a.llt\"]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("a.llt".into()))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("circular include"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_self_circular() {
        let dir = std::env::temp_dir().join("llt_test_include_self");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "self.llt", "[call $include \"self.llt\"]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("self.llt".into()))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("circular include"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_nested() {
        let dir = std::env::temp_dir().join("llt_test_include_nested");
        std::fs::create_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("sub")).ok();
        write_temp_file(
            &dir,
            "outer.llt",
            "[inner: [call $include \"sub/inner.llt\"]]",
        );
        write_temp_file(&dir.join("sub"), "inner.llt", "[val: 99]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("outer.llt".into()))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                let inner = materialize(
                    map.get(&Key::String("inner".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                match inner {
                    Value::Dict(inner_map) => {
                        let val = materialize(
                            inner_map.get(&Key::String("val".into())).unwrap(),
                            None,
                            &test_ctx(),
                            0,
                        )
                        .unwrap();
                        assert_eq!(val, Value::Int(99));
                    }
                    other => panic!("expected inner Dict, got {:?}", other),
                }
            }
            other => panic!("expected Dict, got {:?}", other),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_absolute_path() {
        let dir = std::env::temp_dir().join("llt_test_include_abs");
        std::fs::create_dir_all(&dir).ok();
        let file_path = write_temp_file(&dir, "abs.llt", "[val: 77]");
        // Use a different directory as base to prove absolute path works
        let other_dir = std::env::temp_dir().join("llt_test_include_abs_other");
        std::fs::create_dir_all(&other_dir).ok();
        let ctx = include_ctx(&other_dir);

        let args = vec![thunk(Value::String(
            file_path.to_string_lossy().into_owned(),
        ))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));
        match result {
            Value::Dict(map) => {
                let val = materialize(
                    map.get(&Key::String("val".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(val, Value::Int(77));
            }
            other => panic!("expected Dict, got {:?}", other),
        }
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other_dir).ok();
    }

    #[test]
    fn include_arity_error() {
        let dir = std::env::temp_dir().join("llt_test_include_arity");
        std::fs::create_dir_all(&dir).ok();
        let ctx = include_ctx(&dir);

        // No arguments
        let err = builtin_include(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );

        // Two arguments
        let args = vec![
            thunk(Value::String("a.llt".into())),
            thunk(Value::String("b.llt".into())),
        ];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_rejects_named_args() {
        let dir = std::env::temp_dir().join("llt_test_include_named");
        std::fs::create_dir_all(&dir).ok();
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("test.llt".into()))];
        let mut named = IndexMap::new();
        named.insert("path".to_string(), thunk(Value::String("x".into())));
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("does not accept named arguments"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_multi_document() {
        let dir = std::env::temp_dir().join("llt_test_include_multidoc");
        std::fs::create_dir_all(&dir).ok();
        // Two documents: first produces [x: 10], $$ pipeline passes to second
        write_temp_file(&dir, "multi.llt", "[x: 10]\n---\n[y: $$.x]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("multi.llt".into()))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));
        match result {
            Value::Dict(map) => {
                let y = materialize(
                    map.get(&Key::String("y".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(y, Value::Int(10));
            }
            other => panic!("expected Dict, got {:?}", other),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_uses_stdlib() {
        // The included file should have access to stdlib builtins
        let dir = std::env::temp_dir().join("llt_test_include_stdlib");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "stdlib_test.llt", "[result: [call $+ 1 2]]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("stdlib_test.llt".into()))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));
        match result {
            Value::Dict(map) => {
                let val = materialize(
                    map.get(&Key::String("result".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(val, Value::Int(3));
            }
            other => panic!("expected Dict, got {:?}", other),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_caches_result() {
        // Including the same file twice should return the cached result, not re-evaluate.
        let dir = std::env::temp_dir().join("llt_test_include_cache");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "cached.llt", "[value: 42]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("cached.llt".into()))];

        // First include
        let result1 = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Second include -- should hit cache
        let result2 = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));

        // Both should return the same value
        match (&result1, &result2) {
            (Value::Dict(map1), Value::Dict(map2)) => {
                let val1 = materialize(
                    map1.get(&Key::String("value".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                let val2 = materialize(
                    map2.get(&Key::String("value".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(val1, Value::Int(42));
                assert_eq!(val2, Value::Int(42));
            }
            _ => panic!("expected Dict, got {:?} and {:?}", result1, result2),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_cache_respects_normalization() {
        // Including a file via different paths that resolve to the same canonical path
        // should hit the cache.
        let dir = std::env::temp_dir().join("llt_test_include_cache_norm");
        std::fs::create_dir_all(&dir).ok();
        let subdir = dir.join("subdir");
        std::fs::create_dir_all(&subdir).ok();
        write_temp_file(&dir, "target.llt", "[value: 99]");
        let ctx = include_ctx(&dir);

        // First include with relative path
        let args1 = vec![thunk(Value::String("./target.llt".into()))];
        let result1 = mat(builtin_include(BuiltinArgs {
            args: &args1,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Second include with normalized path
        let args2 = vec![thunk(Value::String("subdir/../target.llt".into()))];
        let result2 = mat(builtin_include(BuiltinArgs {
            args: &args2,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));

        // Both should return the same value
        match (&result1, &result2) {
            (Value::Dict(map1), Value::Dict(map2)) => {
                let val1 = materialize(
                    map1.get(&Key::String("value".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                let val2 = materialize(
                    map2.get(&Key::String("value".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(val1, Value::Int(99));
                assert_eq!(val2, Value::Int(99));
            }
            _ => panic!("expected Dict, got {:?} and {:?}", result1, result2),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_cache_shared_across_nested() {
        // File A includes file B. File C also includes file B. Both should share
        // the cached result of B.
        let dir = std::env::temp_dir().join("llt_test_include_cache_nested");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "shared.llt", "[shared: 123]");
        write_temp_file(&dir, "file_a.llt", "[a: [call $include \"shared.llt\"]]");
        write_temp_file(&dir, "file_c.llt", "[c: [call $include \"shared.llt\"]]");
        let ctx = include_ctx(&dir);

        // Include file_a (which includes shared.llt)
        let args_a = vec![thunk(Value::String("file_a.llt".into()))];
        let result_a = mat(builtin_include(BuiltinArgs {
            args: &args_a,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Include file_c (which also includes shared.llt -- should hit cache)
        let args_c = vec![thunk(Value::String("file_c.llt".into()))];
        let result_c = mat(builtin_include(BuiltinArgs {
            args: &args_c,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));

        // Verify that both got the shared value
        match (&result_a, &result_c) {
            (Value::Dict(map_a), Value::Dict(map_c)) => {
                let a_val = materialize(
                    map_a.get(&Key::String("a".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                let c_val = materialize(
                    map_c.get(&Key::String("c".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();

                // Both should be dicts with "shared: 123"
                match (&a_val, &c_val) {
                    (Value::Dict(a_inner), Value::Dict(c_inner)) => {
                        let a_shared = materialize(
                            a_inner.get(&Key::String("shared".into())).unwrap(),
                            None,
                            &test_ctx(),
                            0,
                        )
                        .unwrap();
                        let c_shared = materialize(
                            c_inner.get(&Key::String("shared".into())).unwrap(),
                            None,
                            &test_ctx(),
                            0,
                        )
                        .unwrap();
                        assert_eq!(a_shared, Value::Int(123));
                        assert_eq!(c_shared, Value::Int(123));
                    }
                    _ => panic!("expected nested dicts, got {:?} and {:?}", a_val, c_val),
                }
            }
            _ => panic!("expected Dict, got {:?} and {:?}", result_a, result_c),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_forbidden_when_no_fs() {
        // When no_fs is true, $include should return IncludeForbidden error
        let dir = std::env::temp_dir().join("llt_test_include_no_fs");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "test.llt", "[x: 42]");

        // Create context with no_fs: true
        let stdlib_env = create_stdlib_env().expect("stdlib env");
        let ctx = crate::eval::EvalContext::new(dir.clone(), stdlib_env, true);

        let args = vec![thunk(Value::String("test.llt".into()))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();

        // Check error message and code
        let error_msg = format!("{}", err);
        assert!(
            error_msg.contains("filesystem access is disabled"),
            "got: {}",
            error_msg
        );
        assert!(
            error_msg.contains("[E042]"),
            "missing error code [E042], got: {}",
            error_msg
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Sequence builtins tests

    #[test]
    fn seq_basic() {
        let head_val = thunk(Value::Int(1));
        let tail_val = thunk(Value::Int(2));
        let result = mat(builtin_seq(BuiltinArgs {
            args: &[head_val.clone(), tail_val.clone()],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                assert_eq!(
                    materialize(&tail, None, &test_ctx(), 0).unwrap(),
                    Value::Int(2)
                );
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn seq_arity_zero() {
        let result = builtin_seq(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn seq_arity_one() {
        let result = builtin_seq(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn seq_arity_three() {
        let result = builtin_seq(BuiltinArgs {
            args: &[
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn seq_lazy() {
        // Head can be a thunk wrapping a VarRef to a nonexistent variable.
        // If we tried to materialize this thunk, it would error (undefined variable).
        // But seq construction should succeed because it doesn't materialize args.
        let undef_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(Spanned::new(
                Expr::VarRef("undefined_var".to_string()),
                test_span(1, 1, 1, 5),
            )),
            Rc::new(RefCell::new(Environment::new())),
            test_ctx(),
            test_span(1, 1, 1, 5),
        ));
        let tail_val = thunk(Value::Int(2));
        // seq construction should succeed even though head would error if materialized
        let result = builtin_seq(BuiltinArgs {
            args: &[undef_thunk, tail_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_ok());
        // Verify the result is a Seq
        match mat(result) {
            Value::Seq { .. } => {} // Success
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn head_basic() {
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::String("first".into())),
            tail: thunk(Value::Dict(IndexMap::new())),
        });
        let result = builtin_head(BuiltinArgs {
            args: &[seq_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_ok());
        let head = mat(result);
        assert_eq!(head, Value::String("first".into()));
    }

    #[test]
    fn head_non_seq() {
        let result = builtin_head(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn head_arity_zero() {
        let result = builtin_head(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn head_arity_two() {
        let result = builtin_head(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn head_empty_dict() {
        let result = builtin_head(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        let err = result.unwrap_err();
        assert!(
            err.message().contains("on empty collection"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn tail_empty_dict() {
        let result = builtin_tail(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        let err = result.unwrap_err();
        assert!(
            err.message().contains("on empty collection"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn tail_basic() {
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::String("first".into())),
            tail: thunk(Value::Int(99)),
        });
        let result = builtin_tail(BuiltinArgs {
            args: &[seq_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_ok());
        let tail = mat(result);
        assert_eq!(tail, Value::Int(99));
    }

    #[test]
    fn tail_non_seq() {
        let result = builtin_tail(BuiltinArgs {
            args: &[thunk(Value::String("not a seq".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn collect_basic() {
        // Build a 3-element sequence: Seq(1, Seq(2, Seq(3, {})))
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Seq {
                head: thunk(Value::Int(2)),
                tail: thunk(Value::Seq {
                    head: thunk(Value::Int(3)),
                    tail: thunk(Value::Dict(IndexMap::new())),
                }),
            }),
        });
        let result = mat(builtin_collect(BuiltinArgs {
            args: &[seq_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(2)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(3)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn collect_empty_tail() {
        // Single element: Seq(42, {})
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::Int(42)),
            tail: thunk(Value::Dict(IndexMap::new())),
        });
        let result = mat(builtin_collect(BuiltinArgs {
            args: &[seq_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(42)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn collect_non_seq() {
        let result = builtin_collect(BuiltinArgs {
            args: &[thunk(Value::Int(123))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn collect_invalid_tail() {
        // Seq with non-empty dict as tail (should error)
        let mut map = IndexMap::new();
        map.insert(Key::String("x".into()), thunk(Value::Int(1)));
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Dict(map)),
        });
        let result = builtin_collect(BuiltinArgs {
            args: &[seq_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn collect_large_sequence() {
        // Test collect with a moderately-sized sequence (200 elements) to verify it works
        // correctly without hitting MAX_EVAL_DEPTH (256) or MAX_COLLECT_SIZE (1M).
        // Testing at the actual MAX_COLLECT_SIZE (1M) would be too slow/memory-intensive,
        // and with depth increment fixes, sequences hit MAX_EVAL_DEPTH around 256 elements.
        let range_result = builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap();

        let take_result = builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(200)), range_result],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap();

        let collect_result = builtin_collect(BuiltinArgs {
            args: &[take_result],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });

        assert!(
            collect_result.is_ok(),
            "collect should succeed for 200 elements"
        );
        match materialize(&collect_result.unwrap(), None, &test_ctx(), 0).unwrap() {
            Value::Dict(map) => {
                assert_eq!(map.len(), 200);
                // Spot-check first and last elements
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(0)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(199)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(199)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn collect_max_size_limit_enforced() {
        // Test that the MAX_COLLECT_SIZE check is present and triggers correctly.
        // We can't practically test with 1M+ elements in a unit test (too slow/memory-intensive),
        // but we can test that attempting to collect from an unbounded sequence without $take
        // will eventually hit either MAX_EVAL_DEPTH or MAX_COLLECT_SIZE.
        //
        // This test verifies the error message is correct for the MAX_COLLECT_SIZE path.
        // The actual limit boundary (1M vs 1M+1) is tested by the corpus test
        // concat_large_seq.llt-eval which creates 300-element sequences.
        //
        // Run in a thread with larger stack to avoid Rust stack overflow when testing
        // depth-exceeded behavior (same pattern as corpus test runners and join_seq_size_limit).
        let result = std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                let range_result = builtin_range(BuiltinArgs {
                    args: &[thunk(Value::Int(0))],
                    named: &no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Attempt to collect infinite range without take
                // This will hit MAX_EVAL_DEPTH (256) before MAX_COLLECT_SIZE (1M)
                // due to depth accumulation in the PendingBuiltin chain.
                let collect_result = builtin_collect(BuiltinArgs {
                    args: &[range_result],
                    named: &no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                });

                // Should fail (either MAX_EVAL_DEPTH or MAX_COLLECT_SIZE)
                assert!(
                    collect_result.is_err(),
                    "collect should fail on infinite sequence"
                );
                let err = collect_result.unwrap_err();
                // Accept either error - both are valid protections
                let is_depth_error = err.message().contains("maximum evaluation depth");
                let is_size_error = err.message().contains("exceeded maximum collection size");
                assert!(
                    is_depth_error || is_size_error,
                    "expected depth or size limit error, got: {}",
                    err.message()
                );
            })
            .unwrap()
            .join();

        // Propagate any panic from the spawned thread
        result.unwrap();
    }

    #[test]
    fn seq_check_true() {
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Dict(IndexMap::new())),
        });
        let result = mat(builtin_seq_check(BuiltinArgs {
            args: &[seq_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn seq_check_false() {
        let result = mat(builtin_seq_check(BuiltinArgs {
            args: &[thunk(Value::String("not a seq".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(false));
    }

    #[test]
    fn seq_check_dict() {
        let result = mat(builtin_seq_check(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(false));
    }

    // === range builtin tests ===

    #[test]
    fn range_finite_basic() {
        // range(0, 5) → 0, 1, 2, 3, 4
        let result = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(Value::Int(5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(0)
                );
                // Materialize tail to get next element
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Seq { head: h2, .. } => {
                        assert_eq!(
                            materialize(&h2, None, &test_ctx(), 0).unwrap(),
                            Value::Int(1)
                        );
                    }
                    other => panic!("expected Seq for tail, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn range_empty() {
        // range(5, 5) → empty
        let result = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[test]
    fn range_negative_range() {
        // range(10, 5) → empty (start >= end)
        let result = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Int(5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[test]
    fn range_single_element() {
        // range(0, 1) → 0
        let result = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(Value::Int(1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(0)
                );
                // tail should be empty (terminal)
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Dict(map) if map.is_empty() => {} // Success
                    other => panic!("expected empty dict for tail, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn range_infinite_basic() {
        // range(0) → 0, 1, 2, ... (take first 3)
        let result = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(0)
                );
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        assert_eq!(
                            materialize(&h2, None, &test_ctx(), 0).unwrap(),
                            Value::Int(1)
                        );
                        let t2_val = materialize(&t2, None, &test_ctx(), 0).unwrap();
                        match t2_val {
                            Value::Seq { head: h3, .. } => {
                                assert_eq!(
                                    materialize(&h3, None, &test_ctx(), 0).unwrap(),
                                    Value::Int(2)
                                );
                            }
                            other => panic!("expected Seq, got {:?}", other),
                        }
                    }
                    other => panic!("expected Seq, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn range_arity_zero() {
        let result = builtin_range(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn range_arity_three() {
        let result = builtin_range(BuiltinArgs {
            args: &[
                thunk(Value::Int(0)),
                thunk(Value::Int(5)),
                thunk(Value::Int(10)),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn range_non_int_start() {
        let result = builtin_range(BuiltinArgs {
            args: &[thunk(Value::String("not an int".into()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn range_non_int_end() {
        let result = builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(Value::Float(5.5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    // === repeat builtin tests ===

    #[test]
    fn repeat_basic() {
        // repeat(42) → 42, 42, 42, ... (take first 3)
        let result = mat(builtin_repeat(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(42)
                );
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        assert_eq!(
                            materialize(&h2, None, &test_ctx(), 0).unwrap(),
                            Value::Int(42)
                        );
                        let t2_val = materialize(&t2, None, &test_ctx(), 0).unwrap();
                        match t2_val {
                            Value::Seq { head: h3, .. } => {
                                assert_eq!(
                                    materialize(&h3, None, &test_ctx(), 0).unwrap(),
                                    Value::Int(42)
                                );
                            }
                            other => panic!("expected Seq, got {:?}", other),
                        }
                    }
                    other => panic!("expected Seq, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn repeat_laziness() {
        // Repeat an unevaluated thunk (would error if materialized)
        let undef_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(Spanned::new(
                Expr::VarRef("undefined_var".to_string()),
                test_span(1, 1, 1, 5),
            )),
            Rc::new(RefCell::new(Environment::new())),
            test_ctx(),
            test_span(1, 1, 1, 5),
        ));
        // repeat construction should succeed without materializing arg
        let result = builtin_repeat(BuiltinArgs {
            args: &[undef_thunk],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_ok());
        match mat(result) {
            Value::Seq { .. } => {} // Success
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn repeat_arity_zero() {
        let result = builtin_repeat(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn repeat_arity_two() {
        let result = builtin_repeat(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    // === cycle builtin tests ===

    #[test]
    fn cycle_basic() {
        // cycle([a, b]) → a, b, a, b, ... (take first 4)
        let mut map = IndexMap::new();
        map.insert(Key::String("x".into()), thunk(Value::String("a".into())));
        map.insert(Key::String("y".into()), thunk(Value::String("b".into())));
        let dict_val = thunk(Value::Dict(map));

        let result = mat(builtin_cycle(BuiltinArgs {
            args: &[dict_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                // First element: "a"
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::String("a".into())
                );
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        // Second element: "b"
                        assert_eq!(
                            materialize(&h2, None, &test_ctx(), 0).unwrap(),
                            Value::String("b".into())
                        );
                        let t2_val = materialize(&t2, None, &test_ctx(), 0).unwrap();
                        match t2_val {
                            Value::Seq { head: h3, tail: t3 } => {
                                // Third element: "a" (cycling back)
                                assert_eq!(
                                    materialize(&h3, None, &test_ctx(), 0).unwrap(),
                                    Value::String("a".into())
                                );
                                let t3_val = materialize(&t3, None, &test_ctx(), 0).unwrap();
                                match t3_val {
                                    Value::Seq { head: h4, .. } => {
                                        // Fourth element: "b"
                                        assert_eq!(
                                            materialize(&h4, None, &test_ctx(), 0).unwrap(),
                                            Value::String("b".into())
                                        );
                                    }
                                    other => panic!("expected Seq, got {:?}", other),
                                }
                            }
                            other => panic!("expected Seq, got {:?}", other),
                        }
                    }
                    other => panic!("expected Seq, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn cycle_empty_dict() {
        let result = builtin_cycle(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().message().contains("empty"));
    }

    #[test]
    fn cycle_non_dict() {
        let result = builtin_cycle(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn cycle_arity_zero() {
        let result = builtin_cycle(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    // === iterate builtin tests ===

    #[test]
    fn iterate_basic() {
        // iterate(+1, 0) → 0, 1, 2, ... (test structure)
        // For this test, we'll just verify the structure is correct
        // The tail is PendingBuiltin(iterate, [f, PendingCall(f, [x])])
        // Materializing it succeeds (returns another Seq), but materializing
        // the head of *that* Seq would error because f is Int(999), not a function
        let f_thunk = thunk(Value::Int(999)); // dummy, won't be called in structure test
        let x_thunk = thunk(Value::Int(0));

        let result = mat(builtin_iterate(BuiltinArgs {
            args: &[f_thunk, x_thunk.clone()],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                // Head should be x (0)
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(0)
                );
                // Tail is a PendingBuiltin wrapping iterate(f, f(x))
                // Materializing it returns another Seq (doesn't error yet)
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Seq { head: h2, .. } => {
                        // Trying to materialize h2 (which is PendingCall(Int(999), [Int(0)]))
                        // will error because Int(999) is not a function
                        let h2_result = materialize(&h2, None, &test_ctx(), 0);
                        assert!(h2_result.is_err());
                    }
                    other => panic!("expected Seq for tail, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn iterate_laziness() {
        // iterate doesn't materialize its args
        let undef_f = Rc::new(Thunk::new_unevaluated(
            Rc::new(Spanned::new(
                Expr::VarRef("undefined_f".to_string()),
                test_span(1, 1, 1, 5),
            )),
            Rc::new(RefCell::new(Environment::new())),
            test_ctx(),
            test_span(1, 1, 1, 5),
        ));
        let undef_x = Rc::new(Thunk::new_unevaluated(
            Rc::new(Spanned::new(
                Expr::VarRef("undefined_x".to_string()),
                test_span(1, 1, 1, 5),
            )),
            Rc::new(RefCell::new(Environment::new())),
            test_ctx(),
            test_span(1, 1, 1, 5),
        ));
        let result = builtin_iterate(BuiltinArgs {
            args: &[undef_f, undef_x],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_ok());
        match mat(result) {
            Value::Seq { .. } => {} // Success
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn iterate_arity_one() {
        let result = builtin_iterate(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    // === unfold builtin tests ===

    #[test]
    fn unfold_basic_termination() {
        // unfold with a step that immediately returns empty dict (termination)
        // We can't easily test a full unfold without a real function, but we can
        // test that it returns a PendingBuiltin
        let step_thunk = thunk(Value::Int(999)); // dummy
        let seed_thunk = thunk(Value::Int(0));

        let result = builtin_unfold(BuiltinArgs {
            args: &[step_thunk, seed_thunk],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_ok());
        // Result is a PendingBuiltin, not yet materialized
        // Materializing it would call unfold_step, which would error because
        // step is Int(999), not a function
        let result_val = materialize(&result.unwrap(), None, &test_ctx(), 0);
        assert!(result_val.is_err());
    }

    #[test]
    fn unfold_arity_one() {
        let result = builtin_unfold(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    // === take builtin tests ===

    #[test]
    fn take_dict_basic() {
        // take(2, [a: 1, b: 2, c: 3]) → [a: 1, b: 2]
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        map.insert(Key::String("b".into()), thunk(Value::Int(2)));
        map.insert(Key::String("c".into()), thunk(Value::Int(3)));
        let dict_val = thunk(Value::Dict(map));

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(2)), dict_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    materialize(
                        map.get(&Key::String("a".into())).unwrap(),
                        None,
                        &test_ctx(),
                        0
                    )
                    .unwrap(),
                    Value::Int(1)
                );
                assert_eq!(
                    materialize(
                        map.get(&Key::String("b".into())).unwrap(),
                        None,
                        &test_ctx(),
                        0
                    )
                    .unwrap(),
                    Value::Int(2)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn take_dict_zero() {
        // take(0, dict) → []
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        let dict_val = thunk(Value::Dict(map));

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(0)), dict_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[test]
    fn take_dict_negative() {
        // take(-5, dict) → []
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        let dict_val = thunk(Value::Dict(map));

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(-5)), dict_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[test]
    fn take_dict_more_than_length() {
        // take(10, [a: 1, b: 2]) → [a: 1, b: 2]
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        map.insert(Key::String("b".into()), thunk(Value::Int(2)));
        let dict_val = thunk(Value::Dict(map));

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(10)), dict_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn take_seq_basic() {
        // Build a 3-element sequence: Seq(1, Seq(2, Seq(3, {})))
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Seq {
                head: thunk(Value::Int(2)),
                tail: thunk(Value::Seq {
                    head: thunk(Value::Int(3)),
                    tail: thunk(Value::Dict(IndexMap::new())),
                }),
            }),
        });

        // take(2, seq) → Seq(1, Seq(2, []))
        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(2)), seq_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        assert_eq!(
                            materialize(&h2, None, &test_ctx(), 0).unwrap(),
                            Value::Int(2)
                        );
                        // tail of tail should be empty dict (terminal)
                        let t2_val = materialize(&t2, None, &test_ctx(), 0).unwrap();
                        match t2_val {
                            Value::Dict(map) if map.is_empty() => {} // Success
                            other => panic!("expected empty dict, got {:?}", other),
                        }
                    }
                    other => panic!("expected Seq, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn take_seq_zero() {
        // take(0, seq) → []
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Dict(IndexMap::new())),
        });

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(0)), seq_val],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[test]
    fn take_n_non_int() {
        let result = builtin_take(BuiltinArgs {
            args: &[thunk(Value::String("not int".into())), thunk(Value::Int(1))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn take_xs_non_dict_or_seq() {
        let result = builtin_take(BuiltinArgs {
            args: &[
                thunk(Value::Int(5)),
                thunk(Value::String("not dict or seq".into())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn take_arity_one() {
        let result = builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(5))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn concat_seq() {
        // Build two 2-element sequences and concat them
        // xs = Seq(1, Seq(2, {}))
        let xs = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Seq {
                head: thunk(Value::Int(2)),
                tail: thunk(Value::Dict(IndexMap::new())),
            }),
        });

        // ys = Seq(3, Seq(4, {}))
        let ys = thunk(Value::Seq {
            head: thunk(Value::Int(3)),
            tail: thunk(Value::Seq {
                head: thunk(Value::Int(4)),
                tail: thunk(Value::Dict(IndexMap::new())),
            }),
        });

        // concat(xs, ys) should produce Seq(1, Seq(2, Seq(3, Seq(4, {}))))
        let result = builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap();

        // Materialize the result to verify structure
        let result_val = materialize(&result, None, &test_ctx(), 0).unwrap();
        match result_val {
            Value::Seq { head: h1, tail: t1 } => {
                assert_eq!(
                    materialize(&h1, None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                let t1_val = materialize(&t1, None, &test_ctx(), 0).unwrap();
                match t1_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        assert_eq!(
                            materialize(&h2, None, &test_ctx(), 0).unwrap(),
                            Value::Int(2)
                        );
                        let t2_val = materialize(&t2, None, &test_ctx(), 0).unwrap();
                        match t2_val {
                            Value::Seq { head: h3, tail: t3 } => {
                                assert_eq!(
                                    materialize(&h3, None, &test_ctx(), 0).unwrap(),
                                    Value::Int(3)
                                );
                                let t3_val = materialize(&t3, None, &test_ctx(), 0).unwrap();
                                match t3_val {
                                    Value::Seq { head: h4, tail: t4 } => {
                                        assert_eq!(
                                            materialize(&h4, None, &test_ctx(), 0).unwrap(),
                                            Value::Int(4)
                                        );
                                        let t4_val =
                                            materialize(&t4, None, &test_ctx(), 0).unwrap();
                                        match t4_val {
                                            Value::Dict(map) if map.is_empty() => {} // Success
                                            other => panic!("expected empty dict, got {:?}", other),
                                        }
                                    }
                                    other => panic!("expected Seq, got {:?}", other),
                                }
                            }
                            other => panic!("expected Seq, got {:?}", other),
                        }
                    }
                    other => panic!("expected Seq, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn concat_seq_empty_xs() {
        // concat({}, ys) should return ys
        let xs = thunk(Value::Dict(IndexMap::new()));
        let ys = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Dict(IndexMap::new())),
        });

        let result = builtin_concat(BuiltinArgs {
            args: &[xs, ys.clone()],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap();

        // Result should be ys (the same thunk)
        assert!(Rc::ptr_eq(&result, &ys));
    }

    #[test]
    fn concat_seq_empty_ys() {
        // concat(xs, {}) should return xs's elements followed by empty dict
        let xs = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Dict(IndexMap::new())),
        });
        let ys = thunk(Value::Dict(IndexMap::new()));

        let result = builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap();

        // Materialize to verify: Seq(1, {})
        let result_val = materialize(&result, None, &test_ctx(), 0).unwrap();
        match result_val {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Dict(map) if map.is_empty() => {} // Success
                    other => panic!("expected empty dict, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn concat_dict() {
        // concat([1, 2], [3, 4]) -> [1, 2, 3, 4] with integer reindexing
        let mut xs_map = IndexMap::new();
        xs_map.insert(Key::Int(0), thunk(Value::Int(1)));
        xs_map.insert(Key::Int(1), thunk(Value::Int(2)));
        let xs = thunk(Value::Dict(xs_map));

        let mut ys_map = IndexMap::new();
        ys_map.insert(Key::Int(0), thunk(Value::Int(3)));
        ys_map.insert(Key::Int(1), thunk(Value::Int(4)));
        let ys = thunk(Value::Dict(ys_map));

        let result = mat(builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));

        match result {
            Value::Dict(ref map) => {
                assert_eq!(map.len(), 4);
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(2)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(3)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(3)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(4)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn join_seq_size_limit() {
        // Test that join enforces MAX_COLLECT_SIZE on sequence iteration.
        // Similar to collect_max_size_limit_enforced, we verify that attempting to join
        // an unbounded sequence will hit either MAX_EVAL_DEPTH or MAX_COLLECT_SIZE.
        //
        // Run in a thread with larger stack to avoid Rust stack overflow when testing
        // depth-exceeded behavior (same pattern as corpus test runners).
        let result = std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                let range_result = builtin_range(BuiltinArgs {
                    args: &[thunk(Value::Int(0))],
                    named: &no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Attempt to join infinite range without take
                // This will hit MAX_EVAL_DEPTH (256) before MAX_COLLECT_SIZE (1M)
                // due to depth accumulation in the sequence traversal.
                let join_result = builtin_join(BuiltinArgs {
                    args: &[thunk(Value::String(",".to_string())), range_result],
                    named: &no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                });

                // Should fail (either MAX_EVAL_DEPTH or MAX_COLLECT_SIZE)
                assert!(
                    join_result.is_err(),
                    "join should fail on infinite sequence"
                );
                let err = join_result.unwrap_err();
                // Accept either error - both are valid protections
                let is_depth_error = err.message().contains("maximum evaluation depth");
                let is_size_error = err.message().contains("sequence exceeds");
                assert!(
                    is_depth_error || is_size_error,
                    "expected depth or size limit error, got: {}",
                    err.message()
                );
            })
            .unwrap()
            .join();

        // Propagate any panic from the spawned thread
        result.unwrap();
    }

    #[test]
    fn join_empty_dict() {
        // Task 3: Test $join with empty Dict to verify the parts.is_empty() guard
        // prevents saturating_sub(1) wraparound
        let result = mat(builtin_join(BuiltinArgs {
            args: &[
                thunk(Value::String(",".to_string())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String(String::new()));
    }

    #[test]
    fn concat_dict_basic() {
        // Task 4: Test $concat with two small dicts to verify correct behavior
        // This exercises the checked_add call site that prevents integer overflow
        let mut dict1 = IndexMap::new();
        dict1.insert(Key::String("a".into()), thunk(Value::Int(1)));
        dict1.insert(Key::String("b".into()), thunk(Value::Int(2)));

        let mut dict2 = IndexMap::new();
        dict2.insert(Key::String("c".into()), thunk(Value::Int(3)));
        dict2.insert(Key::String("d".into()), thunk(Value::Int(4)));

        let result = mat(builtin_concat(BuiltinArgs {
            args: &[thunk(Value::Dict(dict1)), thunk(Value::Dict(dict2))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));

        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 4);
                // All values should be reindexed with integer keys 0, 1, 2, 3
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(2)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(3)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(3)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(4)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn take_large_count_infinite_seq_depth_exceeded() {
        // Verify that $take with a count exceeding MAX_EVAL_DEPTH on an infinite sequence
        // hits the depth limit due to depth accumulation in the recursive PendingBuiltin chain.
        // This test verifies the fix where builtin_take passes depth+1 (not depth) when
        // creating the tail thunk.
        //
        // With the fix: depth accumulates as 1→2→...→257, hitting the depth > MAX_EVAL_DEPTH (256) guard.
        // (The initial call is at depth=0; each PendingBuiltin tail is created with depth+1,
        // so the chain of PendingBuiltin depths starts at 1.)
        // Without the fix: depth stays constant, allowing unbounded sequences.
        //
        // Run in a thread with larger stack to avoid Rust stack overflow.
        let result = std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                // Create infinite range starting at 0
                let range_result = builtin_range(BuiltinArgs {
                    args: &[thunk(Value::Int(0))],
                    named: &no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Try to take 260 elements (slightly more than MAX_EVAL_DEPTH=256)
                // This ensures we hit the depth limit.
                let take_result = builtin_take(BuiltinArgs {
                    args: &[thunk(Value::Int(260)), range_result],
                    named: &no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Force the entire sequence by calling collect
                let collect_result = builtin_collect(BuiltinArgs {
                    args: &[take_result],
                    named: &no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                });

                // Should fail with depth exceeded
                assert!(
                    collect_result.is_err(),
                    "collect(take(260, range(0))) should hit depth limit"
                );
                let err = collect_result.unwrap_err();
                assert!(
                    err.message().contains("maximum evaluation depth"),
                    "expected depth limit error, got: {}",
                    err.message()
                );
            })
            .unwrap()
            .join();

        assert!(result.is_ok(), "test thread panicked: {:?}", result);
    }

    #[test]
    fn test_proxy_returns_proxy_value() {
        let handler = thunk(Value::Int(42));
        let result = builtin_proxy(BuiltinArgs {
            args: &[handler.clone()],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap();

        let val = mat(Ok(result));
        match val {
            Value::Proxy { handler: h } => {
                // Verify the handler thunk is the same Rc
                assert!(Rc::ptr_eq(&h, &handler));
            }
            other => panic!("expected Proxy, got {:?}", other),
        }
    }

    #[test]
    fn test_proxy_arity_error() {
        // Zero args
        let err = builtin_proxy(BuiltinArgs {
            args: &[],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );

        // Two args
        let err = builtin_proxy(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );

        // Three args
        let err = builtin_proxy(BuiltinArgs {
            args: &[
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            named: &no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_proxy_named_arg_error() {
        let mut named = IndexMap::new();
        named.insert("handler".to_string(), thunk(Value::Int(42)));

        let err = builtin_proxy(BuiltinArgs {
            args: &[],
            named: &named,
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("does not accept named arguments"),
            "got: {}",
            err.message()
        );
    }
}
