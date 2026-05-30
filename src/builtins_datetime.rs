//! Date-time builtins: timestamps, durations, clock capabilities, and timezones.
//!
//! Design: Timestamp and Duration are i64 nanoseconds (UTC epoch and signed span).
//! ClockCap provides injectable time access (real or fixed for testing).
//! Timezone reads system zoneinfo via DirCap for timezone conversions.
//!
//! See doc/whatif/lib-datetime.md for the full specification.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::builtin;
use crate::error::{EvalError, EvalResult};
use crate::types::{Row, Type, TypeEnv};
use crate::value::{
    string_val, BuiltinArgs, BuiltinDef, ClockCapInner, Key, Strictness, Thunk, Value,
};

/// Helper to create a boxed EvalError from a message.
fn dt_err(msg: impl Into<String>, span: Span) -> Box<EvalError> {
    EvalError::internal(msg.into(), span).into()
}

/// Parse an RFC 3339 timestamp string to a Timestamp (i64 nanoseconds).
/// Errors if the format is invalid or the timestamp overflows i64 nanoseconds.
pub fn builtin_parse_timestamp(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [s_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "parse-timestamp requires 1 argument",
                call_span.clone(),
            ));
        };

        let s_val = s_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let s = s_val
            .as_str()
            .ok_or_else(|| dt_err("parse-timestamp requires a String", call_span.clone()))?;

        // Parse RFC 3339 string using jiff
        let ts = jiff::Timestamp::from_str(s).map_err(|e| {
            dt_err(
                format!("invalid RFC 3339 timestamp: {e}"),
                call_span.clone(),
            )
        })?;

        // Convert to nanoseconds since epoch
        let nanos = i64::try_from(ts.as_nanosecond()).unwrap_or(i64::MAX);

        Ok(Arc::new(Thunk::new_materialized(
            Value::Timestamp(nanos),
            call_span,
        )))
    })
}

/// Format a Timestamp as an RFC 3339 string.
pub fn builtin_format_timestamp(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "format-timestamp requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let nanos = match &t_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "format-timestamp requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        // Convert nanoseconds to jiff::Timestamp
        let ts = jiff::Timestamp::from_nanosecond(nanos as i128)
            .map_err(|e| dt_err(format!("invalid timestamp value: {e}"), call_span.clone()))?;

        // Format as RFC 3339 string
        let s = ts.to_string();

        Ok(Arc::new(Thunk::new_materialized(string_val(&s), call_span)))
    })
}

/// Convert a Timestamp to Unix seconds (truncating nanoseconds).
pub fn builtin_timestamp_to_unix(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp->unix requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let nanos = match &t_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp->unix requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let seconds = nanos / 1_000_000_000;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Int(seconds),
            call_span,
        )))
    })
}

/// Convert Unix seconds to a Timestamp.
pub fn builtin_unix_to_timestamp(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [n_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "unix->timestamp requires 1 argument",
                call_span.clone(),
            ));
        };

        let n_val = n_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let seconds = match &n_val {
            Value::Int(n) => *n,
            _ => return Err(dt_err("unix->timestamp requires an Int", call_span.clone())),
        };

        let nanos = seconds
            .checked_mul(1_000_000_000)
            .ok_or_else(|| dt_err("unix->timestamp overflow", call_span.clone()))?;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Timestamp(nanos),
            call_span,
        )))
    })
}

/// Read the current time from a ClockCap.
pub fn builtin_now(args: BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [cap_thunk] = args.args.as_slice() else {
            return Err(dt_err("now requires 1 argument", call_span.clone()));
        };

        let cap_val = cap_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let clock_cap = match &cap_val {
            Value::ClockCap(inner) => inner,
            _ => return Err(dt_err("now requires a ClockCap", call_span.clone())),
        };

        let nanos = match clock_cap.as_ref() {
            ClockCapInner::Real => {
                // Read the real system clock
                let ts = jiff::Timestamp::now();
                i64::try_from(ts.as_nanosecond()).unwrap_or(i64::MAX)
            }
            ClockCapInner::Fixed(nanos) => *nanos,
        };

        Ok(Arc::new(Thunk::new_materialized(
            Value::Timestamp(nanos),
            call_span,
        )))
    })
}

/// Create a fixed ClockCap that always returns the given timestamp.
pub fn builtin_fixed_clock(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err("fixed-clock requires 1 argument", call_span.clone()));
        };

        let t_val = t_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let nanos = match &t_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "fixed-clock requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        Ok(Arc::new(Thunk::new_materialized(
            Value::ClockCap(Rc::new(ClockCapInner::Fixed(nanos))),
            call_span,
        )))
    })
}

/// Add a duration to a timestamp.
pub fn builtin_timestamp_add(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk, d_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-add requires 2 arguments",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");
        let d_val = d_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");

        let t_nanos = match &t_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp-add requires Timestamp as first argument",
                    call_span.clone(),
                ))
            }
        };

        let d_nanos = match &d_val {
            Value::Duration(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp-add requires Duration as second argument",
                    call_span.clone(),
                ))
            }
        };

        let result = t_nanos
            .checked_add(d_nanos)
            .ok_or_else(|| dt_err("timestamp-add overflow", call_span.clone()))?;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Timestamp(result),
            call_span,
        )))
    })
}

/// Compute the duration between two timestamps (t1 - t2).
pub fn builtin_timestamp_diff(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t1_thunk, t2_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-diff requires 2 arguments",
                call_span.clone(),
            ));
        };

        let t1_val = t1_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");
        let t2_val = t2_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");

        let t1_nanos = match &t1_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp-diff requires Timestamp as first argument",
                    call_span.clone(),
                ))
            }
        };

        let t2_nanos = match &t2_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp-diff requires Timestamp as second argument",
                    call_span.clone(),
                ))
            }
        };

        let result = t1_nanos
            .checked_sub(t2_nanos)
            .ok_or_else(|| dt_err("timestamp-diff overflow", call_span.clone()))?;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Duration(result),
            call_span,
        )))
    })
}

/// Compare two timestamps: t1 < t2
pub fn builtin_timestamp_lt(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t1_thunk, t2_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp<? requires 2 arguments",
                call_span.clone(),
            ));
        };

        let t1_val = t1_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");
        let t2_val = t2_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");

        let t1_nanos = match &t1_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp<? requires Timestamp as first argument",
                    call_span.clone(),
                ))
            }
        };

        let t2_nanos = match &t2_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp<? requires Timestamp as second argument",
                    call_span.clone(),
                ))
            }
        };

        Ok(Arc::new(Thunk::new_materialized(
            Value::Bool(t1_nanos < t2_nanos),
            call_span,
        )))
    })
}

/// Compare two timestamps: t1 > t2
pub fn builtin_timestamp_gt(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t1_thunk, t2_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp>? requires 2 arguments",
                call_span.clone(),
            ));
        };

        let t1_val = t1_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");
        let t2_val = t2_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");

        let t1_nanos = match &t1_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp>? requires Timestamp as first argument",
                    call_span.clone(),
                ))
            }
        };

        let t2_nanos = match &t2_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp>? requires Timestamp as second argument",
                    call_span.clone(),
                ))
            }
        };

        Ok(Arc::new(Thunk::new_materialized(
            Value::Bool(t1_nanos > t2_nanos),
            call_span,
        )))
    })
}

/// Compare two timestamps for equality: t1 == t2
pub fn builtin_timestamp_eq(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t1_thunk, t2_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp=? requires 2 arguments",
                call_span.clone(),
            ));
        };

        let t1_val = t1_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");
        let t2_val = t2_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");

        let t1_nanos = match &t1_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp=? requires Timestamp as first argument",
                    call_span.clone(),
                ))
            }
        };

        let t2_nanos = match &t2_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp=? requires Timestamp as second argument",
                    call_span.clone(),
                ))
            }
        };

        Ok(Arc::new(Thunk::new_materialized(
            Value::Bool(t1_nanos == t2_nanos),
            call_span,
        )))
    })
}

/// Extract the UTC year from a timestamp.
pub fn builtin_timestamp_year(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-year requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let nanos = match &t_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp-year requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let ts = jiff::Timestamp::from_nanosecond(nanos as i128)
            .map_err(|e| dt_err(format!("invalid timestamp value: {e}"), call_span.clone()))?;

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
        let year = dt.year() as i64;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Int(year),
            call_span,
        )))
    })
}

/// Extract the UTC month (1-12) from a timestamp.
pub fn builtin_timestamp_month(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-month requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let nanos = match &t_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp-month requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let ts = jiff::Timestamp::from_nanosecond(nanos as i128)
            .map_err(|e| dt_err(format!("invalid timestamp value: {e}"), call_span.clone()))?;

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
        let month = dt.month() as i64;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Int(month),
            call_span,
        )))
    })
}

/// Extract the UTC day (1-31) from a timestamp.
pub fn builtin_timestamp_day(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-day requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let nanos = match &t_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp-day requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let ts = jiff::Timestamp::from_nanosecond(nanos as i128)
            .map_err(|e| dt_err(format!("invalid timestamp value: {e}"), call_span.clone()))?;

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
        let day = dt.day() as i64;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Int(day),
            call_span,
        )))
    })
}

/// Extract the UTC hour (0-23) from a timestamp.
pub fn builtin_timestamp_hour(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-hour requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let nanos = match &t_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp-hour requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let ts = jiff::Timestamp::from_nanosecond(nanos as i128)
            .map_err(|e| dt_err(format!("invalid timestamp value: {e}"), call_span.clone()))?;

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
        let hour = dt.hour() as i64;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Int(hour),
            call_span,
        )))
    })
}

/// Extract the UTC minute (0-59) from a timestamp.
pub fn builtin_timestamp_minute(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-minute requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let nanos = match &t_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp-minute requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let ts = jiff::Timestamp::from_nanosecond(nanos as i128)
            .map_err(|e| dt_err(format!("invalid timestamp value: {e}"), call_span.clone()))?;

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
        let minute = dt.minute() as i64;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Int(minute),
            call_span,
        )))
    })
}

/// Extract the UTC second (0-59) from a timestamp.
pub fn builtin_timestamp_second(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-second requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let nanos = match &t_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp-second requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let ts = jiff::Timestamp::from_nanosecond(nanos as i128)
            .map_err(|e| dt_err(format!("invalid timestamp value: {e}"), call_span.clone()))?;

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
        let second = dt.second() as i64;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Int(second),
            call_span,
        )))
    })
}

/// Extract all UTC components from a timestamp as a dict.
pub fn builtin_timestamp_parts(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-parts requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let nanos = match &t_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp-parts requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let ts = jiff::Timestamp::from_nanosecond(nanos as i128)
            .map_err(|e| dt_err(format!("invalid timestamp value: {e}"), call_span.clone()))?;

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);

        let mut map = IndexMap::new();
        map.insert(
            Key::String("year".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(dt.year() as i64),
                call_span.clone(),
            ))),
        );
        map.insert(
            Key::String("month".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(dt.month() as i64),
                call_span.clone(),
            ))),
        );
        map.insert(
            Key::String("day".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(dt.day() as i64),
                call_span.clone(),
            ))),
        );
        map.insert(
            Key::String("hour".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(dt.hour() as i64),
                call_span.clone(),
            ))),
        );
        map.insert(
            Key::String("minute".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(dt.minute() as i64),
                call_span.clone(),
            ))),
        );
        map.insert(
            Key::String("second".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(dt.second() as i64),
                call_span.clone(),
            ))),
        );

        Ok(Arc::new(Thunk::new_materialized(
            Value::Dict(map),
            call_span,
        )))
    })
}

/// Create a duration from nanoseconds.
pub fn builtin_duration_nanos(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [n_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration-nanos requires 1 argument",
                call_span.clone(),
            ));
        };

        let n_val = n_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let nanos = match &n_val {
            Value::Int(n) => *n,
            _ => return Err(dt_err("duration-nanos requires an Int", call_span.clone())),
        };

        Ok(Arc::new(Thunk::new_materialized(
            Value::Duration(nanos),
            call_span,
        )))
    })
}

/// Create a duration from seconds.
pub fn builtin_duration_seconds(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [n_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration-seconds requires 1 argument",
                call_span.clone(),
            ));
        };

        let n_val = n_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let seconds = match &n_val {
            Value::Int(n) => *n,
            _ => {
                return Err(dt_err(
                    "duration-seconds requires an Int",
                    call_span.clone(),
                ))
            }
        };

        let nanos = seconds
            .checked_mul(1_000_000_000)
            .ok_or_else(|| dt_err("duration-seconds overflow", call_span.clone()))?;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Duration(nanos),
            call_span,
        )))
    })
}

/// Create a duration from minutes.
pub fn builtin_duration_minutes(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [n_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration-minutes requires 1 argument",
                call_span.clone(),
            ));
        };

        let n_val = n_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let minutes = match &n_val {
            Value::Int(n) => *n,
            _ => {
                return Err(dt_err(
                    "duration-minutes requires an Int",
                    call_span.clone(),
                ))
            }
        };

        let nanos = minutes
            .checked_mul(60)
            .and_then(|s| s.checked_mul(1_000_000_000))
            .ok_or_else(|| dt_err("duration-minutes overflow", call_span.clone()))?;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Duration(nanos),
            call_span,
        )))
    })
}

/// Create a duration from hours.
pub fn builtin_duration_hours(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [n_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration-hours requires 1 argument",
                call_span.clone(),
            ));
        };

        let n_val = n_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let hours = match &n_val {
            Value::Int(n) => *n,
            _ => return Err(dt_err("duration-hours requires an Int", call_span.clone())),
        };

        let nanos = hours
            .checked_mul(3600)
            .and_then(|s| s.checked_mul(1_000_000_000))
            .ok_or_else(|| dt_err("duration-hours overflow", call_span.clone()))?;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Duration(nanos),
            call_span,
        )))
    })
}

/// Create a duration from days.
pub fn builtin_duration_days(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [n_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration-days requires 1 argument",
                call_span.clone(),
            ));
        };

        let n_val = n_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let days = match &n_val {
            Value::Int(n) => *n,
            _ => return Err(dt_err("duration-days requires an Int", call_span.clone())),
        };

        let nanos = days
            .checked_mul(86400)
            .and_then(|s| s.checked_mul(1_000_000_000))
            .ok_or_else(|| dt_err("duration-days overflow", call_span.clone()))?;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Duration(nanos),
            call_span,
        )))
    })
}

/// Convert a duration to seconds (truncating nanoseconds).
pub fn builtin_duration_to_seconds(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [d_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration->seconds requires 1 argument",
                call_span.clone(),
            ));
        };

        let d_val = d_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let nanos = match &d_val {
            Value::Duration(n) => *n,
            _ => {
                return Err(dt_err(
                    "duration->seconds requires a Duration",
                    call_span.clone(),
                ))
            }
        };

        let seconds = nanos / 1_000_000_000;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Int(seconds),
            call_span,
        )))
    })
}

/// Convert a duration to nanoseconds.
pub fn builtin_duration_to_nanos(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [d_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration->nanos requires 1 argument",
                call_span.clone(),
            ));
        };

        let d_val = d_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let nanos = match &d_val {
            Value::Duration(n) => *n,
            _ => {
                return Err(dt_err(
                    "duration->nanos requires a Duration",
                    call_span.clone(),
                ))
            }
        };

        Ok(Arc::new(Thunk::new_materialized(
            Value::Int(nanos),
            call_span,
        )))
    })
}

/// Load a timezone from a zoneinfo directory (via DirCap).
pub fn builtin_load_tz(args: BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [dir_thunk, name_thunk] = args.args.as_slice() else {
            return Err(dt_err("load-tz requires 2 arguments", call_span.clone()));
        };

        let dir_val = dir_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");
        let name_val = name_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");

        let dir = match &dir_val {
            Value::DirCap { dir, .. } => dir,
            _ => {
                return Err(dt_err(
                    "load-tz requires DirCap as first argument",
                    call_span.clone(),
                ))
            }
        };

        let name = name_val.as_str().ok_or_else(|| {
            dt_err(
                "load-tz requires String as second argument",
                call_span.clone(),
            )
        })?;

        // Read the timezone file from the zoneinfo directory
        let file = dir.open(name).map_err(|e| {
            dt_err(
                format!("failed to open timezone file {name}: {e}"),
                call_span.clone(),
            )
        })?;

        // Read the file contents
        use std::io::Read;
        let mut buf = Vec::new();
        let mut reader = std::io::BufReader::new(file);
        reader.read_to_end(&mut buf).map_err(|e| {
            dt_err(
                format!("failed to read timezone file {name}: {e}"),
                call_span.clone(),
            )
        })?;

        // Parse the TZif binary format
        let tz = jiff::tz::TimeZone::tzif(name, &buf).map_err(|e| {
            dt_err(
                format!("failed to parse timezone file {name}: {e}"),
                call_span.clone(),
            )
        })?;

        Ok(Arc::new(Thunk::new_materialized(
            Value::Timezone(Rc::new(tz)),
            call_span,
        )))
    })
}

/// Convert a timestamp to local time in a timezone.
pub fn builtin_timestamp_in_tz(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk, tz_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-in-tz requires 2 arguments",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");
        let tz_val = tz_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");

        let nanos = match &t_val {
            Value::Timestamp(n) => *n,
            _ => {
                return Err(dt_err(
                    "timestamp-in-tz requires Timestamp as first argument",
                    call_span.clone(),
                ))
            }
        };

        let tz = match &tz_val {
            Value::Timezone(tz) => tz,
            _ => {
                return Err(dt_err(
                    "timestamp-in-tz requires Timezone as second argument",
                    call_span.clone(),
                ))
            }
        };

        let ts = jiff::Timestamp::from_nanosecond(nanos as i128)
            .map_err(|e| dt_err(format!("invalid timestamp value: {e}"), call_span.clone()))?;

        let dt = ts.to_zoned((**tz).clone());

        let mut map = IndexMap::new();
        map.insert(
            Key::String("year".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(dt.year() as i64),
                call_span.clone(),
            ))),
        );
        map.insert(
            Key::String("month".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(dt.month() as i64),
                call_span.clone(),
            ))),
        );
        map.insert(
            Key::String("day".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(dt.day() as i64),
                call_span.clone(),
            ))),
        );
        map.insert(
            Key::String("hour".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(dt.hour() as i64),
                call_span.clone(),
            ))),
        );
        map.insert(
            Key::String("minute".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(dt.minute() as i64),
                call_span.clone(),
            ))),
        );
        map.insert(
            Key::String("second".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(dt.second() as i64),
                call_span.clone(),
            ))),
        );
        map.insert(
            Key::String("offset-seconds".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(dt.offset().seconds() as i64),
                call_span.clone(),
            ))),
        );
        map.insert(
            Key::String("tz-name".into()),
            args.ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val(dt.time_zone().iana_name().unwrap_or("Unknown")),
                call_span.clone(),
            ))),
        );

        Ok(Arc::new(Thunk::new_materialized(
            Value::Dict(map),
            call_span,
        )))
    })
}

/// Convert local time components to a UTC timestamp in a timezone.
pub fn builtin_local_to_timestamp(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        if args.args.len() != 7 {
            return Err(dt_err(
                "local->timestamp requires 7 arguments (year month day hour minute second timezone)",
                call_span.clone(),
            ));
        }

        let year_val = args.args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let month_val = args.args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let day_val = args.args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let hour_val = args.args[3]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let minute_val = args.args[4]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let second_val = args.args[5]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let tz_val = args.args[6]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let year = match &year_val {
            Value::Int(n) => *n as i16,
            _ => {
                return Err(dt_err(
                    "local->timestamp year must be Int",
                    call_span.clone(),
                ))
            }
        };
        let month = match &month_val {
            Value::Int(n) => *n as i8,
            _ => {
                return Err(dt_err(
                    "local->timestamp month must be Int",
                    call_span.clone(),
                ))
            }
        };
        let day = match &day_val {
            Value::Int(n) => *n as i8,
            _ => {
                return Err(dt_err(
                    "local->timestamp day must be Int",
                    call_span.clone(),
                ))
            }
        };
        let hour = match &hour_val {
            Value::Int(n) => *n as i8,
            _ => {
                return Err(dt_err(
                    "local->timestamp hour must be Int",
                    call_span.clone(),
                ))
            }
        };
        let minute = match &minute_val {
            Value::Int(n) => *n as i8,
            _ => {
                return Err(dt_err(
                    "local->timestamp minute must be Int",
                    call_span.clone(),
                ))
            }
        };
        let second = match &second_val {
            Value::Int(n) => *n as i8,
            _ => {
                return Err(dt_err(
                    "local->timestamp second must be Int",
                    call_span.clone(),
                ))
            }
        };
        let tz = match &tz_val {
            Value::Timezone(tz) => tz,
            _ => {
                return Err(dt_err(
                    "local->timestamp timezone must be Timezone",
                    call_span.clone(),
                ))
            }
        };

        // Build a datetime in the given timezone
        let dt =
            jiff::civil::DateTime::new(year, month, day, hour, minute, second, 0).map_err(|e| {
                dt_err(
                    format!("invalid datetime components: {e}"),
                    call_span.clone(),
                )
            })?;

        let zoned = dt.to_zoned((**tz).clone()).map_err(|e| {
            dt_err(
                format!("failed to convert to zoned datetime: {e}"),
                call_span.clone(),
            )
        })?;

        let ts = zoned.timestamp();
        let nanos = i64::try_from(ts.as_nanosecond()).unwrap_or(i64::MAX);

        Ok(Arc::new(Thunk::new_materialized(
            Value::Timestamp(nanos),
            call_span,
        )))
    })
}

/// Get the local timezone name from the system.
pub fn builtin_local_tz_name(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [dir_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "local-tz-name requires 1 argument (DirCap)",
                call_span.clone(),
            ));
        };

        let dir_val = dir_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");
        let _dir = match &dir_val {
            Value::DirCap { dir, .. } => dir,
            _ => return Err(dt_err("local-tz-name requires DirCap", call_span.clone())),
        };

        // Try to get the system timezone name
        // This is a simplified implementation - in production we'd need to:
        // 1. Read /etc/localtime symlink on Unix
        // 2. Parse TZ environment variable
        // 3. Have platform-specific fallbacks
        // For now, return UTC as a safe default
        Ok(Arc::new(Thunk::new_materialized(
            string_val("UTC"),
            call_span,
        )))
    })
}

/// Return all datetime `BuiltinDef` entries for the `"datetime"` module.
///
/// Called by `builtin_module("datetime")` in `src/builtins.rs` (wired in T-718).
pub fn datetime_builtins() -> Vec<BuiltinDef> {
    vec![
        builtin!(
            "parse-timestamp",
            builtin_parse_timestamp,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "format-timestamp",
            builtin_format_timestamp,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "timestamp->unix",
            builtin_timestamp_to_unix,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "unix->timestamp",
            builtin_unix_to_timestamp,
            [Strictness::Seq],
            1
        ),
        builtin!("now", builtin_now, [Strictness::Seq], 1),
        builtin!("fixed-clock", builtin_fixed_clock, [Strictness::Seq], 1),
        builtin!(
            "timestamp-add",
            builtin_timestamp_add,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "timestamp-diff",
            builtin_timestamp_diff,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "timestamp<?",
            builtin_timestamp_lt,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "timestamp>?",
            builtin_timestamp_gt,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "timestamp=?",
            builtin_timestamp_eq,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "timestamp-year",
            builtin_timestamp_year,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "timestamp-month",
            builtin_timestamp_month,
            [Strictness::Seq],
            1
        ),
        builtin!("timestamp-day", builtin_timestamp_day, [Strictness::Seq], 1),
        builtin!(
            "timestamp-hour",
            builtin_timestamp_hour,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "timestamp-minute",
            builtin_timestamp_minute,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "timestamp-second",
            builtin_timestamp_second,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "timestamp-parts",
            builtin_timestamp_parts,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "duration-nanos",
            builtin_duration_nanos,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "duration-seconds",
            builtin_duration_seconds,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "duration-minutes",
            builtin_duration_minutes,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "duration-hours",
            builtin_duration_hours,
            [Strictness::Seq],
            1
        ),
        builtin!("duration-days", builtin_duration_days, [Strictness::Seq], 1),
        builtin!(
            "duration->seconds",
            builtin_duration_to_seconds,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "duration->nanos",
            builtin_duration_to_nanos,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "load-tz",
            builtin_load_tz,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "timestamp-in-tz",
            builtin_timestamp_in_tz,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!("local->timestamp", builtin_local_to_timestamp, [], 7),
        builtin!("local-tz-name", builtin_local_tz_name, [Strictness::Seq], 1),
    ]
}

/// Inject all datetime type signatures into `env`.
///
/// Provides the datetime section of the former `TypeEnv::with_builtins()` (deleted in T-722).
/// Called by `type_env_module("datetime")` in `src/builtins.rs` and (via delegation)
/// by `build_builtins_type_env()`. Add new datetime builtins here.
pub fn datetime_type_env(env: &mut TypeEnv) {
    // parse-timestamp: String → Timestamp
    env.insert(
        "parse-timestamp".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Timestamp),
            variadic: false,
        },
    );
    // format-timestamp: Timestamp → String
    env.insert(
        "format-timestamp".to_string(),
        Type::Function {
            params: vec![(None, Type::Timestamp)],
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
    // timestamp->unix: Timestamp → Int
    env.insert(
        "timestamp->unix".to_string(),
        Type::Function {
            params: vec![(None, Type::Timestamp)],
            ret: Box::new(Type::Int),
            variadic: false,
        },
    );
    // unix->timestamp: Int → Timestamp
    env.insert(
        "unix->timestamp".to_string(),
        Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Timestamp),
            variadic: false,
        },
    );
    // now: ClockCap → Timestamp
    env.insert(
        "now".to_string(),
        Type::Function {
            params: vec![(None, Type::ClockCap)],
            ret: Box::new(Type::Timestamp),
            variadic: false,
        },
    );
    // fixed-clock: Timestamp → ClockCap
    env.insert(
        "fixed-clock".to_string(),
        Type::Function {
            params: vec![(None, Type::Timestamp)],
            ret: Box::new(Type::ClockCap),
            variadic: false,
        },
    );
    // timestamp-add: Timestamp → Duration → Timestamp
    env.insert(
        "timestamp-add".to_string(),
        Type::Function {
            params: vec![(None, Type::Timestamp), (None, Type::Duration)],
            ret: Box::new(Type::Timestamp),
            variadic: false,
        },
    );
    // timestamp-diff: Timestamp → Timestamp → Duration
    env.insert(
        "timestamp-diff".to_string(),
        Type::Function {
            params: vec![(None, Type::Timestamp), (None, Type::Timestamp)],
            ret: Box::new(Type::Duration),
            variadic: false,
        },
    );
    // timestamp<?, timestamp>?, timestamp=?: Timestamp → Timestamp → Bool
    for name in ["timestamp<?", "timestamp>?", "timestamp=?"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Timestamp), (None, Type::Timestamp)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
    }
    // timestamp-year, -month, -day, -hour, -minute, -second: Timestamp → Int
    for name in [
        "timestamp-year",
        "timestamp-month",
        "timestamp-day",
        "timestamp-hour",
        "timestamp-minute",
        "timestamp-second",
    ] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Timestamp)],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        );
    }
    // timestamp-parts: Timestamp → {year: Int, month: Int, day: Int, hour: Int, minute: Int, second: Int}
    env.insert(
        "timestamp-parts".to_string(),
        Type::Function {
            params: vec![(None, Type::Timestamp)],
            ret: Box::new(Type::Record(Row {
                fields: HashMap::from([
                    ("year".to_string(), Type::Int),
                    ("month".to_string(), Type::Int),
                    ("day".to_string(), Type::Int),
                    ("hour".to_string(), Type::Int),
                    ("minute".to_string(), Type::Int),
                    ("second".to_string(), Type::Int),
                ]),
            })),
            variadic: false,
        },
    );
    // duration-nanos, -seconds, -minutes, -hours, -days: Int → Duration
    for name in [
        "duration-nanos",
        "duration-seconds",
        "duration-minutes",
        "duration-hours",
        "duration-days",
    ] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Int)],
                ret: Box::new(Type::Duration),
                variadic: false,
            },
        );
    }
    // duration->seconds, duration->nanos: Duration → Int
    for name in ["duration->seconds", "duration->nanos"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Duration)],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        );
    }
    // load-tz: DirCap → String → Timezone
    env.insert(
        "load-tz".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::Timezone),
            variadic: false,
        },
    );
    // timestamp-in-tz: Timestamp → Timezone → {year, month, day, hour, minute, second, offset-seconds, tz-name}
    env.insert(
        "timestamp-in-tz".to_string(),
        Type::Function {
            params: vec![(None, Type::Timestamp), (None, Type::Timezone)],
            ret: Box::new(Type::Record(Row {
                fields: HashMap::from([
                    ("year".to_string(), Type::Int),
                    ("month".to_string(), Type::Int),
                    ("day".to_string(), Type::Int),
                    ("hour".to_string(), Type::Int),
                    ("minute".to_string(), Type::Int),
                    ("second".to_string(), Type::Int),
                    ("offset-seconds".to_string(), Type::Int),
                    ("tz-name".to_string(), Type::Str),
                ]),
            })),
            variadic: false,
        },
    );
    // local->timestamp: Int Int Int Int Int Int Timezone → Timestamp
    env.insert(
        "local->timestamp".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Int),
                (None, Type::Int),
                (None, Type::Int),
                (None, Type::Int),
                (None, Type::Int),
                (None, Type::Int),
                (None, Type::Timezone),
            ],
            ret: Box::new(Type::Timestamp),
            variadic: false,
        },
    );
    // local-tz-name: DirCap → String
    env.insert(
        "local-tz-name".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap)],
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
}
