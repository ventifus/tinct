//! Date-time builtins: timestamps, durations, clock capabilities, and timezones.
//!
//! Design: Timestamp stores a pre-validated jiff::Timestamp (UTC). Duration is i64 nanoseconds (signed span).
//! ClockCap provides injectable time access (real or fixed for testing).
//! Timezone reads system zoneinfo via DirCap for timezone conversions.
//!
//! See doc/whatif/lib-datetime.md for the full specification.

use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::builtin;
use crate::error::{EvalError, EvalResult};
use crate::value::{
    string_val, BuiltinArgs, BuiltinDef, ClockCapInner, HashableValue, Strictness, Thunk, Value,
};

/// Helper to create a boxed EvalError from a message.
fn dt_err(msg: impl Into<String>, span: Span) -> Box<EvalError> {
    EvalError::internal(msg.into(), span).into()
}

/// Parse an RFC 3339 timestamp string to a Timestamp (i64 nanoseconds).
/// Errors if the format is invalid or the timestamp overflows i64 nanoseconds.
pub fn builtin_parse_timestamp(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [s_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "parse-timestamp requires 1 argument",
                call_span.clone(),
            ));
        };

        let s_val = s_thunk.clone().require_value()?.clone();
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

        Ok(Arc::new(Thunk::value(
            Value::Timestamp {
                ts,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Format a Timestamp as an RFC 3339 string.
pub fn builtin_format_timestamp(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "format-timestamp requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk.clone().require_value()?.clone();
        let ts = match &t_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "format-timestamp requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        // Format as RFC 3339 string
        let s = ts.to_string();

        Ok(Arc::new(Thunk::value(string_val(&s), call_span)))
    })
}

/// Convert a Timestamp to Unix seconds (truncating nanoseconds).
pub fn builtin_timestamp_to_unix(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp->unix requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk.clone().require_value()?.clone();
        let ts = match &t_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp->unix requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let seconds = ts.as_nanosecond() as i64 / 1_000_000_000;

        Ok(Arc::new(Thunk::value(
            Value::Int {
                n: seconds,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Convert Unix seconds to a Timestamp.
pub fn builtin_unix_to_timestamp(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [n_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "unix->timestamp requires 1 argument",
                call_span.clone(),
            ));
        };

        let n_val = n_thunk.clone().require_value()?.clone();
        let seconds = match &n_val {
            Value::Int { n, .. } => *n,
            _ => return Err(dt_err("unix->timestamp requires an Int", call_span.clone())),
        };

        let nanos = seconds
            .checked_mul(1_000_000_000)
            .ok_or_else(|| dt_err("unix->timestamp overflow", call_span.clone()))?;

        let ts = jiff::Timestamp::from_nanosecond(nanos as i128).map_err(|e| {
            dt_err(
                format!("unix->timestamp: nanoseconds out of range: {e}"),
                call_span.clone(),
            )
        })?;

        Ok(Arc::new(Thunk::value(
            Value::Timestamp {
                ts,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Read the current time from a ClockCap.
pub fn builtin_now(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [cap_thunk] = args.args.as_slice() else {
            return Err(dt_err("now requires 1 argument", call_span.clone()));
        };

        let cap_val = cap_thunk.clone().require_value()?.clone();
        let clock_cap = match &cap_val {
            Value::ClockCap { inner, .. } => inner,
            _ => return Err(dt_err("now requires a ClockCap", call_span.clone())),
        };

        let ts = match clock_cap.as_ref() {
            ClockCapInner::Real => {
                // Read the real system clock
                jiff::Timestamp::now()
            }
            ClockCapInner::Fixed(nanos) => {
                // All i64 nanosecond values are within jiff::Timestamp's representable range.
                jiff::Timestamp::from_nanosecond(*nanos as i128)
                    .expect("ClockCapInner::Fixed nanos are always a valid i64 in jiff range")
            }
        };

        Ok(Arc::new(Thunk::value(
            Value::Timestamp {
                ts,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Create a fixed ClockCap that always returns the given timestamp.
pub fn builtin_fixed_clock(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err("fixed-clock requires 1 argument", call_span.clone()));
        };

        let t_val = t_thunk.clone().require_value()?.clone();
        let ts = match &t_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "fixed-clock requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let nanos = ts.as_nanosecond() as i64;

        Ok(Arc::new(Thunk::value(
            Value::ClockCap {
                inner: Arc::new(ClockCapInner::Fixed(nanos)),
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Add a duration to a timestamp.
pub fn builtin_timestamp_add(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk, d_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-add requires 2 arguments",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk.clone().require_value()?.clone();
        let d_val = d_thunk.clone().require_value()?.clone();

        let t_nanos = match &t_val {
            Value::Timestamp { ts, .. } => ts.as_nanosecond() as i64,
            _ => {
                return Err(dt_err(
                    "timestamp-add requires Timestamp as first argument",
                    call_span.clone(),
                ))
            }
        };

        let d_nanos = match &d_val {
            Value::Duration { nanos: n, .. } => *n,
            _ => {
                return Err(dt_err(
                    "timestamp-add requires Duration as second argument",
                    call_span.clone(),
                ))
            }
        };

        let result_nanos = t_nanos
            .checked_add(d_nanos)
            .ok_or_else(|| dt_err("timestamp-add overflow", call_span.clone()))?;

        let result_ts = jiff::Timestamp::from_nanosecond(result_nanos as i128).map_err(|e| {
            dt_err(
                format!("timestamp-add: result out of range: {e}"),
                call_span.clone(),
            )
        })?;

        Ok(Arc::new(Thunk::value(
            Value::Timestamp {
                ts: result_ts,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Compute the duration between two timestamps (t1 - t2).
pub fn builtin_timestamp_diff(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t1_thunk, t2_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-diff requires 2 arguments",
                call_span.clone(),
            ));
        };

        let t1_val = t1_thunk.clone().require_value()?.clone();
        let t2_val = t2_thunk.clone().require_value()?.clone();

        let t1_nanos = match &t1_val {
            Value::Timestamp { ts, .. } => ts.as_nanosecond() as i64,
            _ => {
                return Err(dt_err(
                    "timestamp-diff requires Timestamp as first argument",
                    call_span.clone(),
                ))
            }
        };

        let t2_nanos = match &t2_val {
            Value::Timestamp { ts, .. } => ts.as_nanosecond() as i64,
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

        Ok(Arc::new(Thunk::value(
            Value::Duration {
                nanos: result,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Compare two timestamps: t1 < t2
pub fn builtin_timestamp_lt(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t1_thunk, t2_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp<? requires 2 arguments",
                call_span.clone(),
            ));
        };

        let t1_val = t1_thunk.clone().require_value()?.clone();
        let t2_val = t2_thunk.clone().require_value()?.clone();

        let t1 = match &t1_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp<? requires Timestamp as first argument",
                    call_span.clone(),
                ))
            }
        };

        let t2 = match &t2_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp<? requires Timestamp as second argument",
                    call_span.clone(),
                ))
            }
        };

        Ok(Arc::new(Thunk::value(
            Value::Int {
                n: if t1 < t2 { 1 } else { 0 },
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Compare two timestamps: t1 > t2
pub fn builtin_timestamp_gt(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t1_thunk, t2_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp>? requires 2 arguments",
                call_span.clone(),
            ));
        };

        let t1_val = t1_thunk.clone().require_value()?.clone();
        let t2_val = t2_thunk.clone().require_value()?.clone();

        let t1 = match &t1_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp>? requires Timestamp as first argument",
                    call_span.clone(),
                ))
            }
        };

        let t2 = match &t2_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp>? requires Timestamp as second argument",
                    call_span.clone(),
                ))
            }
        };

        Ok(Arc::new(Thunk::value(
            Value::Int {
                n: if t1 > t2 { 1 } else { 0 },
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Compare two timestamps for equality: t1 == t2
pub fn builtin_timestamp_eq(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t1_thunk, t2_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp=? requires 2 arguments",
                call_span.clone(),
            ));
        };

        let t1_val = t1_thunk.clone().require_value()?.clone();
        let t2_val = t2_thunk.clone().require_value()?.clone();

        let t1 = match &t1_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp=? requires Timestamp as first argument",
                    call_span.clone(),
                ))
            }
        };

        let t2 = match &t2_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp=? requires Timestamp as second argument",
                    call_span.clone(),
                ))
            }
        };

        Ok(Arc::new(Thunk::value(
            Value::Int {
                n: if t1 == t2 { 1 } else { 0 },
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Extract the UTC year from a timestamp.
pub fn builtin_timestamp_year(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-year requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk.clone().require_value()?.clone();
        let ts = match &t_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp-year requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
        let year = dt.year() as i64;

        Ok(Arc::new(Thunk::value(
            Value::Int {
                n: year,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Extract the UTC month (1-12) from a timestamp.
pub fn builtin_timestamp_month(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-month requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk.clone().require_value()?.clone();
        let ts = match &t_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp-month requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
        let month = dt.month() as i64;

        Ok(Arc::new(Thunk::value(
            Value::Int {
                n: month,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Extract the UTC day (1-31) from a timestamp.
pub fn builtin_timestamp_day(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-day requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk.clone().require_value()?.clone();
        let ts = match &t_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp-day requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
        let day = dt.day() as i64;

        Ok(Arc::new(Thunk::value(
            Value::Int {
                n: day,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Extract the UTC hour (0-23) from a timestamp.
pub fn builtin_timestamp_hour(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-hour requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk.clone().require_value()?.clone();
        let ts = match &t_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp-hour requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
        let hour = dt.hour() as i64;

        Ok(Arc::new(Thunk::value(
            Value::Int {
                n: hour,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Extract the UTC minute (0-59) from a timestamp.
pub fn builtin_timestamp_minute(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-minute requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk.clone().require_value()?.clone();
        let ts = match &t_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp-minute requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
        let minute = dt.minute() as i64;

        Ok(Arc::new(Thunk::value(
            Value::Int {
                n: minute,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Extract the UTC second (0-59) from a timestamp.
pub fn builtin_timestamp_second(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-second requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk.clone().require_value()?.clone();
        let ts = match &t_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp-second requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);
        let second = dt.second() as i64;

        Ok(Arc::new(Thunk::value(
            Value::Int {
                n: second,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Extract all UTC components from a timestamp as a dict.
pub fn builtin_timestamp_parts(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-parts requires 1 argument",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk.clone().require_value()?.clone();
        let ts = match &t_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp-parts requires a Timestamp",
                    call_span.clone(),
                ))
            }
        };

        let dt = ts.to_zoned(jiff::tz::TimeZone::UTC);

        let mut map: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        map.insert(
            HashableValue::Str("year".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: dt.year() as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )),
        );
        map.insert(
            HashableValue::Str("month".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: dt.month() as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )),
        );
        map.insert(
            HashableValue::Str("day".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: dt.day() as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )),
        );
        map.insert(
            HashableValue::Str("hour".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: dt.hour() as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )),
        );
        map.insert(
            HashableValue::Str("minute".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: dt.minute() as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )),
        );
        map.insert(
            HashableValue::Str("second".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: dt.second() as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )),
        );

        Ok(Arc::new(Thunk::value(
            Value::Dict {
                entries: map,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Create a duration from nanoseconds.
pub fn builtin_duration_nanos(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [n_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration-nanos requires 1 argument",
                call_span.clone(),
            ));
        };

        let n_val = n_thunk.clone().require_value()?.clone();
        let nanos = match &n_val {
            Value::Int { n, .. } => *n,
            _ => return Err(dt_err("duration-nanos requires an Int", call_span.clone())),
        };

        Ok(Arc::new(Thunk::value(
            Value::Duration {
                nanos,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Create a Timestamp from nanoseconds since the Unix epoch.
///
/// Mirrors `duration-nanos` but produces a `Timestamp` instead of a `Duration`.
/// Required for SCN round-trip serialization where nanosecond-precision timestamps
/// must survive serialize/deserialize without conversion through seconds.
pub fn builtin_timestamp_nanos(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [n_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-nanos requires 1 argument",
                call_span.clone(),
            ));
        };

        let n_val = n_thunk.clone().require_value()?.clone();
        let nanos = match &n_val {
            Value::Int { n, .. } => *n,
            _ => return Err(dt_err("timestamp-nanos requires an Int", call_span.clone())),
        };

        let ts = jiff::Timestamp::from_nanosecond(nanos as i128).map_err(|e| {
            dt_err(
                format!("timestamp-nanos: nanoseconds out of range: {e}"),
                call_span.clone(),
            )
        })?;

        Ok(Arc::new(Thunk::value(
            Value::Timestamp {
                ts,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Create a duration from seconds.
pub fn builtin_duration_seconds(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [n_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration-seconds requires 1 argument",
                call_span.clone(),
            ));
        };

        let n_val = n_thunk.clone().require_value()?.clone();
        let seconds = match &n_val {
            Value::Int { n, .. } => *n,
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

        Ok(Arc::new(Thunk::value(
            Value::Duration {
                nanos,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Create a duration from minutes.
pub fn builtin_duration_minutes(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [n_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration-minutes requires 1 argument",
                call_span.clone(),
            ));
        };

        let n_val = n_thunk.clone().require_value()?.clone();
        let minutes = match &n_val {
            Value::Int { n, .. } => *n,
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

        Ok(Arc::new(Thunk::value(
            Value::Duration {
                nanos,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Create a duration from hours.
pub fn builtin_duration_hours(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [n_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration-hours requires 1 argument",
                call_span.clone(),
            ));
        };

        let n_val = n_thunk.clone().require_value()?.clone();
        let hours = match &n_val {
            Value::Int { n, .. } => *n,
            _ => return Err(dt_err("duration-hours requires an Int", call_span.clone())),
        };

        let nanos = hours
            .checked_mul(3600)
            .and_then(|s| s.checked_mul(1_000_000_000))
            .ok_or_else(|| dt_err("duration-hours overflow", call_span.clone()))?;

        Ok(Arc::new(Thunk::value(
            Value::Duration {
                nanos,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Create a duration from days.
pub fn builtin_duration_days(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [n_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration-days requires 1 argument",
                call_span.clone(),
            ));
        };

        let n_val = n_thunk.clone().require_value()?.clone();
        let days = match &n_val {
            Value::Int { n, .. } => *n,
            _ => return Err(dt_err("duration-days requires an Int", call_span.clone())),
        };

        let nanos = days
            .checked_mul(86400)
            .and_then(|s| s.checked_mul(1_000_000_000))
            .ok_or_else(|| dt_err("duration-days overflow", call_span.clone()))?;

        Ok(Arc::new(Thunk::value(
            Value::Duration {
                nanos,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Convert a duration to seconds (truncating nanoseconds).
pub fn builtin_duration_to_seconds(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [d_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration->seconds requires 1 argument",
                call_span.clone(),
            ));
        };

        let d_val = d_thunk.clone().require_value()?.clone();
        let nanos = match &d_val {
            Value::Duration { nanos: n, .. } => *n,
            _ => {
                return Err(dt_err(
                    "duration->seconds requires a Duration",
                    call_span.clone(),
                ))
            }
        };

        let seconds = nanos / 1_000_000_000;

        Ok(Arc::new(Thunk::value(
            Value::Int {
                n: seconds,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Convert a duration to nanoseconds.
pub fn builtin_duration_to_nanos(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [d_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "duration->nanos requires 1 argument",
                call_span.clone(),
            ));
        };

        let d_val = d_thunk.clone().require_value()?.clone();
        let nanos = match &d_val {
            Value::Duration { nanos: n, .. } => *n,
            _ => {
                return Err(dt_err(
                    "duration->nanos requires a Duration",
                    call_span.clone(),
                ))
            }
        };

        Ok(Arc::new(Thunk::value(
            Value::Int {
                n: nanos,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Load a timezone from a zoneinfo directory (via DirCap).
pub fn builtin_load_tz(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [dir_thunk, name_thunk] = args.args.as_slice() else {
            return Err(dt_err("load-tz requires 2 arguments", call_span.clone()));
        };

        let dir_val = dir_thunk.clone().require_value()?.clone();
        let name_val = name_thunk.clone().require_value()?.clone();

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

        Ok(Arc::new(Thunk::value(
            Value::Timezone {
                tz: Arc::new(tz),
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Convert a timestamp to local time in a timezone.
pub fn builtin_timestamp_in_tz(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        let [t_thunk, tz_thunk] = args.args.as_slice() else {
            return Err(dt_err(
                "timestamp-in-tz requires 2 arguments",
                call_span.clone(),
            ));
        };

        let t_val = t_thunk.clone().require_value()?.clone();
        let tz_val = tz_thunk.clone().require_value()?.clone();

        let ts = match &t_val {
            Value::Timestamp { ts, .. } => ts.clone(),
            _ => {
                return Err(dt_err(
                    "timestamp-in-tz requires Timestamp as first argument",
                    call_span.clone(),
                ))
            }
        };

        let tz = match &tz_val {
            Value::Timezone { tz, .. } => tz,
            _ => {
                return Err(dt_err(
                    "timestamp-in-tz requires Timezone as second argument",
                    call_span.clone(),
                ))
            }
        };

        let dt = ts.to_zoned((**tz).clone());

        let mut map: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        map.insert(
            HashableValue::Str("year".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: dt.year() as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )),
        );
        map.insert(
            HashableValue::Str("month".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: dt.month() as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )),
        );
        map.insert(
            HashableValue::Str("day".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: dt.day() as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )),
        );
        map.insert(
            HashableValue::Str("hour".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: dt.hour() as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )),
        );
        map.insert(
            HashableValue::Str("minute".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: dt.minute() as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )),
        );
        map.insert(
            HashableValue::Str("second".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: dt.second() as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )),
        );
        map.insert(
            HashableValue::Str("offset-seconds".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: dt.offset().seconds() as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )),
        );
        map.insert(
            HashableValue::Str("tz-name".into()),
            Arc::new(Thunk::value(
                string_val(dt.time_zone().iana_name().unwrap_or("Unknown")),
                call_span.clone(),
            )),
        );

        Ok(Arc::new(Thunk::value(
            Value::Dict {
                entries: map,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Convert local time components to a UTC timestamp in a timezone.
pub fn builtin_local_to_timestamp(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        if args.args.len() != 7 {
            return Err(dt_err(
                "local->timestamp requires 7 arguments (year month day hour minute second timezone)",
                call_span.clone(),
            ));
        }

        let year_val = Arc::clone(&args.args[0]).require_value()?.clone();
        let month_val = Arc::clone(&args.args[1]).require_value()?.clone();
        let day_val = Arc::clone(&args.args[2]).require_value()?.clone();
        let hour_val = Arc::clone(&args.args[3]).require_value()?.clone();
        let minute_val = Arc::clone(&args.args[4]).require_value()?.clone();
        let second_val = Arc::clone(&args.args[5]).require_value()?.clone();
        let tz_val = Arc::clone(&args.args[6]).require_value()?.clone();

        let year = match &year_val {
            Value::Int { n, .. } => *n as i16,
            _ => {
                return Err(dt_err(
                    "local->timestamp year must be Int",
                    call_span.clone(),
                ))
            }
        };
        let month = match &month_val {
            Value::Int { n, .. } => *n as i8,
            _ => {
                return Err(dt_err(
                    "local->timestamp month must be Int",
                    call_span.clone(),
                ))
            }
        };
        let day = match &day_val {
            Value::Int { n, .. } => *n as i8,
            _ => {
                return Err(dt_err(
                    "local->timestamp day must be Int",
                    call_span.clone(),
                ))
            }
        };
        let hour = match &hour_val {
            Value::Int { n, .. } => *n as i8,
            _ => {
                return Err(dt_err(
                    "local->timestamp hour must be Int",
                    call_span.clone(),
                ))
            }
        };
        let minute = match &minute_val {
            Value::Int { n, .. } => *n as i8,
            _ => {
                return Err(dt_err(
                    "local->timestamp minute must be Int",
                    call_span.clone(),
                ))
            }
        };
        let second = match &second_val {
            Value::Int { n, .. } => *n as i8,
            _ => {
                return Err(dt_err(
                    "local->timestamp second must be Int",
                    call_span.clone(),
                ))
            }
        };
        let tz = match &tz_val {
            Value::Timezone { tz, .. } => tz,
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

        Ok(Arc::new(Thunk::value(
            Value::Timestamp {
                ts,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Read /etc/localtime as a symlink and extract the IANA timezone name.
///
/// The symlink target on most Unix systems points to a path like
/// `/usr/share/zoneinfo/America/New_York`. The IANA name is the last two
/// path components joined with `/`. If the symlink target cannot be parsed
/// as a two-component IANA name, returns `"UTC"`. IO errors are propagated
/// to the caller, which distinguishes NotFound (no symlink → POSIX default)
/// from real IO failures (permission denied, etc.).
fn local_tz_from_symlink() -> Result<String, std::io::Error> {
    let target = std::fs::read_link("/etc/localtime")?;
    // Extract the last two components: e.g. "America/New_York"
    let components: Vec<&std::ffi::OsStr> = target.iter().filter(|c| !c.is_empty()).collect();
    if components.len() >= 2 {
        let last = components[components.len() - 1].to_str().unwrap_or("");
        let second_last = components[components.len() - 2].to_str().unwrap_or("");
        if !last.is_empty()
            && !second_last.is_empty()
            && !last.contains('.')
            && !second_last.contains('.')
            && last
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '+')
            && second_last
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '+')
        {
            return Ok(format!("{}/{}", second_last, last));
        }
    }
    // Single component (e.g. "UTC" or "GMT" directly in zoneinfo root)
    if components.len() == 1 {
        if let Some(name) = components[0].to_str() {
            if !name.is_empty() && !name.contains('.') {
                return Ok(name.to_string());
            }
        }
    }
    Ok("UTC".to_string())
}

/// Resolve the timezone name from `/etc/localtime`, mapping `NotFound` to `"UTC"` and
/// propagating any other IO error as a tinct user error.
fn resolve_tz_from_symlink(call_span: &Span) -> EvalResult<String> {
    match local_tz_from_symlink() {
        Ok(name) => Ok(name),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("UTC".to_string()),
        Err(e) => Err(EvalError::user_error(
            format!("local-tz-name: failed to read system timezone: {e}"),
            call_span.clone(),
        )
        .into()),
    }
}

/// Get the local timezone name from the system.
///
/// This is a zero-argument builtin — system timezone is an ambient property
/// (analogous to `std::env::var("TZ")`), not a file the caller controls via capability.
pub fn builtin_local_tz_name(
    args: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let call_span = args.call_span.clone();
        if !args.args.is_empty() {
            return Err(dt_err(
                "local-tz-name takes no arguments",
                call_span.clone(),
            ));
        }

        // Determine the local timezone name.
        //
        // Priority:
        //   1. TZ environment variable — if set, non-empty, and allowed by env_allowed policy.
        //   2. /etc/localtime symlink — read the symlink target and extract
        //      the last two path components (e.g. "America/New_York").
        //      NotFound means no symlink is present; fall back to "UTC" (POSIX default).
        //      Any other IO error (permission denied, etc.) is surfaced as a tinct error.
        //   3. "UTC" when neither TZ nor the symlink is available.
        let tz_allowed = match &args.ctx.env_allowed {
            None => true, // unrestricted
            Some(set) => set.contains("TZ"),
        };
        let tz_name = if tz_allowed {
            if let Ok(tz) = std::env::var("TZ") {
                if !tz.is_empty() {
                    tz
                } else {
                    resolve_tz_from_symlink(&call_span)?
                }
            } else {
                resolve_tz_from_symlink(&call_span)?
            }
        } else {
            resolve_tz_from_symlink(&call_span)?
        };

        Ok(Arc::new(Thunk::value(string_val(&tz_name), call_span)))
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
            "timestamp-nanos",
            builtin_timestamp_nanos,
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
        builtin!("local-tz-name", builtin_local_tz_name, [], 0),
    ]
}
