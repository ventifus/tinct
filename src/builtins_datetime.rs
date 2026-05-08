//! Date-time builtins: timestamps, durations, clock capabilities, and timezones.
//!
//! Design: Timestamp and Duration are i64 nanoseconds (UTC epoch and signed span).
//! ClockCap provides injectable time access (real or fixed for testing).
//! Timezone reads system zoneinfo via DirCap for timezone conversions.
//!
//! See doc/whatif/lib-datetime.md for the full specification.

use crate::ast::Span;
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::value::{string_val, BuiltinArgs, ClockCapInner, Key, Thunk, Value};
use indexmap::IndexMap;
use std::rc::Rc;
use std::str::FromStr;

/// Helper to create a boxed EvalError from a message.
fn dt_err(msg: impl Into<String>, span: Span) -> Box<EvalError> {
    EvalError::new(msg.into(), span).into()
}

/// Parse an RFC 3339 timestamp string to a Timestamp (i64 nanoseconds).
/// Errors if the format is invalid or the timestamp overflows i64 nanoseconds.
pub fn builtin_parse_timestamp(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [s_thunk] = args.args else {
        return Err(dt_err(
            "parse-timestamp requires 1 argument",
            args.call_span,
        ));
    };

    let s_val = materialize(s_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let s = s_val
        .as_str()
        .ok_or_else(|| dt_err("parse-timestamp requires a String", args.call_span))?;

    // Parse RFC 3339 string using jiff
    let ts = jiff::Timestamp::from_str(s).map_err(|e| {
        dt_err(
            format!("invalid RFC 3339 timestamp: {e}"),
            args.call_span,
        )
    })?;

    // Convert to nanoseconds since epoch
    let nanos = i64::try_from(ts.as_nanosecond()).unwrap_or(i64::MAX);

    Ok(Rc::new(Thunk::new_materialized(
        Value::Timestamp(nanos),
        args.call_span,
    )))
}

/// Format a Timestamp as an RFC 3339 string.
pub fn builtin_format_timestamp(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t_thunk] = args.args else {
        return Err(dt_err(
            "format-timestamp requires 1 argument",
            args.call_span,
        ));
    };

    let t_val = materialize(t_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let nanos = match &t_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "format-timestamp requires a Timestamp",
                args.call_span,
            ))
        }
    };

    // Convert nanoseconds to jiff::Timestamp
    let ts = jiff::Timestamp::from_nanosecond(nanos as i128).map_err(|e| {
        dt_err(
            format!("invalid timestamp value: {e}"),
            args.call_span,
        )
    })?;

    // Format as RFC 3339 string
    let s = ts.to_string();

    Ok(Rc::new(Thunk::new_materialized(
        string_val(&s),
        args.call_span,
    )))
}

/// Convert a Timestamp to Unix seconds (truncating nanoseconds).
pub fn builtin_timestamp_to_unix(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t_thunk] = args.args else {
        return Err(dt_err(
            "timestamp->unix requires 1 argument",
            args.call_span,
        ));
    };

    let t_val = materialize(t_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let nanos = match &t_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp->unix requires a Timestamp",
                args.call_span,
            ))
        }
    };

    let seconds = nanos / 1_000_000_000;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Int(seconds),
        args.call_span,
    )))
}

/// Convert Unix seconds to a Timestamp.
pub fn builtin_unix_to_timestamp(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [n_thunk] = args.args else {
        return Err(dt_err(
            "unix->timestamp requires 1 argument",
            args.call_span,
        ));
    };

    let n_val = materialize(n_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let seconds = match &n_val {
        Value::Int(n) => *n,
        _ => {
            return Err(dt_err(
                "unix->timestamp requires an Int",
                args.call_span,
            ))
        }
    };

    let nanos = seconds
        .checked_mul(1_000_000_000)
        .ok_or_else(|| dt_err("unix->timestamp overflow", args.call_span))?;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Timestamp(nanos),
        args.call_span,
    )))
}

/// Read the current time from a ClockCap.
pub fn builtin_now(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [cap_thunk] = args.args else {
        return Err(dt_err("now requires 1 argument", args.call_span));
    };

    let cap_val = materialize(cap_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let clock_cap = match &cap_val {
        Value::ClockCap(inner) => inner,
        _ => return Err(dt_err("now requires a ClockCap", args.call_span)),
    };

    let nanos = match clock_cap.as_ref() {
        ClockCapInner::Real => {
            // Read the real system clock
            let ts = jiff::Timestamp::now();
            i64::try_from(ts.as_nanosecond()).unwrap_or(i64::MAX)
        }
        ClockCapInner::Fixed(nanos) => *nanos,
    };

    Ok(Rc::new(Thunk::new_materialized(
        Value::Timestamp(nanos),
        args.call_span,
    )))
}

/// Create a fixed ClockCap that always returns the given timestamp.
pub fn builtin_fixed_clock(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t_thunk] = args.args else {
        return Err(dt_err(
            "fixed-clock requires 1 argument",
            args.call_span,
        ));
    };

    let t_val = materialize(t_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let nanos = match &t_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "fixed-clock requires a Timestamp",
                args.call_span,
            ))
        }
    };

    Ok(Rc::new(Thunk::new_materialized(
        Value::ClockCap(Rc::new(ClockCapInner::Fixed(nanos))),
        args.call_span,
    )))
}

/// Add a duration to a timestamp.
pub fn builtin_timestamp_add(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t_thunk, d_thunk] = args.args else {
        return Err(dt_err(
            "timestamp-add requires 2 arguments",
            args.call_span,
        ));
    };

    let t_val = materialize(t_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let d_val = materialize(d_thunk, Some(&args.call_span), &args.ctx, args.depth)?;

    let t_nanos = match &t_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp-add requires Timestamp as first argument",
                args.call_span,
            ))
        }
    };

    let d_nanos = match &d_val {
        Value::Duration(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp-add requires Duration as second argument",
                args.call_span,
            ))
        }
    };

    let result = t_nanos
        .checked_add(d_nanos)
        .ok_or_else(|| dt_err("timestamp-add overflow", args.call_span))?;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Timestamp(result),
        args.call_span,
    )))
}

/// Compute the duration between two timestamps (t1 - t2).
pub fn builtin_timestamp_diff(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t1_thunk, t2_thunk] = args.args else {
        return Err(dt_err(
            "timestamp-diff requires 2 arguments",
            args.call_span,
        ));
    };

    let t1_val = materialize(t1_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let t2_val = materialize(t2_thunk, Some(&args.call_span), &args.ctx, args.depth)?;

    let t1_nanos = match &t1_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp-diff requires Timestamp as first argument",
                args.call_span,
            ))
        }
    };

    let t2_nanos = match &t2_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp-diff requires Timestamp as second argument",
                args.call_span,
            ))
        }
    };

    let result = t1_nanos
        .checked_sub(t2_nanos)
        .ok_or_else(|| dt_err("timestamp-diff overflow", args.call_span))?;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Duration(result),
        args.call_span,
    )))
}

/// Compare two timestamps: t1 < t2
pub fn builtin_timestamp_lt(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t1_thunk, t2_thunk] = args.args else {
        return Err(dt_err(
            "timestamp<? requires 2 arguments",
            args.call_span,
        ));
    };

    let t1_val = materialize(t1_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let t2_val = materialize(t2_thunk, Some(&args.call_span), &args.ctx, args.depth)?;

    let t1_nanos = match &t1_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp<? requires Timestamp as first argument",
                args.call_span,
            ))
        }
    };

    let t2_nanos = match &t2_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp<? requires Timestamp as second argument",
                args.call_span,
            ))
        }
    };

    Ok(Rc::new(Thunk::new_materialized(
        Value::Bool(t1_nanos < t2_nanos),
        args.call_span,
    )))
}

/// Compare two timestamps: t1 > t2
pub fn builtin_timestamp_gt(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t1_thunk, t2_thunk] = args.args else {
        return Err(dt_err(
            "timestamp>? requires 2 arguments",
            args.call_span,
        ));
    };

    let t1_val = materialize(t1_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let t2_val = materialize(t2_thunk, Some(&args.call_span), &args.ctx, args.depth)?;

    let t1_nanos = match &t1_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp>? requires Timestamp as first argument",
                args.call_span,
            ))
        }
    };

    let t2_nanos = match &t2_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp>? requires Timestamp as second argument",
                args.call_span,
            ))
        }
    };

    Ok(Rc::new(Thunk::new_materialized(
        Value::Bool(t1_nanos > t2_nanos),
        args.call_span,
    )))
}

/// Compare two timestamps for equality: t1 == t2
pub fn builtin_timestamp_eq(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t1_thunk, t2_thunk] = args.args else {
        return Err(dt_err(
            "timestamp=? requires 2 arguments",
            args.call_span,
        ));
    };

    let t1_val = materialize(t1_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let t2_val = materialize(t2_thunk, Some(&args.call_span), &args.ctx, args.depth)?;

    let t1_nanos = match &t1_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp=? requires Timestamp as first argument",
                args.call_span,
            ))
        }
    };

    let t2_nanos = match &t2_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp=? requires Timestamp as second argument",
                args.call_span,
            ))
        }
    };

    Ok(Rc::new(Thunk::new_materialized(
        Value::Bool(t1_nanos == t2_nanos),
        args.call_span,
    )))
}

/// Extract the UTC year from a timestamp.
pub fn builtin_timestamp_year(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t_thunk] = args.args else {
        return Err(dt_err(
            "timestamp-year requires 1 argument",
            args.call_span,
        ));
    };

    let t_val = materialize(t_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let nanos = match &t_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp-year requires a Timestamp",
                args.call_span,
            ))
        }
    };

    let ts = jiff::Timestamp::from_nanosecond(nanos as i128).map_err(|e| {
        dt_err(
            format!("invalid timestamp value: {e}"),
            args.call_span,
        )
    })?;

    let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
    let year = dt.year() as i64;

    Ok(Rc::new(Thunk::new_materialized(Value::Int(year), args.call_span)))
}

/// Extract the UTC month (1-12) from a timestamp.
pub fn builtin_timestamp_month(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t_thunk] = args.args else {
        return Err(dt_err(
            "timestamp-month requires 1 argument",
            args.call_span,
        ));
    };

    let t_val = materialize(t_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let nanos = match &t_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp-month requires a Timestamp",
                args.call_span,
            ))
        }
    };

    let ts = jiff::Timestamp::from_nanosecond(nanos as i128).map_err(|e| {
        dt_err(
            format!("invalid timestamp value: {e}"),
            args.call_span,
        )
    })?;

    let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
    let month = dt.month() as i64;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Int(month),
        args.call_span,
    )))
}

/// Extract the UTC day (1-31) from a timestamp.
pub fn builtin_timestamp_day(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t_thunk] = args.args else {
        return Err(dt_err(
            "timestamp-day requires 1 argument",
            args.call_span,
        ));
    };

    let t_val = materialize(t_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let nanos = match &t_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp-day requires a Timestamp",
                args.call_span,
            ))
        }
    };

    let ts = jiff::Timestamp::from_nanosecond(nanos as i128).map_err(|e| {
        dt_err(
            format!("invalid timestamp value: {e}"),
            args.call_span,
        )
    })?;

    let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
    let day = dt.day() as i64;

    Ok(Rc::new(Thunk::new_materialized(Value::Int(day), args.call_span)))
}

/// Extract the UTC hour (0-23) from a timestamp.
pub fn builtin_timestamp_hour(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t_thunk] = args.args else {
        return Err(dt_err(
            "timestamp-hour requires 1 argument",
            args.call_span,
        ));
    };

    let t_val = materialize(t_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let nanos = match &t_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp-hour requires a Timestamp",
                args.call_span,
            ))
        }
    };

    let ts = jiff::Timestamp::from_nanosecond(nanos as i128).map_err(|e| {
        dt_err(
            format!("invalid timestamp value: {e}"),
            args.call_span,
        )
    })?;

    let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
    let hour = dt.hour() as i64;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Int(hour),
        args.call_span,
    )))
}

/// Extract the UTC minute (0-59) from a timestamp.
pub fn builtin_timestamp_minute(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t_thunk] = args.args else {
        return Err(dt_err(
            "timestamp-minute requires 1 argument",
            args.call_span,
        ));
    };

    let t_val = materialize(t_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let nanos = match &t_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp-minute requires a Timestamp",
                args.call_span,
            ))
        }
    };

    let ts = jiff::Timestamp::from_nanosecond(nanos as i128).map_err(|e| {
        dt_err(
            format!("invalid timestamp value: {e}"),
            args.call_span,
        )
    })?;

    let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
    let minute = dt.minute() as i64;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Int(minute),
        args.call_span,
    )))
}

/// Extract the UTC second (0-59) from a timestamp.
pub fn builtin_timestamp_second(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t_thunk] = args.args else {
        return Err(dt_err(
            "timestamp-second requires 1 argument",
            args.call_span,
        ));
    };

    let t_val = materialize(t_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let nanos = match &t_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp-second requires a Timestamp",
                args.call_span,
            ))
        }
    };

    let ts = jiff::Timestamp::from_nanosecond(nanos as i128).map_err(|e| {
        dt_err(
            format!("invalid timestamp value: {e}"),
            args.call_span,
        )
    })?;

    let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
    let second = dt.second() as i64;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Int(second),
        args.call_span,
    )))
}

/// Extract all UTC components from a timestamp as a dict.
pub fn builtin_timestamp_parts(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t_thunk] = args.args else {
        return Err(dt_err(
            "timestamp-parts requires 1 argument",
            args.call_span,
        ));
    };

    let t_val = materialize(t_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let nanos = match &t_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp-parts requires a Timestamp",
                args.call_span,
            ))
        }
    };

    let ts = jiff::Timestamp::from_nanosecond(nanos as i128).map_err(|e| {
        dt_err(
            format!("invalid timestamp value: {e}"),
            args.call_span,
        )
    })?;

    let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);

    let mut map = IndexMap::new();
    map.insert(
        Key::String("year".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(dt.year() as i64),
            args.call_span,
        ))),
    );
    map.insert(
        Key::String("month".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(dt.month() as i64),
            args.call_span,
        ))),
    );
    map.insert(
        Key::String("day".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(dt.day() as i64),
            args.call_span,
        ))),
    );
    map.insert(
        Key::String("hour".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(dt.hour() as i64),
            args.call_span,
        ))),
    );
    map.insert(
        Key::String("minute".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(dt.minute() as i64),
            args.call_span,
        ))),
    );
    map.insert(
        Key::String("second".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(dt.second() as i64),
            args.call_span,
        ))),
    );

    Ok(Rc::new(Thunk::new_materialized(Value::Dict(map), args.call_span)))
}

/// Create a duration from nanoseconds.
pub fn builtin_duration_nanos(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [n_thunk] = args.args else {
        return Err(dt_err(
            "duration-nanos requires 1 argument",
            args.call_span,
        ));
    };

    let n_val = materialize(n_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let nanos = match &n_val {
        Value::Int(n) => *n,
        _ => {
            return Err(dt_err(
                "duration-nanos requires an Int",
                args.call_span,
            ))
        }
    };

    Ok(Rc::new(Thunk::new_materialized(
        Value::Duration(nanos),
        args.call_span,
    )))
}

/// Create a duration from seconds.
pub fn builtin_duration_seconds(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [n_thunk] = args.args else {
        return Err(dt_err(
            "duration-seconds requires 1 argument",
            args.call_span,
        ));
    };

    let n_val = materialize(n_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let seconds = match &n_val {
        Value::Int(n) => *n,
        _ => {
            return Err(dt_err(
                "duration-seconds requires an Int",
                args.call_span,
            ))
        }
    };

    let nanos = seconds
        .checked_mul(1_000_000_000)
        .ok_or_else(|| dt_err("duration-seconds overflow", args.call_span))?;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Duration(nanos),
        args.call_span,
    )))
}

/// Create a duration from minutes.
pub fn builtin_duration_minutes(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [n_thunk] = args.args else {
        return Err(dt_err(
            "duration-minutes requires 1 argument",
            args.call_span,
        ));
    };

    let n_val = materialize(n_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let minutes = match &n_val {
        Value::Int(n) => *n,
        _ => {
            return Err(dt_err(
                "duration-minutes requires an Int",
                args.call_span,
            ))
        }
    };

    let nanos = minutes
        .checked_mul(60)
        .and_then(|s| s.checked_mul(1_000_000_000))
        .ok_or_else(|| dt_err("duration-minutes overflow", args.call_span))?;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Duration(nanos),
        args.call_span,
    )))
}

/// Create a duration from hours.
pub fn builtin_duration_hours(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [n_thunk] = args.args else {
        return Err(dt_err(
            "duration-hours requires 1 argument",
            args.call_span,
        ));
    };

    let n_val = materialize(n_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let hours = match &n_val {
        Value::Int(n) => *n,
        _ => {
            return Err(dt_err(
                "duration-hours requires an Int",
                args.call_span,
            ))
        }
    };

    let nanos = hours
        .checked_mul(3600)
        .and_then(|s| s.checked_mul(1_000_000_000))
        .ok_or_else(|| dt_err("duration-hours overflow", args.call_span))?;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Duration(nanos),
        args.call_span,
    )))
}

/// Create a duration from days.
pub fn builtin_duration_days(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [n_thunk] = args.args else {
        return Err(dt_err(
            "duration-days requires 1 argument",
            args.call_span,
        ));
    };

    let n_val = materialize(n_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let days = match &n_val {
        Value::Int(n) => *n,
        _ => {
            return Err(dt_err(
                "duration-days requires an Int",
                args.call_span,
            ))
        }
    };

    let nanos = days
        .checked_mul(86400)
        .and_then(|s| s.checked_mul(1_000_000_000))
        .ok_or_else(|| dt_err("duration-days overflow", args.call_span))?;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Duration(nanos),
        args.call_span,
    )))
}

/// Convert a duration to seconds (truncating nanoseconds).
pub fn builtin_duration_to_seconds(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [d_thunk] = args.args else {
        return Err(dt_err(
            "duration->seconds requires 1 argument",
            args.call_span,
        ));
    };

    let d_val = materialize(d_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let nanos = match &d_val {
        Value::Duration(n) => *n,
        _ => {
            return Err(dt_err(
                "duration->seconds requires a Duration",
                args.call_span,
            ))
        }
    };

    let seconds = nanos / 1_000_000_000;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Int(seconds),
        args.call_span,
    )))
}

/// Convert a duration to nanoseconds.
pub fn builtin_duration_to_nanos(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [d_thunk] = args.args else {
        return Err(dt_err(
            "duration->nanos requires 1 argument",
            args.call_span,
        ));
    };

    let d_val = materialize(d_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let nanos = match &d_val {
        Value::Duration(n) => *n,
        _ => {
            return Err(dt_err(
                "duration->nanos requires a Duration",
                args.call_span,
            ))
        }
    };

    Ok(Rc::new(Thunk::new_materialized(Value::Int(nanos), args.call_span)))
}

/// Load a timezone from a zoneinfo directory (via DirCap).
pub fn builtin_load_tz(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [dir_thunk, name_thunk] = args.args else {
        return Err(dt_err(
            "load-tz requires 2 arguments",
            args.call_span,
        ));
    };

    let dir_val = materialize(dir_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let name_val = materialize(name_thunk, Some(&args.call_span), &args.ctx, args.depth)?;

    let dir = match &dir_val {
        Value::DirCap(d) => d,
        _ => {
            return Err(dt_err(
                "load-tz requires DirCap as first argument",
                args.call_span,
            ))
        }
    };

    let name = name_val
        .as_str()
        .ok_or_else(|| dt_err("load-tz requires String as second argument", args.call_span))?;

    // Read the timezone file from the zoneinfo directory
    let file = dir.open(name).map_err(|e| {
        dt_err(
            format!("failed to open timezone file {name}: {e}"),
            args.call_span,
        )
    })?;

    // Read the file contents
    use std::io::Read;
    let mut buf = Vec::new();
    let mut reader = std::io::BufReader::new(file);
    reader.read_to_end(&mut buf).map_err(|e| {
        dt_err(
            format!("failed to read timezone file {name}: {e}"),
            args.call_span,
        )
    })?;

    // Parse the TZif binary format
    let tz = jiff::tz::TimeZone::tzif(name, &buf).map_err(|e| {
        dt_err(
            format!("failed to parse timezone file {name}: {e}"),
            args.call_span,
        )
    })?;

    Ok(Rc::new(Thunk::new_materialized(
        Value::Timezone(Rc::new(tz)),
        args.call_span,
    )))
}

/// Convert a timestamp to local time in a timezone.
pub fn builtin_timestamp_in_tz(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [t_thunk, tz_thunk] = args.args else {
        return Err(dt_err(
            "timestamp-in-tz requires 2 arguments",
            args.call_span,
        ));
    };

    let t_val = materialize(t_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let tz_val = materialize(tz_thunk, Some(&args.call_span), &args.ctx, args.depth)?;

    let nanos = match &t_val {
        Value::Timestamp(n) => *n,
        _ => {
            return Err(dt_err(
                "timestamp-in-tz requires Timestamp as first argument",
                args.call_span,
            ))
        }
    };

    let tz = match &tz_val {
        Value::Timezone(tz) => tz,
        _ => {
            return Err(dt_err(
                "timestamp-in-tz requires Timezone as second argument",
                args.call_span,
            ))
        }
    };

    let ts = jiff::Timestamp::from_nanosecond(nanos as i128).map_err(|e| {
        dt_err(
            format!("invalid timestamp value: {e}"),
            args.call_span,
        )
    })?;

    let dt = ts.to_zoned((**tz).clone());

    let mut map = IndexMap::new();
    map.insert(
        Key::String("year".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(dt.year() as i64),
            args.call_span,
        ))),
    );
    map.insert(
        Key::String("month".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(dt.month() as i64),
            args.call_span,
        ))),
    );
    map.insert(
        Key::String("day".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(dt.day() as i64),
            args.call_span,
        ))),
    );
    map.insert(
        Key::String("hour".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(dt.hour() as i64),
            args.call_span,
        ))),
    );
    map.insert(
        Key::String("minute".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(dt.minute() as i64),
            args.call_span,
        ))),
    );
    map.insert(
        Key::String("second".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(dt.second() as i64),
            args.call_span,
        ))),
    );
    map.insert(
        Key::String("offset-seconds".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(dt.offset().seconds() as i64),
            args.call_span,
        ))),
    );
    map.insert(
        Key::String("tz-name".to_string()),
        args.ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            string_val(dt.time_zone().iana_name().unwrap_or("Unknown")),
            args.call_span,
        ))),
    );

    Ok(Rc::new(Thunk::new_materialized(Value::Dict(map), args.call_span)))
}

/// Convert local time components to a UTC timestamp in a timezone.
pub fn builtin_local_to_timestamp(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    if args.args.len() != 7 {
        return Err(dt_err(
            "local->timestamp requires 7 arguments (year month day hour minute second timezone)",
            args.call_span,
        ));
    }

    let year_val = materialize(&args.args[0], Some(&args.call_span), &args.ctx, args.depth)?;
    let month_val = materialize(&args.args[1], Some(&args.call_span), &args.ctx, args.depth)?;
    let day_val = materialize(&args.args[2], Some(&args.call_span), &args.ctx, args.depth)?;
    let hour_val = materialize(&args.args[3], Some(&args.call_span), &args.ctx, args.depth)?;
    let minute_val = materialize(&args.args[4], Some(&args.call_span), &args.ctx, args.depth)?;
    let second_val = materialize(&args.args[5], Some(&args.call_span), &args.ctx, args.depth)?;
    let tz_val = materialize(&args.args[6], Some(&args.call_span), &args.ctx, args.depth)?;

    let year = match &year_val {
        Value::Int(n) => *n as i16,
        _ => return Err(dt_err("local->timestamp year must be Int", args.call_span)),
    };
    let month = match &month_val {
        Value::Int(n) => *n as i8,
        _ => return Err(dt_err("local->timestamp month must be Int", args.call_span)),
    };
    let day = match &day_val {
        Value::Int(n) => *n as i8,
        _ => return Err(dt_err("local->timestamp day must be Int", args.call_span)),
    };
    let hour = match &hour_val {
        Value::Int(n) => *n as i8,
        _ => return Err(dt_err("local->timestamp hour must be Int", args.call_span)),
    };
    let minute = match &minute_val {
        Value::Int(n) => *n as i8,
        _ => return Err(dt_err("local->timestamp minute must be Int", args.call_span)),
    };
    let second = match &second_val {
        Value::Int(n) => *n as i8,
        _ => return Err(dt_err("local->timestamp second must be Int", args.call_span)),
    };
    let tz = match &tz_val {
        Value::Timezone(tz) => tz,
        _ => return Err(dt_err("local->timestamp timezone must be Timezone", args.call_span)),
    };

    // Build a datetime in the given timezone
    let dt = jiff::civil::DateTime::new(year, month, day, hour, minute, second, 0)
        .map_err(|e| dt_err(format!("invalid datetime components: {e}"), args.call_span))?;

    let zoned = dt.to_zoned((**tz).clone())
        .map_err(|e| dt_err(format!("failed to convert to zoned datetime: {e}"), args.call_span))?;

    let ts = zoned.timestamp();
    let nanos = i64::try_from(ts.as_nanosecond()).unwrap_or(i64::MAX);

    Ok(Rc::new(Thunk::new_materialized(
        Value::Timestamp(nanos),
        args.call_span,
    )))
}

/// Get the local timezone name from the system.
pub fn builtin_local_tz_name(args: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let [dir_thunk] = args.args else {
        return Err(dt_err(
            "local-tz-name requires 1 argument (DirCap)",
            args.call_span,
        ));
    };

    let dir_val = materialize(dir_thunk, Some(&args.call_span), &args.ctx, args.depth)?;
    let _dir = match &dir_val {
        Value::DirCap(d) => d,
        _ => {
            return Err(dt_err(
                "local-tz-name requires DirCap",
                args.call_span,
            ))
        }
    };

    // Try to get the system timezone name
    // This is a simplified implementation - in production we'd need to:
    // 1. Read /etc/localtime symlink on Unix
    // 2. Parse TZ environment variable
    // 3. Have platform-specific fallbacks
    // For now, return UTC as a safe default
    Ok(Rc::new(Thunk::new_materialized(
        string_val("UTC"),
        args.call_span,
    )))
}
