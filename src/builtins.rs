//! Rust-native builtin functions for the LLT language.
//!
//! All builtins follow the `BuiltinFn` signature:
//! `fn(&[Rc<Thunk>], &IndexMap<String, Rc<Thunk>>, usize) -> Result<Value, Box<EvalError>>`
//!
//! ## Builtin groups
//!
//! **Arithmetic:** `+`, `-`, `*`, `/`, `div` (with auto-promotion table)
//! **Comparison:** `=`, `<` (cross-type Int/Float comparison allowed)
//! **Control:** `if` (selective materialization -- only the chosen branch is forced)
//! **Dict primitives:** `keys`, `length`, `merge`, `append`
//! **Strings:** `str`, `split`, `replace`, `upper`, `lower`, `trim`
//! **Numeric:** `floor`, `round`
//! **Parsing:** `to-int`, `to-float`
//! **Evaluation control:** `eval`, `error`, `try`, `apply`
//! **Type introspection:** `type-of`
//! **I/O:** `from-json`

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::error::EvalError;
use crate::eval::{invoke_function, materialize, MAX_EVAL_DEPTH};
use crate::value::{BuiltinFn, Environment, Key, Thunk, Value};

// --- Helpers ---

/// Helper: materialize a single positional argument, enforcing exact arity of 1
/// and rejecting named arguments. Used by many single-arg builtins.
fn expect_one_arg(
    name: &str,
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), Span::origin()).into());
    }
    if !named.is_empty() {
        return Err(EvalError::new(
            format!("{name} does not accept named arguments"),
            Span::origin(),
        )
        .into());
    }
    materialize(&args[0], None, depth)
}

/// Helper: check that an f64 value is within the representable range of i64
/// before casting. Returns an error if the value would saturate.
fn checked_f64_to_i64(name: &str, f: f64) -> Result<i64, Box<EvalError>> {
    if f < (i64::MIN as f64) || f > (i64::MAX as f64) {
        return Err(
            EvalError::new(format!("{name}: {f} is out of Int range"), Span::origin()).into(),
        );
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
fn extract_num_pair(args: &[Rc<Thunk>], depth: usize) -> Result<NumPair, Box<EvalError>> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), Span::origin()).into());
    }
    let left = materialize(&args[0], None, depth)?;
    let right = materialize(&args[1], None, depth)?;
    match (&left, &right) {
        (Value::Int(a), Value::Int(b)) => Ok(NumPair::Ints(*a, *b)),
        (Value::Int(a), Value::Float(b)) => Ok(NumPair::Floats(*a as f64, *b)),
        (Value::Float(a), Value::Int(b)) => Ok(NumPair::Floats(*a, *b as f64)),
        (Value::Float(a), Value::Float(b)) => Ok(NumPair::Floats(*a, *b)),
        _ => Err(EvalError::type_mismatch(
            "Int or Float",
            &format!("{} and {}", left.type_name(), right.type_name()),
            Span::origin(),
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
fn require_dict(name: &str, value: Value) -> Result<IndexMap<Key, Rc<Thunk>>, Box<EvalError>> {
    match value {
        Value::Dict(map) => Ok(map),
        other => Err(EvalError::new(
            format!("{name}: expected Dict, got {}", other.type_name()),
            Span::origin(),
        )
        .into()),
    }
}

/// Helper: require that a materialized value is a String, returning the inner String.
fn require_string(name: &str, value: Value) -> Result<String, Box<EvalError>> {
    match value {
        Value::String(s) => Ok(s),
        other => Err(EvalError::new(
            format!(
                "{name}: type mismatch: expected String, got {}",
                other.type_name()
            ),
            Span::origin(),
        )
        .into()),
    }
}

/// Helper: reject named arguments for multi-arg builtins that don't accept them.
fn reject_named(name: &str, named: &IndexMap<String, Rc<Thunk>>) -> Result<(), Box<EvalError>> {
    if !named.is_empty() {
        return Err(EvalError::new(
            format!("{name} does not accept named arguments"),
            Span::origin(),
        )
        .into());
    }
    Ok(())
}

// --- Arithmetic builtins ---

/// `+`: Addition with auto-promotion. Int + Int -> Int, any Float operand -> Float.
pub fn builtin_add(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    match extract_num_pair(args, depth)? {
        NumPair::Ints(a, b) => Ok(Value::Int(a.wrapping_add(b))),
        NumPair::Floats(a, b) => Ok(Value::Float(a + b)),
    }
}

/// `-`: Subtraction with auto-promotion. Int - Int -> Int, any Float operand -> Float.
pub fn builtin_sub(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    match extract_num_pair(args, depth)? {
        NumPair::Ints(a, b) => Ok(Value::Int(a.wrapping_sub(b))),
        NumPair::Floats(a, b) => Ok(Value::Float(a - b)),
    }
}

/// `*`: Multiplication with auto-promotion. Int * Int -> Int, any Float operand -> Float.
pub fn builtin_mul(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    match extract_num_pair(args, depth)? {
        NumPair::Ints(a, b) => Ok(Value::Int(a.wrapping_mul(b))),
        NumPair::Floats(a, b) => Ok(Value::Float(a * b)),
    }
}

/// `/`: Float division. ALWAYS returns Float, even for Int / Int. Division by zero produces an error.
pub fn builtin_div_float(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    match extract_num_pair(args, depth)? {
        NumPair::Ints(a, b) => {
            if b == 0 {
                Err(EvalError::new("division by zero", Span::origin()).into())
            } else {
                Ok(Value::Float(a as f64 / b as f64))
            }
        }
        NumPair::Floats(a, b) => {
            if b == 0.0 {
                Err(EvalError::new("division by zero", Span::origin()).into())
            } else {
                Ok(Value::Float(a / b))
            }
        }
    }
}

/// `div`: Integer division.
/// Int div Int -> Int (truncates toward zero, matching Rust's `/`).
/// For Float inputs, performs f64 division then truncates toward zero to Int.
/// Division by zero produces an error.
pub fn builtin_div_int(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    match extract_num_pair(args, depth)? {
        NumPair::Ints(a, b) => {
            if b == 0 {
                Err(EvalError::new("division by zero", Span::origin()).into())
            } else {
                Ok(Value::Int(a / b))
            }
        }
        NumPair::Floats(a, b) => {
            if b == 0.0 {
                Err(EvalError::new("division by zero", Span::origin()).into())
            } else {
                let result = (a / b).trunc();
                if result.is_nan() || result.is_infinite() {
                    return Err(EvalError::new("div: result is not finite", Span::origin()).into());
                }
                Ok(Value::Int(checked_f64_to_i64("div", result)?))
            }
        }
    }
}

// --- Comparison builtins ---

/// `=`: Equality comparison.
/// Works on Int, Float, String, Bool. Cross-type Int/Float comparison
/// promotes Int to Float. Dict/Function/Builtin are never equal (returns false,
/// not an error).
pub fn builtin_eq(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), Span::origin()).into());
    }
    let left = materialize(&args[0], None, depth)?;
    let right = materialize(&args[1], None, depth)?;

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
    Ok(Value::Bool(result))
}

/// `<`: Less-than comparison.
/// Works on Int, Float, String. Cross-type Int/Float comparison promotes
/// Int to Float. String comparison is lexicographic. Incompatible types
/// (e.g. Int vs String, Bool vs anything) produce a type error.
pub fn builtin_lt(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), Span::origin()).into());
    }
    let left = materialize(&args[0], None, depth)?;
    let right = materialize(&args[1], None, depth)?;

    let result = match (&left, &right) {
        (Value::Int(a), Value::Int(b)) => a < b,
        (Value::Float(a), Value::Float(b)) => a < b,
        (Value::String(a), Value::String(b)) => a < b,
        // Cross-type: Int/Float promotion
        (Value::Int(a), Value::Float(b)) => (*a as f64) < *b,
        (Value::Float(a), Value::Int(b)) => *a < (*b as f64),
        _ => {
            return Err(EvalError::type_mismatch(
                "Int, Float, or String (same or compatible types)",
                &format!("{} and {}", left.type_name(), right.type_name()),
                Span::origin(),
            )
            .into());
        }
    };
    Ok(Value::Bool(result))
}

// --- Control flow ---

/// `if`: Conditional with selective materialization.
///
/// Takes 3 positional args: condition, then-branch, else-branch.
/// Materializes ONLY the condition, then materializes ONLY the chosen branch.
/// The unchosen branch's thunk is never materialized -- this preserves lazy
/// semantics because `eval_call` wraps each arg as a thunk before calling.
pub fn builtin_if(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), Span::origin()).into());
    }

    // Materialize only the condition
    let condition = materialize(&args[0], None, depth)?;

    match condition {
        Value::Bool(true) => materialize(&args[1], None, depth),
        Value::Bool(false) => materialize(&args[2], None, depth),
        _ => Err(EvalError::type_mismatch("Bool", condition.type_name(), Span::origin()).into()),
    }
}

// --- Dict primitives ---

/// `keys`: Takes 1 arg (a Dict). Returns a Dict with integer keys `0..n`
/// mapping to the key values (Int keys become Int values, String keys become
/// String values). Insertion order is preserved.
pub fn builtin_keys(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), Span::origin()).into());
    }
    let val = materialize(&args[0], None, depth)?;
    let map = require_dict("keys", val)?;

    let origin = Span::origin();
    let mut result = IndexMap::new();
    for (i, (key, _)) in map.iter().enumerate() {
        let key_value = match key {
            Key::Int(n) => Value::Int(*n),
            Key::String(s) => Value::String(s.clone()),
        };
        result.insert(
            Key::Int(i as i64),
            Rc::new(Thunk::new_materialized(key_value, origin)),
        );
    }
    Ok(Value::Dict(result))
}

/// `length`: Takes 1 arg (a Dict). Returns an Int with the number of entries.
pub fn builtin_length(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), Span::origin()).into());
    }
    let val = materialize(&args[0], None, depth)?;
    let map = require_dict("length", val)?;
    Ok(Value::Int(map.len() as i64))
}

/// `merge`: Takes 2 args (both Dicts). Returns a right-biased merge: all
/// entries from the left dict, then all entries from the right dict. If both
/// have the same key, right wins. Values remain as thunks (no materialization
/// of values).
pub fn builtin_merge(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), Span::origin()).into());
    }
    let left_val = materialize(&args[0], None, depth)?;
    let right_val = materialize(&args[1], None, depth)?;
    let left = require_dict("merge", left_val)?;
    let right = require_dict("merge", right_val)?;

    let mut result = IndexMap::with_capacity(left.len() + right.len());
    // Insert all left entries
    for (key, thunk) in &left {
        result.insert(key.clone(), Rc::clone(thunk));
    }
    // Insert all right entries (overwrites on collision)
    for (key, thunk) in &right {
        result.insert(key.clone(), Rc::clone(thunk));
    }
    Ok(Value::Dict(result))
}

/// `append`: Takes 2 args: a Dict and any value. Returns a new dict with the
/// value inserted at the next integer key (one past the current maximum integer
/// key, or 0 for empty dicts / dicts with no integer keys).
///
/// This is O(n) for the clone but O(1) amortized for the insert itself,
/// compared to the old LLT `append` which did a full `merge` (copying the
/// entire accumulator into a new dict via two-dict iteration).
pub fn builtin_append(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), Span::origin()).into());
    }
    let dict_val = materialize(&args[0], None, depth)?;
    let mut map = require_dict("append", dict_val)?;

    // Compute the next integer key: max existing int key + 1, or 0 if none.
    let next_key = map
        .keys()
        .filter_map(|k| match k {
            Key::Int(n) => Some(*n),
            _ => None,
        })
        .max()
        .map_or(0, |max| max + 1);

    map.insert(Key::Int(next_key), Rc::clone(&args[1]));
    Ok(Value::Dict(map))
}

// --- String builtins ---

/// `str`: Variadic string concatenation and toString.
///
/// Materializes each argument and concatenates their string representations.
/// With zero args, returns an empty string.
pub fn builtin_str(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    let mut result = String::new();
    for arg in args {
        let val = materialize(arg, None, depth)?;
        result.push_str(&stringify(&val));
    }
    Ok(Value::String(result))
}

/// `split`: Split a string by a separator.
///
/// Takes 2 args: `separator` (String), `input` (String).
/// Returns a Dict with integer keys `0..n` mapping to the split substrings.
pub fn builtin_split(
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    reject_named("split", named)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), Span::origin()).into());
    }
    let sep_val = materialize(&args[0], None, depth)?;
    let input_val = materialize(&args[1], None, depth)?;

    let sep = require_string("split", sep_val)?;
    let input = require_string("split", input_val)?;

    let parts: Vec<&str> = input.split(sep.as_str()).collect();
    let mut map = IndexMap::new();
    for (i, part) in parts.into_iter().enumerate() {
        map.insert(
            Key::Int(i as i64),
            Rc::new(Thunk::new_materialized(
                Value::String(part.to_string()),
                Span::origin(),
            )),
        );
    }
    Ok(Value::Dict(map))
}

/// `replace`: Replace all occurrences of a pattern in a string.
///
/// Takes 3 args: `pattern` (String), `replacement` (String), `input` (String).
/// Returns a new String with all occurrences of `pattern` replaced by `replacement`.
pub fn builtin_replace(
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    reject_named("replace", named)?;
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), Span::origin()).into());
    }
    let pattern_val = materialize(&args[0], None, depth)?;
    let replacement_val = materialize(&args[1], None, depth)?;
    let input_val = materialize(&args[2], None, depth)?;

    let pattern = require_string("replace", pattern_val)?;
    let replacement = require_string("replace", replacement_val)?;
    let input = require_string("replace", input_val)?;

    Ok(Value::String(input.replace(pattern.as_str(), &replacement)))
}

/// `upper`: Convert a string to uppercase. Takes 1 arg (String).
pub fn builtin_upper(
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    let val = expect_one_arg("upper", args, named, depth)?;
    let s = require_string("upper", val)?;
    Ok(Value::String(s.to_uppercase()))
}

/// `lower`: Convert a string to lowercase. Takes 1 arg (String).
pub fn builtin_lower(
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    let val = expect_one_arg("lower", args, named, depth)?;
    let s = require_string("lower", val)?;
    Ok(Value::String(s.to_lowercase()))
}

/// `trim`: Remove leading and trailing whitespace from a string.
///
/// Takes 1 arg (String). Returns the trimmed string.
pub fn builtin_trim(
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    let val = expect_one_arg("trim", args, named, depth)?;
    let s = require_string("trim", val)?;
    Ok(Value::String(s.trim().to_string()))
}

// --- Numeric builtins ---

/// `floor`: Takes 1 numeric arg (Int or Float). Returns Int.
///
/// - Int input: returned unchanged.
/// - Float input: applies `f64::floor()` then converts to `i64`.
/// - NaN or infinity: errors (cannot convert to Int).
/// - Non-numeric input: type error.
pub fn builtin_floor(
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    let val = expect_one_arg("floor", args, named, depth)?;
    match val {
        Value::Int(n) => Ok(Value::Int(n)),
        Value::Float(f) => {
            if f.is_nan() {
                return Err(EvalError::new(
                    "floor: NaN cannot be converted to Int",
                    Span::origin(),
                )
                .into());
            }
            if f.is_infinite() {
                return Err(EvalError::new(
                    "floor: infinity cannot be converted to Int",
                    Span::origin(),
                )
                .into());
            }
            Ok(Value::Int(checked_f64_to_i64("floor", f.floor())?))
        }
        other => {
            Err(EvalError::type_mismatch("Int or Float", other.type_name(), Span::origin()).into())
        }
    }
}

/// `round`: Takes 1 numeric arg (Int or Float). Returns Int.
///
/// - Int input: returned unchanged.
/// - Float input: applies `f64::round()` (half-away-from-zero) then converts to `i64`.
/// - NaN or infinity: errors (cannot convert to Int).
/// - Non-numeric input: type error.
pub fn builtin_round(
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    let val = expect_one_arg("round", args, named, depth)?;
    match val {
        Value::Int(n) => Ok(Value::Int(n)),
        Value::Float(f) => {
            if f.is_nan() {
                return Err(EvalError::new(
                    "round: NaN cannot be converted to Int",
                    Span::origin(),
                )
                .into());
            }
            if f.is_infinite() {
                return Err(EvalError::new(
                    "round: infinity cannot be converted to Int",
                    Span::origin(),
                )
                .into());
            }
            Ok(Value::Int(checked_f64_to_i64("round", f.round())?))
        }
        other => {
            Err(EvalError::type_mismatch("Int or Float", other.type_name(), Span::origin()).into())
        }
    }
}

// --- Parsing builtins ---

/// `to-int`: STRING-TO-NUMBER PARSING ONLY. Takes 1 String arg.
///
/// Parses the string as an integer via `str::parse::<i64>()`. Returns Int.
/// Does NOT accept numeric inputs -- it is a string parser, not a type converter.
pub fn builtin_to_int(
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    let val = expect_one_arg("to-int", args, named, depth)?;
    let s = require_string("to-int", val)?;
    match s.parse::<i64>() {
        Ok(n) => Ok(Value::Int(n)),
        Err(_) => Err(EvalError::new(
            format!("to-int: cannot parse {:?} as Int", s),
            Span::origin(),
        )
        .into()),
    }
}

/// `to-float`: STRING-TO-NUMBER PARSING ONLY. Takes 1 String arg.
///
/// Parses the string as a float via `str::parse::<f64>()`. Returns Float.
/// Does NOT accept numeric inputs -- it is a string parser, not a type converter.
pub fn builtin_to_float(
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    let val = expect_one_arg("to-float", args, named, depth)?;
    let s = require_string("to-float", val)?;
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() => Ok(Value::Float(f)),
        Ok(_) => Err(EvalError::new(
            format!(
                "to-float: cannot parse {:?} as Float (non-finite values not allowed)",
                s
            ),
            Span::origin(),
        )
        .into()),
        Err(_) => Err(EvalError::new(
            format!("to-float: cannot parse {:?} as Float", s),
            Span::origin(),
        )
        .into()),
    }
}

// --- Evaluation control builtins ---

/// Recursively materialize a value: if it is a Dict, materialize every entry
/// value and recurse into nested dicts.
fn deep_eval(val: &Value, depth: usize) -> Result<Value, Box<EvalError>> {
    if depth >= MAX_EVAL_DEPTH {
        return Err(EvalError::new(
            format!("maximum evaluation depth exceeded ({MAX_EVAL_DEPTH})"),
            Span::origin(),
        )
        .into());
    }
    match val {
        Value::Dict(map) => {
            let mut result = IndexMap::new();
            for (key, thunk) in map {
                let v = materialize(thunk, None, depth)?;
                let forced = deep_eval(&v, depth + 1)?;
                result.insert(
                    key.clone(),
                    Rc::new(Thunk::new_materialized(forced, thunk.span)),
                );
            }
            Ok(Value::Dict(result))
        }
        // Non-dict values are already fully materialized
        other => Ok(other.clone()),
    }
}

/// `eval`: takes 1 arg, deep-forces all thunks recursively.
pub fn builtin_eval(
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    let val = expect_one_arg("eval", args, named, depth)?;
    deep_eval(&val, depth)
}

/// `error`: takes 1 arg (String message), always raises.
pub fn builtin_error(
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    let val = expect_one_arg("error", args, named, depth)?;
    let msg = require_string("error", val)?;
    Err(EvalError::new(msg, args[0].span).into())
}

/// `try`: takes 1 arg (a zero-arg Function). Calls it. Returns `[ok: value]`
/// on success or `[err: message]` on failure.
pub fn builtin_try(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), Span::origin()).into());
    }
    let func_val = materialize(&args[0], None, depth)?;

    let call_result = match func_val {
        Value::Function {
            params,
            body,
            env: closure_env,
            ..
        } => {
            if !params.is_empty() {
                return Err(EvalError::new(
                    format!(
                        "try requires a zero-argument function, got {} parameters",
                        params.len()
                    ),
                    args[0].span,
                )
                .into());
            }
            // Evaluate the body in the closure's environment
            let body_thunk = Rc::new(Thunk::new_unevaluated(
                (*body).clone(),
                Rc::clone(&closure_env),
                body.span,
            ));
            materialize(&body_thunk, None, depth)
        }
        Value::Builtin { func, .. } => func(&[], &IndexMap::new(), depth),
        _ => {
            return Err(
                EvalError::type_mismatch("Function", func_val.type_name(), args[0].span).into(),
            )
        }
    };

    match call_result {
        Ok(value) => {
            let mut result = IndexMap::new();
            result.insert(
                Key::String("ok".to_string()),
                Rc::new(Thunk::new_materialized(value, Span::origin())),
            );
            Ok(Value::Dict(result))
        }
        Err(e) => {
            let mut result = IndexMap::new();
            result.insert(
                Key::String("err".to_string()),
                Rc::new(Thunk::new_materialized(
                    Value::String(e.message.clone()),
                    Span::origin(),
                )),
            );
            Ok(Value::Dict(result))
        }
    }
}

/// `apply`: takes 2 args (function, dict/list). Spreads the dict's values as
/// positional arguments to the function call.
///
/// For user-defined functions, delegates to `eval::invoke_function` so that
/// default values, named args, and variadics are handled identically to `call`.
pub fn builtin_apply(
    args: &[Rc<Thunk>],
    _named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), Span::origin()).into());
    }
    let func_val = materialize(&args[0], None, depth)?;
    let args_val = materialize(&args[1], None, depth)?;

    let arg_dict = match args_val {
        Value::Dict(map) => map,
        _ => {
            return Err(EvalError::type_mismatch("Dict", args_val.type_name(), args[1].span).into())
        }
    };

    // Collect the dict values in insertion order as positional args
    let positional: Vec<Rc<Thunk>> = arg_dict.values().cloned().collect();

    match func_val {
        Value::Function {
            params,
            body,
            env: closure_env,
            ..
        } => {
            // Delegate to the shared invoke path. Defaults are evaluated in
            // the closure env since apply has no caller-side AST context.
            let result_thunk = invoke_function(
                &params,
                &body,
                &closure_env,
                &positional,
                &IndexMap::new(),
                &closure_env,
                args[0].span,
                depth,
            )?;
            materialize(&result_thunk, None, depth)
        }
        Value::Builtin { func, .. } => func(&positional, &IndexMap::new(), depth),
        _ => Err(EvalError::type_mismatch("Function", func_val.type_name(), args[0].span).into()),
    }
}

// --- Type introspection ---

/// `type-of`: takes 1 arg, materializes it, returns the type name.
/// Both `Function` and `Builtin` return "Function" (from the user's perspective).
pub fn builtin_type_of(
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    let val = expect_one_arg("type-of", args, named, depth)?;
    let type_name = match &val {
        Value::Int(_) => "Int",
        Value::Float(_) => "Float",
        Value::String(_) => "String",
        Value::Bool(_) => "Bool",
        Value::Dict(_) => "Dict",
        Value::Function { .. } | Value::Builtin { .. } => "Function",
    };
    Ok(Value::String(type_name.to_string()))
}

// --- I/O builtins ---

/// Convert a `serde_json::Value` into an LLT `Value`.
fn json_to_value(json: &serde_json::Value, depth: usize) -> Result<Value, Box<EvalError>> {
    if depth >= MAX_EVAL_DEPTH {
        return Err(EvalError::new(
            format!("maximum JSON nesting depth exceeded ({MAX_EVAL_DEPTH})"),
            Span::origin(),
        )
        .into());
    }
    match json {
        serde_json::Value::Null => Ok(Value::Dict(IndexMap::new())),
        serde_json::Value::Bool(b) => Ok(Value::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Int(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Float(f))
            } else {
                // Fallback for u64 values outside i64 range; as_f64() always
                // succeeds for JSON numbers, so unwrap() is safe here.
                Ok(Value::Float(n.as_f64().unwrap()))
            }
        }
        serde_json::Value::String(s) => Ok(Value::String(s.clone())),
        serde_json::Value::Array(arr) => {
            let mut map = IndexMap::new();
            for (i, item) in arr.iter().enumerate() {
                let val = json_to_value(item, depth + 1)?;
                map.insert(
                    Key::Int(i as i64),
                    Rc::new(Thunk::new_materialized(val, Span::origin())),
                );
            }
            Ok(Value::Dict(map))
        }
        serde_json::Value::Object(obj) => {
            let mut map = IndexMap::new();
            for (k, v) in obj {
                let val = json_to_value(v, depth + 1)?;
                map.insert(
                    Key::String(k.clone()),
                    Rc::new(Thunk::new_materialized(val, Span::origin())),
                );
            }
            Ok(Value::Dict(map))
        }
    }
}

/// `from-json`: takes 1 arg (String containing JSON), parses into LLT value.
pub fn builtin_from_json(
    args: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    let val = expect_one_arg("from-json", args, named, depth)?;
    let json_str = require_string("from-json", val)?;
    let parsed: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| EvalError::new(format!("invalid JSON: {e}"), args[0].span))?;
    json_to_value(&parsed, 0)
}

// --- Registration ---

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
        ("div", builtin_div_int),
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
    env
}

/// Create the stdlib environment: root builtins + prelude functions.
///
/// Parses and evaluates `stdlib/prelude.llt` using the root env, then
/// layers the prelude dict entries as a child scope. User code should
/// use this as the parent environment.
pub fn create_stdlib_env() -> Result<Rc<RefCell<Environment>>, String> {
    let root_env = create_root_env();

    let prelude_source = include_str!("../stdlib/prelude.llt");
    let file =
        crate::parser::parse(prelude_source).map_err(|e| format!("prelude parse error: {e}"))?;

    let thunk = crate::eval::eval_file(&file.node, Rc::clone(&root_env), 0)
        .map_err(|e| format!("prelude eval error: {e}"))?;

    let val = crate::eval::materialize(&thunk, None, 0)
        .map_err(|e| format!("prelude materialize error: {e}"))?;

    let dict = match val {
        Value::Dict(map) => map,
        other => {
            return Err(format!(
                "prelude must evaluate to a Dict, got {}",
                other.type_name()
            ))
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

    /// Helper: wrap a Value in a materialized Thunk inside an Rc.
    fn thunk(val: Value) -> Rc<Thunk> {
        Rc::new(Thunk::new_materialized(val, test_span(1, 1, 1, 5)))
    }

    fn no_named() -> IndexMap<String, Rc<Thunk>> {
        IndexMap::new()
    }

    /// Helper: make a zero-arg function whose body is a single expression.
    fn zero_arg_fn(body_expr: Expr) -> Value {
        Value::Function {
            params: Rc::new(vec![]),
            body: Rc::new(Spanned::new(body_expr, test_span(1, 1, 1, 10))),
            env: Rc::new(RefCell::new(Environment::new())),
            return_ann: None,
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
            return_ann: None,
        }
    }

    fn thunk_dict(map: IndexMap<Key, Rc<Thunk>>) -> Rc<Thunk> {
        Rc::new(Thunk::new_materialized(
            Value::Dict(map),
            test_span(1, 1, 1, 5),
        ))
    }

    // --- floor ---

    #[test]
    fn floor_int_passthrough() {
        let result = builtin_floor(&[thunk(Value::Int(42))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn floor_negative_int_passthrough() {
        let result = builtin_floor(&[thunk(Value::Int(-7))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(-7));
    }

    #[test]
    fn floor_zero_int() {
        let result = builtin_floor(&[thunk(Value::Int(0))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn floor_positive_float() {
        let result = builtin_floor(&[thunk(Value::Float(3.7))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn floor_negative_float() {
        // floor(-3.2) = -4, not -3
        let result = builtin_floor(&[thunk(Value::Float(-3.2))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(-4));
    }

    #[test]
    fn floor_float_exact_integer() {
        let result = builtin_floor(&[thunk(Value::Float(5.0))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn floor_float_just_below_integer() {
        let result = builtin_floor(&[thunk(Value::Float(2.9999999))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn floor_nan_errors() {
        let err = builtin_floor(&[thunk(Value::Float(f64::NAN))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("NaN"), "got: {}", err.message);
    }

    #[test]
    fn floor_positive_infinity_errors() {
        let err = builtin_floor(&[thunk(Value::Float(f64::INFINITY))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("infinity"), "got: {}", err.message);
    }

    #[test]
    fn floor_negative_infinity_errors() {
        let err =
            builtin_floor(&[thunk(Value::Float(f64::NEG_INFINITY))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("infinity"), "got: {}", err.message);
    }

    #[test]
    fn floor_string_type_error() {
        let err = builtin_floor(&[thunk(Value::String("3.5".into()))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("String"), "got: {}", err.message);
    }

    #[test]
    fn floor_bool_type_error() {
        let err = builtin_floor(&[thunk(Value::Bool(true))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn floor_dict_type_error() {
        let err =
            builtin_floor(&[thunk(Value::Dict(IndexMap::new()))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn floor_wrong_arity_zero() {
        let err = builtin_floor(&[], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn floor_wrong_arity_two() {
        let err = builtin_floor(
            &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn floor_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_floor(&[thunk(Value::Float(3.5))], &named, 0).unwrap_err();
        assert!(
            err.message.contains("named arguments"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn floor_large_positive_float_out_of_range() {
        let err = builtin_floor(&[thunk(Value::Float(1e19))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("out of Int range"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn floor_large_negative_float_out_of_range() {
        let err = builtin_floor(&[thunk(Value::Float(-1e19))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("out of Int range"),
            "got: {}",
            err.message
        );
    }

    // --- round ---

    #[test]
    fn round_int_passthrough() {
        let result = builtin_round(&[thunk(Value::Int(42))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn round_negative_int_passthrough() {
        let result = builtin_round(&[thunk(Value::Int(-7))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(-7));
    }

    #[test]
    fn round_positive_half_rounds_up() {
        // 0.5 rounds to 1 (half-away-from-zero)
        let result = builtin_round(&[thunk(Value::Float(0.5))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(1));
    }

    #[test]
    fn round_negative_half_rounds_down() {
        // -0.5 rounds to -1 (half-away-from-zero)
        let result = builtin_round(&[thunk(Value::Float(-0.5))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(-1));
    }

    #[test]
    fn round_positive_below_half() {
        let result = builtin_round(&[thunk(Value::Float(2.4))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn round_positive_above_half() {
        let result = builtin_round(&[thunk(Value::Float(2.6))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn round_negative_below_half() {
        // -2.4 rounds to -2
        let result = builtin_round(&[thunk(Value::Float(-2.4))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(-2));
    }

    #[test]
    fn round_negative_above_half() {
        // -2.6 rounds to -3
        let result = builtin_round(&[thunk(Value::Float(-2.6))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(-3));
    }

    #[test]
    fn round_1_5_rounds_to_2() {
        let result = builtin_round(&[thunk(Value::Float(1.5))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn round_negative_1_5_rounds_to_negative_2() {
        let result = builtin_round(&[thunk(Value::Float(-1.5))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(-2));
    }

    #[test]
    fn round_float_exact_integer() {
        let result = builtin_round(&[thunk(Value::Float(5.0))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn round_nan_errors() {
        let err = builtin_round(&[thunk(Value::Float(f64::NAN))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("NaN"), "got: {}", err.message);
    }

    #[test]
    fn round_positive_infinity_errors() {
        let err = builtin_round(&[thunk(Value::Float(f64::INFINITY))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("infinity"), "got: {}", err.message);
    }

    #[test]
    fn round_negative_infinity_errors() {
        let err =
            builtin_round(&[thunk(Value::Float(f64::NEG_INFINITY))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("infinity"), "got: {}", err.message);
    }

    #[test]
    fn round_string_type_error() {
        let err = builtin_round(&[thunk(Value::String("3.5".into()))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn round_bool_type_error() {
        let err = builtin_round(&[thunk(Value::Bool(false))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn round_wrong_arity_zero() {
        let err = builtin_round(&[], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn round_wrong_arity_two() {
        let err = builtin_round(
            &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn round_large_positive_float_out_of_range() {
        let err = builtin_round(&[thunk(Value::Float(1e19))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("out of Int range"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn round_large_negative_float_out_of_range() {
        let err = builtin_round(&[thunk(Value::Float(-1e19))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("out of Int range"),
            "got: {}",
            err.message
        );
    }

    // --- to-int ---

    #[test]
    fn to_int_valid_positive() {
        let result = builtin_to_int(&[thunk(Value::String("42".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn to_int_valid_negative() {
        let result = builtin_to_int(&[thunk(Value::String("-7".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(-7));
    }

    #[test]
    fn to_int_valid_zero() {
        let result = builtin_to_int(&[thunk(Value::String("0".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn to_int_valid_large() {
        let result = builtin_to_int(
            &[thunk(Value::String("9223372036854775807".into()))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(result, Value::Int(i64::MAX));
    }

    #[test]
    fn to_int_invalid_float_string() {
        let err =
            builtin_to_int(&[thunk(Value::String("3.14".into()))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("cannot parse"), "got: {}", err.message);
    }

    #[test]
    fn to_int_invalid_text() {
        let err =
            builtin_to_int(&[thunk(Value::String("hello".into()))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("cannot parse"), "got: {}", err.message);
    }

    #[test]
    fn to_int_invalid_empty() {
        let err = builtin_to_int(&[thunk(Value::String("".into()))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("cannot parse"), "got: {}", err.message);
    }

    #[test]
    fn to_int_invalid_with_spaces() {
        let err =
            builtin_to_int(&[thunk(Value::String(" 42 ".into()))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("cannot parse"), "got: {}", err.message);
    }

    #[test]
    fn to_int_rejects_int_input() {
        let err = builtin_to_int(&[thunk(Value::Int(42))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("Int"),
            "should mention Int, got: {}",
            err.message
        );
    }

    #[test]
    fn to_int_rejects_float_input() {
        let err = builtin_to_int(&[thunk(Value::Float(3.14))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn to_int_rejects_bool_input() {
        let err = builtin_to_int(&[thunk(Value::Bool(true))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn to_int_rejects_dict_input() {
        let err =
            builtin_to_int(&[thunk(Value::Dict(IndexMap::new()))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn to_int_wrong_arity_zero() {
        let err = builtin_to_int(&[], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn to_int_wrong_arity_two() {
        let err = builtin_to_int(
            &[
                thunk(Value::String("1".into())),
                thunk(Value::String("2".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    // --- to-float ---

    #[test]
    fn to_float_valid_decimal() {
        let result =
            builtin_to_float(&[thunk(Value::String("3.14".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn to_float_valid_integer_string() {
        // "42" parses as 42.0
        let result =
            builtin_to_float(&[thunk(Value::String("42".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Float(42.0));
    }

    #[test]
    fn to_float_valid_negative() {
        let result =
            builtin_to_float(&[thunk(Value::String("-2.5".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Float(-2.5));
    }

    #[test]
    fn to_float_valid_scientific_notation() {
        let result =
            builtin_to_float(&[thunk(Value::String("1.5e10".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Float(1.5e10));
    }

    #[test]
    fn to_float_valid_negative_exponent() {
        let result =
            builtin_to_float(&[thunk(Value::String("2.5e-3".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Float(2.5e-3));
    }

    #[test]
    fn to_float_valid_zero() {
        let result =
            builtin_to_float(&[thunk(Value::String("0.0".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Float(0.0));
    }

    #[test]
    fn to_float_valid_leading_dot() {
        // ".5" parses to 0.5
        let result =
            builtin_to_float(&[thunk(Value::String(".5".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Float(0.5));
    }

    #[test]
    fn to_float_invalid_text() {
        let err =
            builtin_to_float(&[thunk(Value::String("hello".into()))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("cannot parse"), "got: {}", err.message);
    }

    #[test]
    fn to_float_invalid_empty() {
        let err = builtin_to_float(&[thunk(Value::String("".into()))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("cannot parse"), "got: {}", err.message);
    }

    #[test]
    fn to_float_rejects_inf() {
        let err =
            builtin_to_float(&[thunk(Value::String("inf".into()))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("non-finite"), "got: {}", err.message);
    }

    #[test]
    fn to_float_rejects_negative_inf() {
        let err =
            builtin_to_float(&[thunk(Value::String("-inf".into()))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("non-finite"), "got: {}", err.message);
    }

    #[test]
    fn to_float_rejects_infinity() {
        let err = builtin_to_float(&[thunk(Value::String("infinity".into()))], &no_named(), 0)
            .unwrap_err();
        assert!(err.message.contains("non-finite"), "got: {}", err.message);
    }

    #[test]
    fn to_float_rejects_nan() {
        let err =
            builtin_to_float(&[thunk(Value::String("NaN".into()))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("non-finite"), "got: {}", err.message);
    }

    #[test]
    fn to_float_rejects_int_input() {
        let err = builtin_to_float(&[thunk(Value::Int(42))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn to_float_rejects_float_input() {
        let err = builtin_to_float(&[thunk(Value::Float(3.14))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn to_float_rejects_bool_input() {
        let err = builtin_to_float(&[thunk(Value::Bool(true))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn to_float_wrong_arity_zero() {
        let err = builtin_to_float(&[], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn to_float_wrong_arity_two() {
        let err = builtin_to_float(
            &[
                thunk(Value::String("1.0".into())),
                thunk(Value::String("2.0".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn to_float_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::String("1.0".into())));
        let err = builtin_to_float(&[thunk(Value::String("3.14".into()))], &named, 0).unwrap_err();
        assert!(
            err.message.contains("named arguments"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn to_int_overflow() {
        // One past i64::MAX
        let err = builtin_to_int(
            &[thunk(Value::String("9223372036854775808".into()))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(err.message.contains("cannot parse"), "got: {}", err.message);
    }

    // --- eval ---

    #[test]
    fn eval_primitive_int() {
        let result = builtin_eval(&[thunk(Value::Int(42))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn eval_primitive_string() {
        let result = builtin_eval(&[thunk(Value::String("hello".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn eval_primitive_float() {
        let result = builtin_eval(&[thunk(Value::Float(3.14))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn eval_primitive_bool() {
        let result = builtin_eval(&[thunk(Value::Bool(true))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn eval_empty_dict() {
        let dict = Value::Dict(IndexMap::new());
        let result = builtin_eval(&[thunk(dict)], &no_named(), 0).unwrap();
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
        let result = builtin_eval(&[thunk(dict)], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let a = materialize(&map[&Key::String("a".into())], None, 0).unwrap();
                assert_eq!(a, Value::Int(1));
                let b = materialize(&map[&Key::String("b".into())], None, 0).unwrap();
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

        let result = builtin_eval(&[thunk(outer_dict)], &no_named(), 0).unwrap();
        match result {
            Value::Dict(outer_map) => {
                let x_val = materialize(&outer_map[&Key::String("x".into())], None, 0).unwrap();
                match x_val {
                    Value::Dict(inner_map) => {
                        let y_val =
                            materialize(&inner_map[&Key::String("y".into())], None, 0).unwrap();
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
        let expr = Spanned::new(Expr::Int(99), test_span(1, 1, 1, 5));
        let env = Rc::new(RefCell::new(Environment::new()));
        let unevaluated = Rc::new(Thunk::new_unevaluated(expr, env, test_span(1, 1, 1, 5)));

        let mut map = IndexMap::new();
        map.insert(Key::String("val".into()), unevaluated);
        let dict = Value::Dict(map);

        let result = builtin_eval(&[thunk(dict)], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                let v = materialize(&map[&Key::String("val".into())], None, 0).unwrap();
                assert_eq!(v, Value::Int(99));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn eval_arity_error() {
        let err = builtin_eval(&[], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    // --- error ---

    #[test]
    fn error_raises_with_message() {
        let err =
            builtin_error(&[thunk(Value::String("boom".into()))], &no_named(), 0).unwrap_err();
        assert_eq!(err.message, "boom");
    }

    #[test]
    fn error_custom_message() {
        let err = builtin_error(
            &[thunk(Value::String("division by zero".into()))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert_eq!(err.message, "division by zero");
    }

    #[test]
    fn error_type_mismatch_on_non_string() {
        let err = builtin_error(&[thunk(Value::Int(42))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("String"), "got: {}", err.message);
    }

    #[test]
    fn error_arity_check() {
        let err = builtin_error(&[], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    // --- try ---

    #[test]
    fn try_success_returns_ok_dict() {
        // [fn [] 42]
        let func = zero_arg_fn(Expr::Int(42));
        let result = builtin_try(&[thunk(func)], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                assert!(map.contains_key(&Key::String("ok".into())));
                let ok_val = materialize(&map[&Key::String("ok".into())], None, 0).unwrap();
                assert_eq!(ok_val, Value::Int(42));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn try_success_with_string_body() {
        let func = zero_arg_fn(Expr::Str("hello".into()));
        let result = builtin_try(&[thunk(func)], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                let ok_val = materialize(&map[&Key::String("ok".into())], None, 0).unwrap();
                assert_eq!(ok_val, Value::String("hello".into()));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn try_failure_returns_err_dict() {
        // [fn [] $nonexistent] -- references an undefined variable
        let func = zero_arg_fn(Expr::VarRef("nonexistent".into()));
        let result = builtin_try(&[thunk(func)], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                assert!(map.contains_key(&Key::String("err".into())));
                let err_val = materialize(&map[&Key::String("err".into())], None, 0).unwrap();
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
        let err = builtin_try(&[thunk(Value::Int(42))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn try_non_zero_arg_function_error() {
        let func = n_arg_fn(&["x"], Expr::VarRef("x".into()));
        let err = builtin_try(&[thunk(func)], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("zero-argument"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn try_arity_check() {
        let err = builtin_try(&[], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn try_with_builtin_success() {
        fn ok_builtin(
            _: &[Rc<Thunk>],
            _: &IndexMap<String, Rc<Thunk>>,
            _: usize,
        ) -> Result<Value, Box<EvalError>> {
            Ok(Value::Int(99))
        }
        let b = Value::Builtin {
            name: "ok",
            func: ok_builtin,
        };
        let result = builtin_try(&[thunk(b)], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                let ok_val = materialize(&map[&Key::String("ok".into())], None, 0).unwrap();
                assert_eq!(ok_val, Value::Int(99));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn try_with_builtin_failure() {
        fn err_builtin(
            _: &[Rc<Thunk>],
            _: &IndexMap<String, Rc<Thunk>>,
            _: usize,
        ) -> Result<Value, Box<EvalError>> {
            Err(EvalError::new("builtin error", Span::origin()).into())
        }
        let b = Value::Builtin {
            name: "fail",
            func: err_builtin,
        };
        let result = builtin_try(&[thunk(b)], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                let err_val = materialize(&map[&Key::String("err".into())], None, 0).unwrap();
                assert_eq!(err_val, Value::String("builtin error".into()));
            }
            _ => panic!("expected Dict"),
        }
    }

    // --- apply ---

    #[test]
    fn apply_single_arg() {
        // [fn [x] $x] applied to [42]
        let func = n_arg_fn(&["x"], Expr::VarRef("x".into()));
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(42)));
        let args_val = Value::Dict(arg_dict);

        let result = builtin_apply(&[thunk(func), thunk(args_val)], &no_named(), 0).unwrap();
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

        let result = builtin_apply(&[thunk(func), thunk(args_val)], &no_named(), 0).unwrap();
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

        let result = builtin_apply(&[thunk(func), thunk(args_val)], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(20));
    }

    #[test]
    fn apply_with_builtin() {
        fn add_builtin(
            args: &[Rc<Thunk>],
            _named: &IndexMap<String, Rc<Thunk>>,
            _depth: usize,
        ) -> Result<Value, Box<EvalError>> {
            let a = materialize(&args[0], None, 0)?;
            let b = materialize(&args[1], None, 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x + y)),
                _ => Err(EvalError::new("expected ints", Span::origin()).into()),
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

        let result = builtin_apply(&[thunk(func), thunk(args_val)], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(7));
    }

    #[test]
    fn apply_arity_mismatch() {
        let func = n_arg_fn(&["x", "y"], Expr::VarRef("x".into()));
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(1)));
        let args_val = Value::Dict(arg_dict);

        let err = builtin_apply(&[thunk(func), thunk(args_val)], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn apply_non_function_type_error() {
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(1)));
        let args_val = Value::Dict(arg_dict);

        let err =
            builtin_apply(&[thunk(Value::Int(42)), thunk(args_val)], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn apply_non_dict_args_type_error() {
        let func = n_arg_fn(&["x"], Expr::VarRef("x".into()));
        let err = builtin_apply(&[thunk(func), thunk(Value::Int(42))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn apply_wrong_arity() {
        let err = builtin_apply(&[thunk(Value::Int(1))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    // --- type-of ---

    #[test]
    fn type_of_int() {
        let result = builtin_type_of(&[thunk(Value::Int(42))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("Int".into()));
    }

    #[test]
    fn type_of_float() {
        let result = builtin_type_of(&[thunk(Value::Float(3.14))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("Float".into()));
    }

    #[test]
    fn type_of_string() {
        let result = builtin_type_of(&[thunk(Value::String("hi".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("String".into()));
    }

    #[test]
    fn type_of_bool() {
        let result = builtin_type_of(&[thunk(Value::Bool(false))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("Bool".into()));
    }

    #[test]
    fn type_of_dict() {
        let result =
            builtin_type_of(&[thunk(Value::Dict(IndexMap::new()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("Dict".into()));
    }

    #[test]
    fn type_of_function() {
        let func = zero_arg_fn(Expr::Int(0));
        let result = builtin_type_of(&[thunk(func)], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("Function".into()));
    }

    #[test]
    fn type_of_builtin_returns_function() {
        fn dummy(
            _: &[Rc<Thunk>],
            _: &IndexMap<String, Rc<Thunk>>,
            _: usize,
        ) -> Result<Value, Box<EvalError>> {
            Ok(Value::Int(0))
        }
        let builtin = Value::Builtin {
            name: "dummy",
            func: dummy,
        };
        let result = builtin_type_of(&[thunk(builtin)], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("Function".into()));
    }

    #[test]
    fn type_of_arity_check() {
        let err = builtin_type_of(&[], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    // --- from-json ---

    #[test]
    fn from_json_int() {
        let result =
            builtin_from_json(&[thunk(Value::String("42".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn from_json_float() {
        let result =
            builtin_from_json(&[thunk(Value::String("3.14".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn from_json_string() {
        let result =
            builtin_from_json(&[thunk(Value::String(r#""hello""#.into()))], &no_named(), 0)
                .unwrap();
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn from_json_bool_true() {
        let result =
            builtin_from_json(&[thunk(Value::String("true".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn from_json_bool_false() {
        let result =
            builtin_from_json(&[thunk(Value::String("false".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Bool(false));
    }

    #[test]
    fn from_json_null_becomes_empty_dict() {
        let result =
            builtin_from_json(&[thunk(Value::String("null".into()))], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            _ => panic!("expected empty Dict for null"),
        }
    }

    #[test]
    fn from_json_array() {
        let result =
            builtin_from_json(&[thunk(Value::String("[1, 2, 3]".into()))], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v0 = materialize(&map[&Key::Int(0)], None, 0).unwrap();
                assert_eq!(v0, Value::Int(1));
                let v1 = materialize(&map[&Key::Int(1)], None, 0).unwrap();
                assert_eq!(v1, Value::Int(2));
                let v2 = materialize(&map[&Key::Int(2)], None, 0).unwrap();
                assert_eq!(v2, Value::Int(3));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn from_json_object() {
        let result = builtin_from_json(
            &[thunk(Value::String(
                r#"{"name": "Alice", "age": 30}"#.into(),
            ))],
            &no_named(),
            0,
        )
        .unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let name = materialize(&map[&Key::String("name".into())], None, 0).unwrap();
                assert_eq!(name, Value::String("Alice".into()));
                let age = materialize(&map[&Key::String("age".into())], None, 0).unwrap();
                assert_eq!(age, Value::Int(30));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn from_json_nested_structure() {
        let json = r#"{"users": [{"name": "Bob"}, {"name": "Eve"}]}"#;
        let result =
            builtin_from_json(&[thunk(Value::String(json.into()))], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                let users = materialize(&map[&Key::String("users".into())], None, 0).unwrap();
                match users {
                    Value::Dict(arr) => {
                        assert_eq!(arr.len(), 2);
                        let user0 = materialize(&arr[&Key::Int(0)], None, 0).unwrap();
                        match user0 {
                            Value::Dict(u) => {
                                let name =
                                    materialize(&u[&Key::String("name".into())], None, 0).unwrap();
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
        let err = builtin_from_json(&[thunk(Value::String("{bad json".into()))], &no_named(), 0)
            .unwrap_err();
        assert!(err.message.contains("invalid JSON"), "got: {}", err.message);
    }

    #[test]
    fn from_json_non_string_type_error() {
        let err = builtin_from_json(&[thunk(Value::Int(42))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn from_json_arity_check() {
        let err = builtin_from_json(&[], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn from_json_empty_object() {
        let result =
            builtin_from_json(&[thunk(Value::String("{}".into()))], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            _ => panic!("expected empty Dict"),
        }
    }

    #[test]
    fn from_json_empty_array() {
        let result =
            builtin_from_json(&[thunk(Value::String("[]".into()))], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            _ => panic!("expected empty Dict"),
        }
    }

    #[test]
    fn from_json_mixed_array() {
        let result = builtin_from_json(
            &[thunk(Value::String(r#"[1, "two", true, null]"#.into()))],
            &no_named(),
            0,
        )
        .unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 4);
                let v0 = materialize(&map[&Key::Int(0)], None, 0).unwrap();
                assert_eq!(v0, Value::Int(1));
                let v1 = materialize(&map[&Key::Int(1)], None, 0).unwrap();
                assert_eq!(v1, Value::String("two".into()));
                let v2 = materialize(&map[&Key::Int(2)], None, 0).unwrap();
                assert_eq!(v2, Value::Bool(true));
                let v3 = materialize(&map[&Key::Int(3)], None, 0).unwrap();
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
        let err = json_to_value(&deep, 0).unwrap_err();
        assert!(
            err.message.contains("maximum JSON nesting depth exceeded"),
            "expected depth error, got: {}",
            err.message
        );
    }

    // --- keys ---

    #[test]
    fn keys_empty_dict() {
        let dict = thunk_dict(IndexMap::new());
        let result = builtin_keys(&[dict], &no_named(), 0).unwrap();
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

        let result = builtin_keys(&[dict], &no_named(), 0).unwrap();
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 3);
                for i in 0..3 {
                    let val = materialize(&keys_map[&Key::Int(i)], None, 0).unwrap();
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

        let result = builtin_keys(&[dict], &no_named(), 0).unwrap();
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 2);
                let k0 = materialize(&keys_map[&Key::Int(0)], None, 0).unwrap();
                assert_eq!(k0, Value::String("name".into()));
                let k1 = materialize(&keys_map[&Key::Int(1)], None, 0).unwrap();
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

        let result = builtin_keys(&[dict], &no_named(), 0).unwrap();
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 3);
                let k0 = materialize(&keys_map[&Key::Int(0)], None, 0).unwrap();
                assert_eq!(k0, Value::Int(0));
                let k1 = materialize(&keys_map[&Key::Int(1)], None, 0).unwrap();
                assert_eq!(k1, Value::String("label".into()));
                let k2 = materialize(&keys_map[&Key::Int(2)], None, 0).unwrap();
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

        let result = builtin_keys(&[dict], &no_named(), 0).unwrap();
        match result {
            Value::Dict(keys_map) => {
                let k0 = materialize(&keys_map[&Key::Int(0)], None, 0).unwrap();
                let k1 = materialize(&keys_map[&Key::Int(1)], None, 0).unwrap();
                let k2 = materialize(&keys_map[&Key::Int(2)], None, 0).unwrap();
                assert_eq!(k0, Value::String("z".into()));
                assert_eq!(k1, Value::String("a".into()));
                assert_eq!(k2, Value::String("m".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- length ---

    #[test]
    fn length_empty_dict() {
        let dict = thunk_dict(IndexMap::new());
        let result = builtin_length(&[dict], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn length_non_empty_dict() {
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        map.insert(Key::String("b".into()), thunk(Value::Int(2)));
        map.insert(Key::String("c".into()), thunk(Value::Int(3)));
        let dict = thunk_dict(map);
        let result = builtin_length(&[dict], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn length_int_keyed_dict() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::String("x".into())));
        map.insert(Key::Int(1), thunk(Value::String("y".into())));
        let dict = thunk_dict(map);
        let result = builtin_length(&[dict], &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(2));
    }

    // --- merge ---

    #[test]
    fn merge_disjoint_keys() {
        let mut left = IndexMap::new();
        left.insert(Key::String("a".into()), thunk(Value::Int(1)));
        left.insert(Key::String("b".into()), thunk(Value::Int(2)));
        let mut right = IndexMap::new();
        right.insert(Key::String("c".into()), thunk(Value::Int(3)));
        right.insert(Key::String("d".into()), thunk(Value::Int(4)));

        let result = builtin_merge(&[thunk_dict(left), thunk_dict(right)], &no_named(), 0).unwrap();
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

        let result = builtin_merge(&[thunk_dict(left), thunk_dict(right)], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let x = materialize(&map[&Key::String("x".into())], None, 0).unwrap();
                assert_eq!(x, Value::Int(1));
                let y = materialize(&map[&Key::String("y".into())], None, 0).unwrap();
                assert_eq!(y, Value::Int(99));
                let z = materialize(&map[&Key::String("z".into())], None, 0).unwrap();
                assert_eq!(z, Value::Int(3));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn merge_empty_dicts() {
        let result = builtin_merge(
            &[thunk_dict(IndexMap::new()), thunk_dict(IndexMap::new())],
            &no_named(),
            0,
        )
        .unwrap();
        match result {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn merge_left_empty() {
        let mut right = IndexMap::new();
        right.insert(Key::Int(0), thunk(Value::String("only".into())));
        let result = builtin_merge(
            &[thunk_dict(IndexMap::new()), thunk_dict(right)],
            &no_named(),
            0,
        )
        .unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let v = materialize(&map[&Key::Int(0)], None, 0).unwrap();
                assert_eq!(v, Value::String("only".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn merge_right_empty() {
        let mut left = IndexMap::new();
        left.insert(Key::Int(0), thunk(Value::String("only".into())));
        let result = builtin_merge(
            &[thunk_dict(left), thunk_dict(IndexMap::new())],
            &no_named(),
            0,
        )
        .unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let v = materialize(&map[&Key::Int(0)], None, 0).unwrap();
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

        let result = builtin_merge(&[thunk_dict(left), thunk_dict(right)], &no_named(), 0).unwrap();
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

        let result = builtin_merge(&[thunk_dict(left), thunk_dict(right)], &no_named(), 0).unwrap();
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

    // --- Dict error cases ---

    #[test]
    fn keys_wrong_arity_zero() {
        let err = builtin_keys(&[], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn keys_wrong_arity_two() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_keys(&[d.clone(), d], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn length_wrong_arity_zero() {
        let err = builtin_length(&[], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn length_wrong_arity_two() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_length(&[d.clone(), d], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn merge_wrong_arity_one() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_merge(&[d], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn merge_wrong_arity_three() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_merge(&[d.clone(), d.clone(), d], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn keys_non_dict_int() {
        let err = builtin_keys(&[thunk(Value::Int(42))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("keys"), "got: {}", err.message);
        assert!(
            err.message.contains("expected Dict"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("got Int"), "got: {}", err.message);
    }

    #[test]
    fn keys_non_dict_string() {
        let err =
            builtin_keys(&[thunk(Value::String("hello".into()))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("keys"), "got: {}", err.message);
        assert!(err.message.contains("got String"), "got: {}", err.message);
    }

    #[test]
    fn keys_non_dict_bool() {
        let err = builtin_keys(&[thunk(Value::Bool(true))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("keys"), "got: {}", err.message);
        assert!(err.message.contains("got Bool"), "got: {}", err.message);
    }

    #[test]
    fn length_non_dict() {
        let err =
            builtin_length(&[thunk(Value::String("hello".into()))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("length"), "got: {}", err.message);
        assert!(
            err.message.contains("expected Dict"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("got String"), "got: {}", err.message);
    }

    #[test]
    fn merge_first_arg_non_dict() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_merge(&[thunk(Value::Int(1)), d], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("merge"), "got: {}", err.message);
        assert!(
            err.message.contains("expected Dict"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("got Int"), "got: {}", err.message);
    }

    #[test]
    fn merge_second_arg_non_dict() {
        let d = thunk_dict(IndexMap::new());
        let err =
            builtin_merge(&[d, thunk(Value::String("nope".into()))], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("merge"), "got: {}", err.message);
        assert!(
            err.message.contains("expected Dict"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("got String"), "got: {}", err.message);
    }

    // --- append ---

    #[test]
    fn append_to_empty_dict() {
        let empty = thunk_dict(IndexMap::new());
        let result = builtin_append(&[empty, thunk(Value::Int(42))], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let val = materialize(map.get(&Key::Int(0)).unwrap(), None, 0).unwrap();
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
        let result =
            builtin_append(&[dict, thunk(Value::String("c".into()))], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let val = materialize(map.get(&Key::Int(2)).unwrap(), None, 0).unwrap();
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
        let result = builtin_append(&[dict, thunk(Value::Int(99))], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let val = materialize(map.get(&Key::Int(0)).unwrap(), None, 0).unwrap();
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
        let result = builtin_append(&[dict, thunk(Value::Int(60))], &no_named(), 0).unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let val = materialize(map.get(&Key::Int(6)).unwrap(), None, 0).unwrap();
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
        let result = builtin_append(
            &[dict, thunk(Value::String("second".into()))],
            &no_named(),
            0,
        )
        .unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let first = materialize(map.get(&Key::Int(0)).unwrap(), None, 0).unwrap();
                assert_eq!(first, Value::String("first".into()));
                let second = materialize(map.get(&Key::Int(1)).unwrap(), None, 0).unwrap();
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
        let result = builtin_append(&[empty, Rc::clone(&val_thunk)], &no_named(), 0).unwrap();
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
        let err = builtin_append(&[], &no_named(), 0).unwrap_err();
        assert!(err.message.contains("2"), "got: {}", err.message);
    }

    #[test]
    fn append_wrong_arity_three() {
        let err = builtin_append(
            &[
                thunk_dict(IndexMap::new()),
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
            ],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(err.message.contains("2"), "got: {}", err.message);
    }

    #[test]
    fn append_first_arg_non_dict() {
        let err = builtin_append(
            &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(err.message.contains("append"), "got: {}", err.message);
        assert!(
            err.message.contains("expected Dict"),
            "got: {}",
            err.message
        );
    }

    // --- str ---

    #[test]
    fn str_no_args() {
        let result = builtin_str(&[], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn str_single_int() {
        let result = builtin_str(&[thunk(Value::Int(42))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("42".into()));
    }

    #[test]
    fn str_single_negative_int() {
        let result = builtin_str(&[thunk(Value::Int(-7))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("-7".into()));
    }

    #[test]
    fn str_single_float() {
        let result = builtin_str(&[thunk(Value::Float(3.14))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("3.14".into()));
    }

    #[test]
    fn str_single_string() {
        let result = builtin_str(&[thunk(Value::String("hello".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn str_single_bool_true() {
        let result = builtin_str(&[thunk(Value::Bool(true))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("true".into()));
    }

    #[test]
    fn str_single_bool_false() {
        let result = builtin_str(&[thunk(Value::Bool(false))], &no_named(), 0).unwrap();
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
        let result = builtin_str(&[thunk(Value::Dict(map))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("[x: <thunk>]".into()));
    }

    #[test]
    fn str_single_empty_string() {
        let result = builtin_str(&[thunk(Value::String("".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn str_concat_multiple_strings() {
        let args = vec![
            thunk(Value::String("Hello".into())),
            thunk(Value::String(" ".into())),
            thunk(Value::String("World".into())),
        ];
        let result = builtin_str(&args, &no_named(), 0).unwrap();
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
        let result = builtin_str(&args, &no_named(), 0).unwrap();
        assert_eq!(
            result,
            Value::String("count: 42, ratio: 3.14, ok: true".into())
        );
    }

    // --- split ---

    #[test]
    fn split_basic() {
        let result = builtin_split(
            &[
                thunk(Value::String(",".into())),
                thunk(Value::String("a,b,c".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v0 = materialize(map.get(&Key::Int(0)).unwrap(), None, 0).unwrap();
                let v1 = materialize(map.get(&Key::Int(1)).unwrap(), None, 0).unwrap();
                let v2 = materialize(map.get(&Key::Int(2)).unwrap(), None, 0).unwrap();
                assert_eq!(v0, Value::String("a".into()));
                assert_eq!(v1, Value::String("b".into()));
                assert_eq!(v2, Value::String("c".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_empty_parts() {
        let result = builtin_split(
            &[
                thunk(Value::String(",".into())),
                thunk(Value::String("a,,b".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v1 = materialize(map.get(&Key::Int(1)).unwrap(), None, 0).unwrap();
                assert_eq!(v1, Value::String("".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_single_char_separator() {
        let result = builtin_split(
            &[
                thunk(Value::String("/".into())),
                thunk(Value::String("a/b/c/d".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        match result {
            Value::Dict(map) => assert_eq!(map.len(), 4),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_no_match() {
        let result = builtin_split(
            &[
                thunk(Value::String(",".into())),
                thunk(Value::String("hello".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let v0 = materialize(map.get(&Key::Int(0)).unwrap(), None, 0).unwrap();
                assert_eq!(v0, Value::String("hello".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_multi_char_separator() {
        let result = builtin_split(
            &[
                thunk(Value::String("::".into())),
                thunk(Value::String("a::b::c".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v0 = materialize(map.get(&Key::Int(0)).unwrap(), None, 0).unwrap();
                let v1 = materialize(map.get(&Key::Int(1)).unwrap(), None, 0).unwrap();
                let v2 = materialize(map.get(&Key::Int(2)).unwrap(), None, 0).unwrap();
                assert_eq!(v0, Value::String("a".into()));
                assert_eq!(v1, Value::String("b".into()));
                assert_eq!(v2, Value::String("c".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_empty_input() {
        let result = builtin_split(
            &[
                thunk(Value::String(",".into())),
                thunk(Value::String("".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let v0 = materialize(map.get(&Key::Int(0)).unwrap(), None, 0).unwrap();
                assert_eq!(v0, Value::String("".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- replace ---

    #[test]
    fn replace_basic() {
        let result = builtin_replace(
            &[
                thunk(Value::String("world".into())),
                thunk(Value::String("Rust".into())),
                thunk(Value::String("hello world".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(result, Value::String("hello Rust".into()));
    }

    #[test]
    fn replace_multiple_occurrences() {
        let result = builtin_replace(
            &[
                thunk(Value::String("a".into())),
                thunk(Value::String("o".into())),
                thunk(Value::String("banana".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(result, Value::String("bonono".into()));
    }

    #[test]
    fn replace_no_match() {
        let result = builtin_replace(
            &[
                thunk(Value::String("xyz".into())),
                thunk(Value::String("abc".into())),
                thunk(Value::String("hello".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn replace_empty_pattern() {
        let result = builtin_replace(
            &[
                thunk(Value::String("".into())),
                thunk(Value::String("-".into())),
                thunk(Value::String("abc".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(result, Value::String("-a-b-c-".into()));
    }

    #[test]
    fn replace_to_empty() {
        let result = builtin_replace(
            &[
                thunk(Value::String("l".into())),
                thunk(Value::String("".into())),
                thunk(Value::String("hello".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(result, Value::String("heo".into()));
    }

    // --- upper ---

    #[test]
    fn upper_basic() {
        let result =
            builtin_upper(&[thunk(Value::String("hello".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("HELLO".into()));
    }

    #[test]
    fn upper_mixed_case() {
        let result = builtin_upper(
            &[thunk(Value::String("Hello World".into()))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(result, Value::String("HELLO WORLD".into()));
    }

    #[test]
    fn upper_already_upper() {
        let result = builtin_upper(&[thunk(Value::String("ABC".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("ABC".into()));
    }

    #[test]
    fn upper_empty() {
        let result = builtin_upper(&[thunk(Value::String("".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn upper_with_numbers() {
        let result =
            builtin_upper(&[thunk(Value::String("abc123".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("ABC123".into()));
    }

    // --- lower ---

    #[test]
    fn lower_basic() {
        let result =
            builtin_lower(&[thunk(Value::String("HELLO".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn lower_mixed_case() {
        let result = builtin_lower(
            &[thunk(Value::String("Hello World".into()))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(result, Value::String("hello world".into()));
    }

    #[test]
    fn lower_already_lower() {
        let result = builtin_lower(&[thunk(Value::String("abc".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("abc".into()));
    }

    #[test]
    fn lower_empty() {
        let result = builtin_lower(&[thunk(Value::String("".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("".into()));
    }

    // --- trim ---

    #[test]
    fn trim_basic() {
        let result =
            builtin_trim(&[thunk(Value::String("  hello  ".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_leading_only() {
        let result =
            builtin_trim(&[thunk(Value::String("   hello".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_trailing_only() {
        let result =
            builtin_trim(&[thunk(Value::String("hello   ".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_no_whitespace() {
        let result = builtin_trim(&[thunk(Value::String("hello".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_all_whitespace() {
        let result = builtin_trim(&[thunk(Value::String("   ".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn trim_tabs_and_newlines() {
        let result = builtin_trim(
            &[thunk(Value::String("\t\nhello\n\t".into()))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_empty() {
        let result = builtin_trim(&[thunk(Value::String("".into()))], &no_named(), 0).unwrap();
        assert_eq!(result, Value::String("".into()));
    }

    // --- String error cases: wrong arity ---

    #[test]
    fn split_wrong_arity_too_few() {
        let err = builtin_split(&[thunk(Value::String(",".into()))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("expected 2"), "got: {}", err.message);
    }

    #[test]
    fn split_wrong_arity_too_many() {
        let err = builtin_split(
            &[
                thunk(Value::String(",".into())),
                thunk(Value::String("a,b".into())),
                thunk(Value::String("extra".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn replace_wrong_arity() {
        let err = builtin_replace(
            &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("expected 3"), "got: {}", err.message);
    }

    #[test]
    fn upper_wrong_arity_zero() {
        let err = builtin_upper(&[], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn upper_wrong_arity_two() {
        let err = builtin_upper(
            &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn lower_wrong_arity() {
        let err = builtin_lower(&[], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn trim_wrong_arity() {
        let err = builtin_trim(
            &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    // --- String error cases: wrong types ---

    #[test]
    fn split_wrong_type_separator() {
        let err = builtin_split(
            &[thunk(Value::Int(42)), thunk(Value::String("hello".into()))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("expected String"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("got Int"), "got: {}", err.message);
    }

    #[test]
    fn split_wrong_type_input() {
        let err = builtin_split(
            &[thunk(Value::String(",".into())), thunk(Value::Int(42))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("expected String"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn replace_wrong_type_pattern() {
        let err = builtin_replace(
            &[
                thunk(Value::Int(1)),
                thunk(Value::String("b".into())),
                thunk(Value::String("abc".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("got Int"), "got: {}", err.message);
    }

    #[test]
    fn replace_wrong_type_replacement() {
        let err = builtin_replace(
            &[
                thunk(Value::String("a".into())),
                thunk(Value::Bool(true)),
                thunk(Value::String("abc".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("got Bool"), "got: {}", err.message);
    }

    #[test]
    fn replace_wrong_type_input() {
        let err = builtin_replace(
            &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
                thunk(Value::Float(3.14)),
            ],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("got Float"), "got: {}", err.message);
    }

    #[test]
    fn upper_wrong_type() {
        let err = builtin_upper(&[thunk(Value::Int(42))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("expected String"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("got Int"), "got: {}", err.message);
    }

    #[test]
    fn lower_wrong_type() {
        let err = builtin_lower(&[thunk(Value::Bool(true))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("expected String"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("got Bool"), "got: {}", err.message);
    }

    #[test]
    fn trim_wrong_type() {
        let err = builtin_trim(&[thunk(Value::Float(3.14))], &no_named(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("expected String"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("got Float"), "got: {}", err.message);
    }

    // --- Named argument rejection ---

    #[test]
    fn upper_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::String("hi".into())));
        let err = builtin_upper(&[thunk(Value::String("hello".into()))], &named, 0).unwrap_err();
        assert!(
            err.message.contains("named arguments"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn lower_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::String("hi".into())));
        let err = builtin_lower(&[thunk(Value::String("HELLO".into()))], &named, 0).unwrap_err();
        assert!(
            err.message.contains("named arguments"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn trim_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::String("hi".into())));
        let err = builtin_trim(&[thunk(Value::String("  hello  ".into()))], &named, 0).unwrap_err();
        assert!(
            err.message.contains("named arguments"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn eval_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_eval(&[thunk(Value::Int(42))], &named, 0).unwrap_err();
        assert!(
            err.message.contains("named arguments"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn error_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_error(&[thunk(Value::String("boom".into()))], &named, 0).unwrap_err();
        assert!(
            err.message.contains("named arguments"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn type_of_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_type_of(&[thunk(Value::Int(42))], &named, 0).unwrap_err();
        assert!(
            err.message.contains("named arguments"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn from_json_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_from_json(&[thunk(Value::String("42".into()))], &named, 0).unwrap_err();
        assert!(
            err.message.contains("named arguments"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn to_int_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_to_int(&[thunk(Value::String("42".into()))], &named, 0).unwrap_err();
        assert!(
            err.message.contains("named arguments"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn split_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_split(
            &[
                thunk(Value::String(",".into())),
                thunk(Value::String("a,b".into())),
            ],
            &named,
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("named arguments"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn replace_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_replace(
            &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
                thunk(Value::String("abc".into())),
            ],
            &named,
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("named arguments"),
            "got: {}",
            err.message
        );
    }

    // --- registry ---

    #[test]
    fn standard_builtins_contains_all() {
        let builtins = standard_builtins();
        let names: Vec<&str> = builtins.iter().map(|(name, _)| *name).collect();
        // Arithmetic
        assert!(names.contains(&"+"), "missing +");
        assert!(names.contains(&"-"), "missing -");
        assert!(names.contains(&"*"), "missing *");
        assert!(names.contains(&"/"), "missing /");
        assert!(names.contains(&"div"), "missing div");
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
        // Total count
        assert_eq!(names.len(), 28, "expected 28 builtins, got {}", names.len());
    }

    // --- Addition ($+) ---

    #[test]
    fn add_int_int() {
        let r = builtin_add(
            &[thunk(Value::Int(3)), thunk(Value::Int(5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(8));
    }

    #[test]
    fn add_int_float() {
        let r = builtin_add(
            &[thunk(Value::Int(3)), thunk(Value::Float(2.5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Float(5.5));
    }

    #[test]
    fn add_float_int() {
        let r = builtin_add(
            &[thunk(Value::Float(2.5)), thunk(Value::Int(3))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Float(5.5));
    }

    #[test]
    fn add_float_float() {
        let r = builtin_add(
            &[thunk(Value::Float(1.5)), thunk(Value::Float(2.5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Float(4.0));
    }

    #[test]
    fn add_negative_ints() {
        let r = builtin_add(
            &[thunk(Value::Int(-10)), thunk(Value::Int(3))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(-7));
    }

    #[test]
    fn add_zeros() {
        let r = builtin_add(
            &[thunk(Value::Int(0)), thunk(Value::Int(0))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn add_type_error_string() {
        let e = builtin_add(
            &[thunk(Value::Int(1)), thunk(Value::String("hello".into()))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(e.message.contains("type mismatch"), "got: {}", e.message);
    }

    #[test]
    fn add_arity_one_arg() {
        let e = builtin_add(&[thunk(Value::Int(1))], &no_named(), 0).unwrap_err();
        assert!(e.message.contains("arity mismatch"), "got: {}", e.message);
    }

    #[test]
    fn add_arity_three_args() {
        let e = builtin_add(
            &[
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(e.message.contains("arity mismatch"), "got: {}", e.message);
    }

    #[test]
    fn add_large_ints_wrapping() {
        let r = builtin_add(
            &[thunk(Value::Int(i64::MAX)), thunk(Value::Int(1))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(i64::MIN));
    }

    // --- Subtraction ($-) ---

    #[test]
    fn sub_int_int() {
        let r = builtin_sub(
            &[thunk(Value::Int(10)), thunk(Value::Int(3))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(7));
    }

    #[test]
    fn sub_int_float() {
        let r = builtin_sub(
            &[thunk(Value::Int(10)), thunk(Value::Float(3.5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Float(6.5));
    }

    #[test]
    fn sub_float_int() {
        let r = builtin_sub(
            &[thunk(Value::Float(10.5)), thunk(Value::Int(3))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Float(7.5));
    }

    #[test]
    fn sub_float_float() {
        let r = builtin_sub(
            &[thunk(Value::Float(10.5)), thunk(Value::Float(3.5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Float(7.0));
    }

    #[test]
    fn sub_result_negative() {
        let r = builtin_sub(
            &[thunk(Value::Int(3)), thunk(Value::Int(10))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(-7));
    }

    #[test]
    fn sub_to_zero() {
        let r = builtin_sub(
            &[thunk(Value::Int(5)), thunk(Value::Int(5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn sub_arity_zero_args() {
        let e = builtin_sub(&[], &no_named(), 0).unwrap_err();
        assert!(e.message.contains("arity mismatch"), "got: {}", e.message);
    }

    #[test]
    fn sub_arity_one_arg() {
        let e = builtin_sub(&[thunk(Value::Int(1))], &no_named(), 0).unwrap_err();
        assert!(e.message.contains("arity mismatch"), "got: {}", e.message);
    }

    #[test]
    fn sub_arity_three_args() {
        let e = builtin_sub(
            &[
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(e.message.contains("arity mismatch"), "got: {}", e.message);
    }

    #[test]
    fn sub_type_error_string() {
        let e = builtin_sub(
            &[thunk(Value::Int(1)), thunk(Value::String("hello".into()))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(e.message.contains("type mismatch"), "got: {}", e.message);
    }

    // --- Multiplication ($*) ---

    #[test]
    fn mul_int_int() {
        let r = builtin_mul(
            &[thunk(Value::Int(4)), thunk(Value::Int(5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(20));
    }

    #[test]
    fn mul_int_float() {
        let r = builtin_mul(
            &[thunk(Value::Int(4)), thunk(Value::Float(2.5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Float(10.0));
    }

    #[test]
    fn mul_float_int() {
        let r = builtin_mul(
            &[thunk(Value::Float(2.5)), thunk(Value::Int(4))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Float(10.0));
    }

    #[test]
    fn mul_float_float() {
        let r = builtin_mul(
            &[thunk(Value::Float(2.5)), thunk(Value::Float(3.0))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Float(7.5));
    }

    #[test]
    fn mul_by_zero() {
        let r = builtin_mul(
            &[thunk(Value::Int(42)), thunk(Value::Int(0))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn mul_negative() {
        let r = builtin_mul(
            &[thunk(Value::Int(-3)), thunk(Value::Int(4))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(-12));
    }

    #[test]
    fn mul_by_negative_one() {
        let r = builtin_mul(
            &[thunk(Value::Int(42)), thunk(Value::Int(-1))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(-42));
    }

    #[test]
    fn mul_overflow_wrapping() {
        let r = builtin_mul(
            &[thunk(Value::Int(i64::MAX)), thunk(Value::Int(2))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(i64::MAX.wrapping_mul(2)));
    }

    // --- Float division ($/) ---

    #[test]
    fn div_float_int_int_returns_float() {
        let r = builtin_div_float(
            &[thunk(Value::Int(10)), thunk(Value::Int(3))],
            &no_named(),
            0,
        )
        .unwrap();
        match r {
            Value::Float(f) => {
                assert!((f - 10.0 / 3.0).abs() < 1e-10, "expected ~3.333, got {f}")
            }
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn div_float_int_int_exact_returns_float() {
        let r = builtin_div_float(
            &[thunk(Value::Int(10)), thunk(Value::Int(2))],
            &no_named(),
            0,
        )
        .unwrap();
        match r {
            Value::Float(f) => assert_eq!(f, 5.0),
            other => panic!("expected Float(5.0), got {other:?}"),
        }
    }

    #[test]
    fn div_float_int_float() {
        let r = builtin_div_float(
            &[thunk(Value::Int(10)), thunk(Value::Float(3.0))],
            &no_named(),
            0,
        )
        .unwrap();
        match r {
            Value::Float(f) => {
                assert!((f - 10.0 / 3.0).abs() < 1e-10, "expected ~3.333, got {f}")
            }
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn div_float_float_float() {
        let r = builtin_div_float(
            &[thunk(Value::Float(7.5)), thunk(Value::Float(2.5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Float(3.0));
    }

    #[test]
    fn div_float_by_zero_int() {
        let e = builtin_div_float(
            &[thunk(Value::Int(10)), thunk(Value::Int(0))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(e.message.contains("division by zero"), "got: {}", e.message);
    }

    #[test]
    fn div_float_by_zero_float() {
        let e = builtin_div_float(
            &[thunk(Value::Float(10.0)), thunk(Value::Float(0.0))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(e.message.contains("division by zero"), "got: {}", e.message);
    }

    #[test]
    fn div_float_by_zero_mixed() {
        let e = builtin_div_float(
            &[thunk(Value::Int(10)), thunk(Value::Float(0.0))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(e.message.contains("division by zero"), "got: {}", e.message);
    }

    #[test]
    fn div_float_negative_zero() {
        let r = builtin_div_float(
            &[thunk(Value::Float(-0.0)), thunk(Value::Float(1.0))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Float(0.0));
    }

    // --- Integer division ($div) ---

    #[test]
    fn div_int_basic() {
        let r = builtin_div_int(
            &[thunk(Value::Int(10)), thunk(Value::Int(3))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(3));
    }

    #[test]
    fn div_int_exact() {
        let r = builtin_div_int(
            &[thunk(Value::Int(10)), thunk(Value::Int(2))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(5));
    }

    #[test]
    fn div_int_negative_truncates_toward_zero() {
        let r = builtin_div_int(
            &[thunk(Value::Int(-10)), thunk(Value::Int(3))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(-3));
    }

    #[test]
    fn div_int_by_zero() {
        let e = builtin_div_int(
            &[thunk(Value::Int(10)), thunk(Value::Int(0))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(e.message.contains("division by zero"), "got: {}", e.message);
    }

    #[test]
    fn div_int_float_inputs_trunc() {
        let r = builtin_div_int(
            &[thunk(Value::Float(10.0)), thunk(Value::Float(3.0))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(3));
    }

    #[test]
    fn div_int_float_negative_truncates_toward_zero() {
        // trunc(-10.0 / 3.0) = trunc(-3.333...) = -3
        let r = builtin_div_int(
            &[thunk(Value::Float(-10.0)), thunk(Value::Float(3.0))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(-3));
    }

    #[test]
    fn div_int_mixed_int_float() {
        let r = builtin_div_int(
            &[thunk(Value::Int(7)), thunk(Value::Float(2.0))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(3));
    }

    #[test]
    fn div_int_float_by_zero() {
        let e = builtin_div_int(
            &[thunk(Value::Float(10.0)), thunk(Value::Float(0.0))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(e.message.contains("division by zero"), "got: {}", e.message);
    }

    #[test]
    fn div_int_one() {
        let r = builtin_div_int(
            &[thunk(Value::Int(7)), thunk(Value::Int(1))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Int(7));
    }

    #[test]
    fn div_int_float_large_result_out_of_range() {
        // 1e19 / 1.0 = 1e19, which exceeds i64::MAX
        let err = builtin_div_int(
            &[thunk(Value::Float(1e19)), thunk(Value::Float(1.0))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("out of Int range"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn div_int_float_large_negative_result_out_of_range() {
        let err = builtin_div_int(
            &[thunk(Value::Float(-1e19)), thunk(Value::Float(1.0))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(
            err.message.contains("out of Int range"),
            "got: {}",
            err.message
        );
    }

    // --- Equality ($=) ---

    #[test]
    fn eq_int_int_equal() {
        let r = builtin_eq(
            &[thunk(Value::Int(5)), thunk(Value::Int(5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_int_int_not_equal() {
        let r = builtin_eq(
            &[thunk(Value::Int(5)), thunk(Value::Int(6))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_float_float_equal() {
        let r = builtin_eq(
            &[thunk(Value::Float(3.14)), thunk(Value::Float(3.14))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_float_float_not_equal() {
        let r = builtin_eq(
            &[thunk(Value::Float(3.14)), thunk(Value::Float(2.71))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_string_equal() {
        let r = builtin_eq(
            &[
                thunk(Value::String("hello".into())),
                thunk(Value::String("hello".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_string_not_equal() {
        let r = builtin_eq(
            &[
                thunk(Value::String("hello".into())),
                thunk(Value::String("world".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_bool_equal() {
        let r = builtin_eq(
            &[thunk(Value::Bool(true)), thunk(Value::Bool(true))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_bool_not_equal() {
        let r = builtin_eq(
            &[thunk(Value::Bool(true)), thunk(Value::Bool(false))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_cross_type_int_float_equal() {
        let r = builtin_eq(
            &[thunk(Value::Int(5)), thunk(Value::Float(5.0))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_cross_type_float_int_equal() {
        let r = builtin_eq(
            &[thunk(Value::Float(5.0)), thunk(Value::Int(5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_cross_type_int_float_not_equal() {
        let r = builtin_eq(
            &[thunk(Value::Int(5)), thunk(Value::Float(5.1))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_dict_never_equal() {
        let r = builtin_eq(
            &[
                thunk(Value::Dict(IndexMap::new())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_different_types_not_equal() {
        let r = builtin_eq(
            &[thunk(Value::Int(1)), thunk(Value::String("1".into()))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_bool_vs_int_not_equal() {
        let r = builtin_eq(
            &[thunk(Value::Bool(true)), thunk(Value::Int(1))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_nan_not_equal_to_self() {
        let r = builtin_eq(
            &[thunk(Value::Float(f64::NAN)), thunk(Value::Float(f64::NAN))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_negative_zero_float() {
        let r = builtin_eq(
            &[thunk(Value::Float(-0.0)), thunk(Value::Float(0.0))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_arity_error() {
        let e = builtin_eq(&[thunk(Value::Int(1))], &no_named(), 0).unwrap_err();
        assert!(e.message.contains("arity mismatch"), "got: {}", e.message);
    }

    // --- Less-than ($<) ---

    #[test]
    fn lt_int_int_true() {
        let r = builtin_lt(
            &[thunk(Value::Int(3)), thunk(Value::Int(5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_int_int_false() {
        let r = builtin_lt(
            &[thunk(Value::Int(5)), thunk(Value::Int(3))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_int_int_equal_is_false() {
        let r = builtin_lt(
            &[thunk(Value::Int(5)), thunk(Value::Int(5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_float_float() {
        let r = builtin_lt(
            &[thunk(Value::Float(2.5)), thunk(Value::Float(3.5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_string_lexicographic() {
        let r = builtin_lt(
            &[
                thunk(Value::String("apple".into())),
                thunk(Value::String("banana".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_string_lexicographic_reverse() {
        let r = builtin_lt(
            &[
                thunk(Value::String("banana".into())),
                thunk(Value::String("apple".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_string_equal_is_false() {
        let r = builtin_lt(
            &[
                thunk(Value::String("same".into())),
                thunk(Value::String("same".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_string_prefix() {
        let r = builtin_lt(
            &[
                thunk(Value::String("ab".into())),
                thunk(Value::String("abc".into())),
            ],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_cross_type_int_float() {
        let r = builtin_lt(
            &[thunk(Value::Int(3)), thunk(Value::Float(3.5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_cross_type_float_int() {
        let r = builtin_lt(
            &[thunk(Value::Float(2.5)), thunk(Value::Int(3))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_cross_type_equal_values() {
        let r = builtin_lt(
            &[thunk(Value::Int(5)), thunk(Value::Float(5.0))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_incompatible_types_error() {
        let e = builtin_lt(
            &[thunk(Value::Int(1)), thunk(Value::String("hello".into()))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(e.message.contains("type mismatch"), "got: {}", e.message);
    }

    #[test]
    fn lt_bool_error() {
        let e = builtin_lt(
            &[thunk(Value::Bool(true)), thunk(Value::Bool(false))],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(e.message.contains("type mismatch"), "got: {}", e.message);
    }

    #[test]
    fn lt_dict_error() {
        let e = builtin_lt(
            &[
                thunk(Value::Dict(IndexMap::new())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            &no_named(),
            0,
        )
        .unwrap_err();
        assert!(e.message.contains("type mismatch"), "got: {}", e.message);
    }

    #[test]
    fn lt_arity_error() {
        let e = builtin_lt(&[thunk(Value::Int(1))], &no_named(), 0).unwrap_err();
        assert!(e.message.contains("arity mismatch"), "got: {}", e.message);
    }

    #[test]
    fn lt_negative_numbers() {
        let r = builtin_lt(
            &[thunk(Value::Int(-10)), thunk(Value::Int(-5))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_nan_float() {
        let r = builtin_lt(
            &[thunk(Value::Float(f64::NAN)), thunk(Value::Float(1.0))],
            &no_named(),
            0,
        )
        .unwrap();
        assert_eq!(r, Value::Bool(false));
    }

    // --- Conditional ($if) ---

    #[test]
    fn if_true_returns_then_branch() {
        let args = vec![
            thunk(Value::Bool(true)),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let result = builtin_if(&args, &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn if_false_returns_else_branch() {
        let args = vec![
            thunk(Value::Bool(false)),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let result = builtin_if(&args, &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(99));
    }

    #[test]
    fn if_does_not_materialize_unchosen_else_branch() {
        let error_expr = Spanned::new(
            Expr::VarRef("nonexistent".to_string()),
            test_span(1, 1, 1, 10),
        );
        let env = Rc::new(RefCell::new(Environment::new()));
        let error_thunk = Rc::new(Thunk::new_unevaluated(
            error_expr,
            env,
            test_span(1, 1, 1, 10),
        ));

        let args = vec![thunk(Value::Bool(true)), thunk(Value::Int(42)), error_thunk];
        let result = builtin_if(&args, &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn if_does_not_materialize_unchosen_then_branch() {
        let error_expr = Spanned::new(
            Expr::VarRef("nonexistent".to_string()),
            test_span(1, 1, 1, 10),
        );
        let env = Rc::new(RefCell::new(Environment::new()));
        let error_thunk = Rc::new(Thunk::new_unevaluated(
            error_expr,
            env,
            test_span(1, 1, 1, 10),
        ));

        let args = vec![
            thunk(Value::Bool(false)),
            error_thunk,
            thunk(Value::Int(99)),
        ];
        let result = builtin_if(&args, &no_named(), 0).unwrap();
        assert_eq!(result, Value::Int(99));
    }

    #[test]
    fn if_non_bool_condition_error() {
        let args = vec![
            thunk(Value::Int(1)),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let e = builtin_if(&args, &no_named(), 0).unwrap_err();
        assert!(e.message.contains("type mismatch"), "got: {}", e.message);
        assert!(
            e.message.contains("Bool"),
            "expected Bool mentioned, got: {}",
            e.message
        );
    }

    #[test]
    fn if_string_condition_error() {
        let args = vec![
            thunk(Value::String("true".into())),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let e = builtin_if(&args, &no_named(), 0).unwrap_err();
        assert!(e.message.contains("type mismatch"), "got: {}", e.message);
    }

    #[test]
    fn if_arity_too_few() {
        let args = vec![thunk(Value::Bool(true)), thunk(Value::Int(42))];
        let e = builtin_if(&args, &no_named(), 0).unwrap_err();
        assert!(e.message.contains("arity mismatch"), "got: {}", e.message);
    }

    #[test]
    fn if_arity_too_many() {
        let args = vec![
            thunk(Value::Bool(true)),
            thunk(Value::Int(1)),
            thunk(Value::Int(2)),
            thunk(Value::Int(3)),
        ];
        let e = builtin_if(&args, &no_named(), 0).unwrap_err();
        assert!(e.message.contains("arity mismatch"), "got: {}", e.message);
    }

    // --- create_root_env ---

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

    // --- create_stdlib_env ---

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
}
