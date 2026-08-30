// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic `UnixTime.*` construction and calendar decomposition.
//!
//! The M1 catalogue represents Unix timestamps as signed 32-bit `Integer`
//! values. There is no current-time method in that catalogue: every constructor
//! takes explicit date/time arguments, and every decomposition method takes an
//! explicit timestamp. This module therefore uses only integer Gregorian/POSIX
//! arithmetic. It never reads the host clock, locale, or timezone.
//!
//! `UnixTime.Timezone` sets run-local state on [`Env`]. Whole values `-12..=14`
//! mean hours; other values use signed `HHMM`, within the documented
//! `-1245..=1445` range. Local conversion uses that fixed offset and defaults to
//! UTC in a fresh environment.
//!
//! The catalogue permits constructor seconds `60` and `61`. POSIX timestamps do
//! not encode leap seconds, so this model normalizes those values into the next
//! minute. `FromGPS` floors its non-negative fractional seconds. Both details
//! remain explicit evaluator assumptions until a publishable M1 capture exists.

use crate::env::Env;
use crate::error::EvalError;
use crate::value::{M1Scalar, Value};

const SECONDS_PER_MINUTE: i64 = 60;
const SECONDS_PER_HOUR: i64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: i64 = 24 * SECONDS_PER_HOUR;

/// The exact `UnixTime` method set implemented by runtime and coverage.
pub(crate) const METHODS: &[&str] = &[
    "FromGPS",
    "FromLocal",
    "FromUtc",
    "Timezone",
    "ToLocalDay",
    "ToLocalHour",
    "ToLocalMinute",
    "ToLocalMonth",
    "ToLocalSecond",
    "ToLocalWeekDay",
    "ToLocalYear",
    "ToLocalYearDay",
    "ToUtcDay",
    "ToUtcHour",
    "ToUtcMinute",
    "ToUtcMonth",
    "ToUtcSecond",
    "ToUtcWeekDay",
    "ToUtcYear",
    "ToUtcYearDay",
];

/// Evaluate one catalogue-backed `UnixTime.<method>` call.
pub(crate) fn call(method: &str, args: &[Value], env: &mut Env) -> Result<Value, EvalError> {
    match method {
        "FromGPS" => from_gps(args),
        "FromLocal" => from_components(args, env.unix_timezone_offset_seconds(), method),
        "FromUtc" => from_components(args, 0, method),
        "Timezone" => set_timezone(args, env),
        "ToLocalDay" => component(args, env, method, |civil| civil.day),
        "ToLocalHour" => component(args, env, method, |civil| civil.hour),
        "ToLocalMinute" => component(args, env, method, |civil| civil.minute),
        "ToLocalMonth" => component(args, env, method, |civil| civil.month),
        "ToLocalSecond" => component(args, env, method, |civil| civil.second),
        "ToLocalWeekDay" => component(args, env, method, |civil| civil.weekday),
        "ToLocalYear" => component(args, env, method, |civil| civil.year),
        "ToLocalYearDay" => component(args, env, method, |civil| civil.year_day),
        "ToUtcDay" => utc_component(args, method, |civil| civil.day),
        "ToUtcHour" => utc_component(args, method, |civil| civil.hour),
        "ToUtcMinute" => utc_component(args, method, |civil| civil.minute),
        "ToUtcMonth" => utc_component(args, method, |civil| civil.month),
        "ToUtcSecond" => utc_component(args, method, |civil| civil.second),
        "ToUtcWeekDay" => utc_component(args, method, |civil| civil.weekday),
        "ToUtcYear" => utc_component(args, method, |civil| civil.year),
        "ToUtcYearDay" => utc_component(args, method, |civil| civil.year_day),
        _ => Err(EvalError::UnsupportedBuiltin {
            object: "UnixTime".to_string(),
            method: method.to_string(),
        }),
    }
}

fn from_gps(args: &[Value]) -> Result<Value, EvalError> {
    let date = unsigned_arg("FromGPS", "date", &args[0])?;
    if date > 999_999 {
        return Err(bad_call(
            "FromGPS",
            format!("date {date} is not a DDMMYY value"),
        ));
    }
    let day = i32::try_from(date / 10_000).expect("six-digit date day fits i32");
    let month = i32::try_from((date / 100) % 100).expect("six-digit date month fits i32");
    let short_year = i32::try_from(date % 100).expect("two-digit year fits i32");
    let year = match short_year {
        70..=99 => 1900 + short_year,
        0..=38 => 2000 + short_year,
        _ => {
            return Err(bad_call(
                "FromGPS",
                format!("two-digit year {short_year:02} is outside 1970-2038"),
            ));
        }
    };
    let seconds = float_arg("FromGPS", "time", &args[1])?;
    if !seconds.is_finite() || !(0.0..86_400.0).contains(&seconds) {
        return Err(bad_call(
            "FromGPS",
            format!("UTC seconds since midnight must be finite and in [0, 86400), got {seconds:?}"),
        ));
    }
    let whole_seconds = seconds.floor() as i64;
    let hour = i32::try_from(whole_seconds / SECONDS_PER_HOUR).expect("GPS hour fits i32");
    let minute =
        i32::try_from((whole_seconds / SECONDS_PER_MINUTE) % 60).expect("GPS minute fits i32");
    let second = i32::try_from(whole_seconds % 60).expect("GPS second fits i32");
    construct_timestamp("FromGPS", year, month - 1, day, hour, minute, second, 0)
}

fn from_components(args: &[Value], offset_seconds: i32, method: &str) -> Result<Value, EvalError> {
    let year = integer_arg(method, "year", &args[0])?;
    let month = integer_arg(method, "month", &args[1])?;
    let day = integer_arg(method, "day", &args[2])?;
    let hour = integer_arg(method, "hour", &args[3])?;
    let minute = integer_arg(method, "minute", &args[4])?;
    let second = integer_arg(method, "second", &args[5])?;
    construct_timestamp(
        method,
        year,
        month,
        day,
        hour,
        minute,
        second,
        offset_seconds,
    )
}

#[allow(clippy::too_many_arguments)]
fn construct_timestamp(
    method: &str,
    year: i32,
    month: i32,
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
    offset_seconds: i32,
) -> Result<Value, EvalError> {
    if !(1970..=2038).contains(&year) {
        return Err(bad_call(
            method,
            format!("year must be in 1970..=2038, got {year}"),
        ));
    }
    if !(0..=11).contains(&month) {
        return Err(bad_call(
            method,
            format!("month must be in 0..=11, got {month}"),
        ));
    }
    let days = days_from_civil(year, month, day).ok_or_else(|| {
        bad_call(
            method,
            format!("day {day} is invalid for year {year}, month {month}"),
        )
    })?;
    if !(0..=23).contains(&hour) {
        return Err(bad_call(
            method,
            format!("hour must be in 0..=23, got {hour}"),
        ));
    }
    if !(0..=59).contains(&minute) {
        return Err(bad_call(
            method,
            format!("minute must be in 0..=59, got {minute}"),
        ));
    }
    if !(0..=61).contains(&second) {
        return Err(bad_call(
            method,
            format!("second must be in 0..=61, got {second}"),
        ));
    }
    let local_seconds = days
        .checked_mul(SECONDS_PER_DAY)
        .and_then(|value| value.checked_add(i64::from(hour) * SECONDS_PER_HOUR))
        .and_then(|value| value.checked_add(i64::from(minute) * SECONDS_PER_MINUTE))
        .and_then(|value| value.checked_add(i64::from(second)))
        .expect("supported calendar range fits i64");
    let unix = local_seconds - i64::from(offset_seconds);
    let unix = i32::try_from(unix).map_err(|_| {
        bad_call(
            method,
            format!("date/time resolves to Unix timestamp {unix}, outside the M1 Integer range"),
        )
    })?;
    Ok(Value::m1_integer(unix))
}

fn set_timezone(args: &[Value], env: &mut Env) -> Result<Value, EvalError> {
    let encoded = integer_arg("Timezone", "hour", &args[0])?;
    let seconds = timezone_seconds(encoded)?;
    env.set_unix_timezone_offset_seconds(seconds);
    Ok(Value::Bool(true))
}

fn timezone_seconds(encoded: i32) -> Result<i32, EvalError> {
    if (-12..=14).contains(&encoded) {
        return Ok(encoded * 3_600);
    }
    if !(-1245..=1445).contains(&encoded) {
        return Err(bad_call(
            "Timezone",
            format!("offset must be an hour in -12..=14 or HHMM in -1245..=1445, got {encoded}"),
        ));
    }
    let magnitude = encoded.unsigned_abs();
    let hours = magnitude / 100;
    let minutes = magnitude % 100;
    if minutes >= 60 {
        return Err(bad_call(
            "Timezone",
            format!("HHMM minute field must be in 00..=59, got {encoded}"),
        ));
    }
    let magnitude_seconds = hours * 3_600 + minutes * 60;
    let signed = i32::try_from(magnitude_seconds).expect("documented timezone fits i32");
    Ok(if encoded < 0 { -signed } else { signed })
}

fn component(
    args: &[Value],
    env: &Env,
    method: &str,
    select: impl FnOnce(Civil) -> i32,
) -> Result<Value, EvalError> {
    let unix = integer_arg(method, "unix", &args[0])?;
    let civil = decompose(unix, env.unix_timezone_offset_seconds());
    Ok(Value::m1_integer(select(civil)))
}

fn utc_component(
    args: &[Value],
    method: &str,
    select: impl FnOnce(Civil) -> i32,
) -> Result<Value, EvalError> {
    let unix = integer_arg(method, "unix", &args[0])?;
    Ok(Value::m1_integer(select(decompose(unix, 0))))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Civil {
    year: i32,
    /// Zero-based month, matching the catalogue.
    month: i32,
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
    /// Sunday is zero, matching POSIX `tm_wday` and the M1 manual convention.
    weekday: i32,
    /// Zero-based day within the year.
    year_day: i32,
}

fn decompose(unix: i32, offset_seconds: i32) -> Civil {
    let adjusted = i64::from(unix) + i64::from(offset_seconds);
    let days = adjusted.div_euclid(SECONDS_PER_DAY);
    let within_day = adjusted.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let first_day = days_from_civil(year, 0, 1).expect("January 1 is always valid");
    Civil {
        year,
        month,
        day,
        hour: i32::try_from(within_day / SECONDS_PER_HOUR).expect("hour fits i32"),
        minute: i32::try_from((within_day / SECONDS_PER_MINUTE) % 60).expect("minute fits i32"),
        second: i32::try_from(within_day % 60).expect("second fits i32"),
        weekday: i32::try_from((days + 4).rem_euclid(7)).expect("weekday fits i32"),
        year_day: i32::try_from(days - first_day).expect("year day fits i32"),
    }
}

/// Days since 1970-01-01 for a validated proleptic-Gregorian date.
fn days_from_civil(year: i32, month_zero: i32, day: i32) -> Option<i64> {
    if !(0..=11).contains(&month_zero) || day < 1 || day > days_in_month(year, month_zero) {
        return None;
    }
    let month = i64::from(month_zero + 1);
    let mut year = i64::from(year);
    year -= i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

/// Inverse of [`days_from_civil`] for every timestamp representable by M1.
fn civil_from_days(days_since_epoch: i64) -> (i32, i32, i32) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (
        i32::try_from(year).expect("M1 timestamp year fits i32"),
        i32::try_from(month - 1).expect("month fits i32"),
        i32::try_from(day).expect("day fits i32"),
    )
}

fn days_in_month(year: i32, month_zero: i32) -> i32 {
    match month_zero {
        0 | 2 | 4 | 6 | 7 | 9 | 11 => 31,
        3 | 5 | 8 | 10 => 30,
        1 if is_leap_year(year) => 29,
        1 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn integer_arg(method: &str, name: &str, value: &Value) -> Result<i32, EvalError> {
    match value {
        Value::M1(M1Scalar::Integer(value)) => Ok(*value),
        other => Err(bad_call(
            method,
            format!("{name} expects M1 Integer, got {other:?}"),
        )),
    }
}

fn unsigned_arg(method: &str, name: &str, value: &Value) -> Result<u32, EvalError> {
    match value {
        Value::M1(M1Scalar::UnsignedInteger(value)) => Ok(*value),
        other => Err(bad_call(
            method,
            format!("{name} expects M1 UnsignedInteger, got {other:?}"),
        )),
    }
}

fn float_arg(method: &str, name: &str, value: &Value) -> Result<f32, EvalError> {
    match value {
        Value::M1(M1Scalar::FloatingPoint(value)) => Ok(*value),
        other => Err(bad_call(
            method,
            format!("{name} expects M1 FloatingPoint, got {other:?}"),
        )),
    }
}

fn bad_call(method: &str, detail: impl std::fmt::Display) -> EvalError {
    EvalError::BadCall {
        detail: format!("UnixTime.{method}: {detail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn int(value: Value) -> i32 {
        match value {
            Value::M1(M1Scalar::Integer(value)) => value,
            other => panic!("expected Integer, got {other:?}"),
        }
    }

    fn invoke(method: &str, args: &[Value], env: &mut Env) -> Result<Value, EvalError> {
        call(method, args, env)
    }

    fn from_utc(parts: [i32; 6]) -> Result<i32, EvalError> {
        let mut env = Env::new();
        let args = parts.map(Value::m1_integer);
        invoke("FromUtc", &args, &mut env).map(int)
    }

    #[test]
    fn implementation_table_matches_the_catalogue_exactly() {
        let object = m1_typecheck::intrinsics::get()
            .library_object("UnixTime")
            .expect("catalogue has UnixTime");
        let catalogued: BTreeSet<&str> = object
            .functions
            .iter()
            .map(|overload| overload.name.as_str())
            .collect();
        let implemented: BTreeSet<&str> = METHODS.iter().copied().collect();
        assert_eq!(implemented, catalogued);
        assert_eq!(METHODS.len(), 20, "duplicate implementation entries");
    }

    #[test]
    fn independent_epoch_leap_and_signed_maximum_vectors() {
        for (parts, expected) in [
            ([1970, 0, 1, 0, 0, 0], 0),
            ([1970, 0, 2, 0, 0, 0], 86_400),
            ([2000, 1, 29, 12, 34, 56], 951_827_696),
            ([2038, 0, 19, 3, 14, 7], i32::MAX),
        ] {
            assert_eq!(from_utc(parts).unwrap(), expected, "parts={parts:?}");
        }
    }

    #[test]
    fn utc_decomposition_covers_every_catalogued_component() {
        let mut env = Env::new();
        let unix = Value::m1_integer(from_utc([2004, 1, 29, 12, 34, 56]).unwrap());
        for (method, expected) in [
            ("ToUtcYear", 2004),
            ("ToUtcMonth", 1),
            ("ToUtcDay", 29),
            ("ToUtcHour", 12),
            ("ToUtcMinute", 34),
            ("ToUtcSecond", 56),
            ("ToUtcWeekDay", 0),
            ("ToUtcYearDay", 59),
        ] {
            assert_eq!(
                int(invoke(method, std::slice::from_ref(&unix), &mut env).unwrap()),
                expected
            );
        }
    }

    #[test]
    fn local_decomposition_uses_fixed_offset_across_day_and_year_boundaries() {
        let mut env = Env::new();
        assert_eq!(
            invoke("Timezone", &[Value::m1_integer(530)], &mut env).unwrap(),
            Value::Bool(true)
        );
        let unix = Value::m1_integer(from_utc([1999, 11, 31, 20, 0, 1]).unwrap());
        for (method, expected) in [
            ("ToLocalYear", 2000),
            ("ToLocalMonth", 0),
            ("ToLocalDay", 1),
            ("ToLocalHour", 1),
            ("ToLocalMinute", 30),
            ("ToLocalSecond", 1),
            ("ToLocalWeekDay", 6),
            ("ToLocalYearDay", 0),
        ] {
            assert_eq!(
                int(invoke(method, std::slice::from_ref(&unix), &mut env).unwrap()),
                expected
            );
        }
        let local_args = [2000, 0, 1, 1, 30, 1].map(Value::m1_integer);
        assert_eq!(
            int(invoke("FromLocal", &local_args, &mut env).unwrap()),
            int(unix)
        );
    }

    #[test]
    fn timezone_encodings_persist_and_fresh_env_resets_to_utc() {
        for (encoded, seconds) in [
            (0, 0),
            (10, 36_000),
            (-5, -18_000),
            (530, 19_800),
            (-930, -34_200),
            (1245, 45_900),
            (1445, 53_100),
        ] {
            let mut env = Env::new();
            invoke("Timezone", &[Value::m1_integer(encoded)], &mut env).unwrap();
            assert_eq!(env.unix_timezone_offset_seconds(), seconds);
        }
        assert_eq!(Env::new().unix_timezone_offset_seconds(), 0);
    }

    #[test]
    fn invalid_timezone_does_not_replace_previous_state() {
        let mut env = Env::new();
        invoke("Timezone", &[Value::m1_integer(10)], &mut env).unwrap();
        for invalid in [1260, -1260, 1446, -1246, i32::MIN, i32::MAX] {
            assert!(invoke("Timezone", &[Value::m1_integer(invalid)], &mut env).is_err());
            assert_eq!(env.unix_timezone_offset_seconds(), 36_000);
        }
    }

    #[test]
    fn gps_date_pivot_and_fractional_seconds_are_explicit() {
        let mut env = Env::new();
        for (date, seconds, expected) in [
            (1_0170, 0.0, 0),
            (29_0204, 45_296.75, 1_078_058_096),
            (19_0138, 11_647.99, i32::MAX),
        ] {
            let actual = invoke(
                "FromGPS",
                &[Value::m1_unsigned(date), Value::m1_float(seconds)],
                &mut env,
            )
            .unwrap();
            assert_eq!(int(actual), expected, "date={date:06}, seconds={seconds}");
        }
        for date in [1_0139, 31_1269] {
            assert!(
                invoke(
                    "FromGPS",
                    &[Value::m1_unsigned(date), Value::m1_float(0.0)],
                    &mut env,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn leap_seconds_normalize_and_invalid_components_fail_loud() {
        assert_eq!(
            from_utc([1998, 11, 31, 23, 59, 60]).unwrap(),
            from_utc([1999, 0, 1, 0, 0, 0]).unwrap()
        );
        assert_eq!(
            from_utc([1998, 11, 31, 23, 59, 61]).unwrap(),
            from_utc([1999, 0, 1, 0, 0, 1]).unwrap()
        );
        for parts in [
            [1969, 0, 1, 0, 0, 0],
            [2039, 0, 1, 0, 0, 0],
            [2001, 1, 29, 0, 0, 0],
            [2000, 12, 1, 0, 0, 0],
            [2000, 0, 1, 24, 0, 0],
            [2000, 0, 1, 0, 60, 0],
            [2000, 0, 1, 0, 0, 62],
        ] {
            assert!(from_utc(parts).is_err(), "parts={parts:?}");
        }
        assert!(from_utc([2038, 0, 19, 3, 14, 8]).is_err());
    }

    #[test]
    fn argument_families_are_strict() {
        let mut env = Env::new();
        assert!(matches!(
            invoke("ToUtcYear", &[Value::m1_unsigned(0)], &mut env),
            Err(EvalError::BadCall { .. })
        ));
        assert!(matches!(
            invoke(
                "FromGPS",
                &[Value::m1_integer(1_0170), Value::m1_float(0.0)],
                &mut env,
            ),
            Err(EvalError::BadCall { .. })
        ));
        assert!(matches!(
            invoke(
                "FromGPS",
                &[Value::m1_unsigned(1_0170), Value::m1_float(f32::NAN)],
                &mut env,
            ),
            Err(EvalError::BadCall { .. })
        ));
    }

    #[test]
    fn every_supported_day_round_trips_and_matches_independent_time_crate() {
        use time::{Date, Month, PrimitiveDateTime, Time};

        let mut days = 0_i64;
        loop {
            let (year, month, day) = civil_from_days(days);
            if year > 2038 {
                break;
            }
            let ours = from_utc([year, month, day, 0, 0, 0]);
            if let Ok(ours) = ours {
                assert_eq!(
                    decompose(ours, 0),
                    Civil {
                        year,
                        month,
                        day,
                        hour: 0,
                        minute: 0,
                        second: 0,
                        weekday: i32::try_from((days + 4).rem_euclid(7)).unwrap(),
                        year_day: i32::try_from(days - days_from_civil(year, 0, 1).unwrap())
                            .unwrap(),
                    }
                );
                let month_ref = Month::try_from(u8::try_from(month + 1).unwrap()).unwrap();
                let date_ref =
                    Date::from_calendar_date(year, month_ref, u8::try_from(day).unwrap()).unwrap();
                let expected = PrimitiveDateTime::new(date_ref, Time::MIDNIGHT)
                    .assume_utc()
                    .unix_timestamp();
                assert_eq!(i64::from(ours), expected);
            }
            days += 1;
        }
    }

    #[test]
    fn signed_timestamp_extrema_decompose_without_host_clock_or_overflow() {
        assert_eq!(
            decompose(i32::MIN, 0),
            Civil {
                year: 1901,
                month: 11,
                day: 13,
                hour: 20,
                minute: 45,
                second: 52,
                weekday: 5,
                year_day: 346,
            }
        );
        assert_eq!(
            decompose(i32::MAX, 0),
            Civil {
                year: 2038,
                month: 0,
                day: 19,
                hour: 3,
                minute: 14,
                second: 7,
                weekday: 2,
                year_day: 18,
            }
        );
    }
}
