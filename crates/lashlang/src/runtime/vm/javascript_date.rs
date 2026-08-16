// Copyright (c) 2019 Jason Williams. Adapted from
// https://github.com/boa-dev/boa@93a9e31a83bbaa15bbd8b687e61639ffc53bbef1,
// MIT licensed. Local modifications: standalone UTC-pinned ISO-only Date
// operations for Lash's durable TypeScript VM; no host timezone access.

use super::javascript::js_stdlib_error;
use super::*;
use crate::runtime::ErrorKind;

const MS_PER_SECOND: f64 = 1_000.0;
const MS_PER_MINUTE: f64 = 60.0 * MS_PER_SECOND;
const MS_PER_HOUR: f64 = 60.0 * MS_PER_MINUTE;
const MS_PER_DAY: f64 = 24.0 * MS_PER_HOUR;
const MAX_TIME: f64 = 8.64e15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IsoDateError {
    NonIso,
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DateParts {
    year: i64,
    month: u32,
    date: u32,
    weekday: u32,
    hour: u32,
    minute: u32,
    second: u32,
    millisecond: u32,
}

pub(super) fn time_clip(time: f64) -> f64 {
    if !time.is_finite() || time.abs() > MAX_TIME {
        return f64::NAN;
    }
    let time = time.trunc();
    if time == 0.0 { 0.0 } else { time }
}

fn integer_or_infinity(value: f64) -> f64 {
    value.abs().floor().copysign(value)
}

fn make_full_year(year: f64) -> f64 {
    if year.is_nan() {
        return f64::NAN;
    }
    let year = integer_or_infinity(year);
    if (0.0..=99.0).contains(&year) {
        year + 1_900.0
    } else {
        year
    }
}

fn day_from_year(year: f64) -> f64 {
    365.0 * (year - 1_970.0) + ((year - 1_969.0) / 4.0).floor() - ((year - 1_901.0) / 100.0).floor()
        + ((year - 1_601.0) / 400.0).floor()
}

fn make_day(year: f64, month: f64, date: f64) -> f64 {
    if !year.is_finite() || !month.is_finite() || !date.is_finite() {
        return f64::NAN;
    }
    let year = integer_or_infinity(year);
    let month = integer_or_infinity(month);
    let date = integer_or_infinity(date);
    let normalized_year = year + (month / 12.0).floor();
    if !normalized_year.is_finite() {
        return f64::NAN;
    }
    let normalized_month = month.rem_euclid(12.0) as u8;
    let after_february = f64::from(normalized_month > 1);
    let days_before_month = match normalized_month {
        0 => 0.0,
        1 => 31.0,
        2 => 59.0,
        3 => 90.0,
        4 => 120.0,
        5 => 151.0,
        6 => 181.0,
        7 => 212.0,
        8 => 243.0,
        9 => 273.0,
        10 => 304.0,
        11 => 334.0,
        _ => unreachable!(),
    };
    let first = day_from_year(normalized_year + after_february) - 365.0 * after_february
        + days_before_month;
    first + date - 1.0
}

fn make_time(hour: f64, minute: f64, second: f64, millisecond: f64) -> f64 {
    if !hour.is_finite() || !minute.is_finite() || !second.is_finite() || !millisecond.is_finite() {
        return f64::NAN;
    }
    integer_or_infinity(hour) * MS_PER_HOUR
        + integer_or_infinity(minute) * MS_PER_MINUTE
        + integer_or_infinity(second) * MS_PER_SECOND
        + integer_or_infinity(millisecond)
}

fn make_date(day: f64, time: f64) -> f64 {
    if !day.is_finite() || !time.is_finite() {
        f64::NAN
    } else {
        day * MS_PER_DAY + time
    }
}

pub(super) fn date_utc(numbers: &[f64]) -> f64 {
    let year = numbers.first().copied().unwrap_or(f64::NAN);
    let month = numbers.get(1).copied().unwrap_or(0.0);
    let date = numbers.get(2).copied().unwrap_or(1.0);
    let hour = numbers.get(3).copied().unwrap_or(0.0);
    let minute = numbers.get(4).copied().unwrap_or(0.0);
    let second = numbers.get(5).copied().unwrap_or(0.0);
    let millisecond = numbers.get(6).copied().unwrap_or(0.0);
    time_clip(make_date(
        make_day(make_full_year(year), month, date),
        make_time(hour, minute, second, millisecond),
    ))
}

fn parse_two(input: &[u8], cursor: &mut usize) -> Option<u32> {
    let tens = *input.get(*cursor)?;
    let ones = *input.get(*cursor + 1)?;
    if !tens.is_ascii_digit() || !ones.is_ascii_digit() {
        return None;
    }
    *cursor += 2;
    Some(u32::from(tens - b'0') * 10 + u32::from(ones - b'0'))
}

fn parse_year(input: &[u8], cursor: &mut usize) -> Option<i64> {
    let signed = matches!(input.first(), Some(b'+' | b'-'));
    let sign = if signed {
        let sign = input[0];
        *cursor = 1;
        sign
    } else {
        b'+'
    };
    let digits = if signed { 6 } else { 4 };
    let mut year = 0_i64;
    for _ in 0..digits {
        let digit = *input.get(*cursor)?;
        if !digit.is_ascii_digit() {
            return None;
        }
        year = year * 10 + i64::from(digit - b'0');
        *cursor += 1;
    }
    if sign == b'-' {
        if year == 0 {
            return None;
        }
        year = -year;
    }
    Some(year)
}

fn structurally_iso(input: &[u8]) -> bool {
    match input.first() {
        Some(b'+' | b'-') => input
            .get(1..7)
            .is_some_and(|part| part.len() == 6 && part.iter().all(u8::is_ascii_digit)),
        Some(first) if first.is_ascii_digit() => input
            .get(..4)
            .is_some_and(|part| part.iter().all(u8::is_ascii_digit)),
        _ => false,
    }
}

pub(super) fn parse_iso_date(input: &str) -> Result<f64, IsoDateError> {
    let input = input.as_bytes();
    if !input.is_ascii() || !structurally_iso(input) {
        return Err(IsoDateError::NonIso);
    }
    let mut cursor = 0;
    let year = parse_year(input, &mut cursor).ok_or(IsoDateError::Invalid)?;
    let mut month = 1;
    let mut date = 1;
    let mut hour = 0;
    let mut minute = 0;
    let mut second = 0;
    let mut millisecond = 0;

    if cursor < input.len() && input[cursor] != b'T' {
        if input[cursor] != b'-' {
            return Err(IsoDateError::Invalid);
        }
        cursor += 1;
        month = parse_two(input, &mut cursor).ok_or(IsoDateError::Invalid)?;
        if !(1..=12).contains(&month) {
            return Err(IsoDateError::Invalid);
        }
        if cursor < input.len() && input[cursor] != b'T' {
            if input[cursor] != b'-' {
                return Err(IsoDateError::Invalid);
            }
            cursor += 1;
            date = parse_two(input, &mut cursor).ok_or(IsoDateError::Invalid)?;
            if !(1..=31).contains(&date) {
                return Err(IsoDateError::Invalid);
            }
        }
    }

    let mut offset_minutes = 0_i64;
    if cursor < input.len() {
        if input[cursor] != b'T' {
            return Err(IsoDateError::Invalid);
        }
        cursor += 1;
        hour = parse_two(input, &mut cursor).ok_or(IsoDateError::Invalid)?;
        if hour > 24 || input.get(cursor) != Some(&b':') {
            return Err(IsoDateError::Invalid);
        }
        cursor += 1;
        minute = parse_two(input, &mut cursor).ok_or(IsoDateError::Invalid)?;
        if minute > 59 {
            return Err(IsoDateError::Invalid);
        }
        if input.get(cursor) == Some(&b':') {
            cursor += 1;
            second = parse_two(input, &mut cursor).ok_or(IsoDateError::Invalid)?;
            if second > 59 {
                return Err(IsoDateError::Invalid);
            }
            if input.get(cursor) == Some(&b'.') {
                cursor += 1;
                let start = cursor;
                while input.get(cursor).is_some_and(u8::is_ascii_digit) {
                    cursor += 1;
                }
                if cursor == start {
                    return Err(IsoDateError::Invalid);
                }
                let digits = &input[start..cursor];
                millisecond = digits
                    .iter()
                    .take(3)
                    .fold(0_u32, |value, digit| value * 10 + u32::from(*digit - b'0'));
                for _ in digits.len()..3 {
                    millisecond *= 10;
                }
            }
        }
        if hour == 24 && (minute != 0 || second != 0 || millisecond != 0) {
            return Err(IsoDateError::Invalid);
        }
        if cursor < input.len() {
            match input[cursor] {
                b'Z' => cursor += 1,
                sign @ (b'+' | b'-') => {
                    cursor += 1;
                    let offset_hour = parse_two(input, &mut cursor).ok_or(IsoDateError::Invalid)?;
                    if offset_hour > 23 {
                        return Err(IsoDateError::Invalid);
                    }
                    let offset_minute = if cursor < input.len() {
                        if input[cursor] != b':' {
                            return Err(IsoDateError::Invalid);
                        }
                        cursor += 1;
                        parse_two(input, &mut cursor).ok_or(IsoDateError::Invalid)?
                    } else {
                        0
                    };
                    if offset_minute > 59 {
                        return Err(IsoDateError::Invalid);
                    }
                    let magnitude = i64::from(offset_hour) * 60 + i64::from(offset_minute);
                    offset_minutes = if sign == b'+' { magnitude } else { -magnitude };
                }
                _ => return Err(IsoDateError::Invalid),
            }
        }
    }
    if cursor != input.len() {
        return Err(IsoDateError::Invalid);
    }
    let local = make_date(
        make_day(year as f64, f64::from(month - 1), f64::from(date)),
        make_time(
            f64::from(hour),
            f64::from(minute),
            f64::from(second),
            f64::from(millisecond),
        ),
    );
    Ok(time_clip(local - offset_minutes as f64 * MS_PER_MINUTE))
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let date = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, date as u32)
}

fn date_parts(milliseconds: f64) -> Option<DateParts> {
    if !milliseconds.is_finite() || milliseconds.abs() > MAX_TIME {
        return None;
    }
    let milliseconds = milliseconds.trunc() as i64;
    let days = milliseconds.div_euclid(MS_PER_DAY as i64);
    let within = milliseconds.rem_euclid(MS_PER_DAY as i64);
    let (year, month, date) = civil_from_days(days);
    Some(DateParts {
        year,
        month: month - 1,
        date,
        weekday: (days + 4).rem_euclid(7) as u32,
        hour: (within / MS_PER_HOUR as i64) as u32,
        minute: ((within % MS_PER_HOUR as i64) / MS_PER_MINUTE as i64) as u32,
        second: ((within % MS_PER_MINUTE as i64) / MS_PER_SECOND as i64) as u32,
        millisecond: (within % MS_PER_SECOND as i64) as u32,
    })
}

fn to_iso_string(milliseconds: f64) -> Option<String> {
    let parts = date_parts(milliseconds)?;
    let year = if (0..=9_999).contains(&parts.year) {
        format!("{:04}", parts.year)
    } else if parts.year < 0 {
        format!("-{:06}", parts.year.unsigned_abs())
    } else {
        format!("+{:06}", parts.year)
    };
    Some(format!(
        "{year}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        parts.month + 1,
        parts.date,
        parts.hour,
        parts.minute,
        parts.second,
        parts.millisecond
    ))
}

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn construct_javascript_date(
        &mut self,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        let milliseconds = match args {
            [] => {
                return Err(js_stdlib_error(
                    "TS_DATE_NOW_EFFECT_REQUIRED: argless Date must be lowered through the journaled clock effect",
                ));
            }
            [Value::Ref(id)] if matches!(self.heap.get(*id)?, HeapObject::Date(_)) => self
                .heap
                .date_milliseconds(*id)?
                .expect("Date receiver was checked"),
            [Value::String(text)] => match parse_iso_date(text) {
                Ok(value) => value,
                Err(IsoDateError::Invalid) => f64::NAN,
                Err(IsoDateError::NonIso) => {
                    return Err(js_stdlib_error(
                        "TS_DATE_PARSE_NON_ISO: Date accepts only the ECMA date-time string format; use an ISO string such as 2020-01-01T00:00:00.000Z",
                    ));
                }
            },
            [value] => time_clip(self.heap.javascript_to_number(value)?),
            values => {
                let numbers = values
                    .iter()
                    .take(7)
                    .map(|value| self.heap.javascript_to_number(value))
                    .collect::<Result<Vec<_>, _>>()?;
                date_utc(&numbers)
            }
        };
        self.heap.allocate_date(milliseconds)
    }

    pub(super) fn execute_javascript_date_static(
        &mut self,
        method: &str,
        args: &[Value],
    ) -> Result<Option<Value>, RuntimeError> {
        match method {
            "Date.UTC" => {
                let numbers = args
                    .iter()
                    .take(7)
                    .map(|value| self.heap.javascript_to_number(value))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Some(Value::Number(date_utc(&numbers))))
            }
            "Date.parse" => {
                let value = args.first().unwrap_or(&Value::Undefined);
                let text = self.heap.javascript_to_string(value)?;
                let milliseconds = match parse_iso_date(&text) {
                    Ok(value) => value,
                    Err(IsoDateError::Invalid) => f64::NAN,
                    Err(IsoDateError::NonIso) => {
                        return Err(js_stdlib_error(
                            "TS_DATE_PARSE_NON_ISO: Date.parse accepts only the ECMA date-time string format; use an ISO string such as 2020-01-01T00:00:00.000Z",
                        ));
                    }
                };
                Ok(Some(Value::Number(milliseconds)))
            }
            _ => Ok(None),
        }
    }

    pub(super) fn execute_javascript_date_method(
        &mut self,
        method: &str,
        receiver: HeapId,
    ) -> Result<Option<Value>, RuntimeError> {
        let milliseconds = self
            .heap
            .date_milliseconds(receiver)?
            .expect("Date receiver was checked");
        let parts = date_parts(milliseconds);
        let number = |value: Option<u32>| Value::Number(value.map_or(f64::NAN, f64::from));
        match method {
            "valueOf" | "getTime" => Ok(Some(Value::Number(milliseconds))),
            "getUTCFullYear" => Ok(Some(Value::Number(
                parts.map_or(f64::NAN, |parts| parts.year as f64),
            ))),
            "getUTCMonth" => Ok(Some(number(parts.map(|parts| parts.month)))),
            "getUTCDate" => Ok(Some(number(parts.map(|parts| parts.date)))),
            "getUTCDay" => Ok(Some(number(parts.map(|parts| parts.weekday)))),
            "getUTCHours" => Ok(Some(number(parts.map(|parts| parts.hour)))),
            "getUTCMinutes" => Ok(Some(number(parts.map(|parts| parts.minute)))),
            "getUTCSeconds" => Ok(Some(number(parts.map(|parts| parts.second)))),
            "getUTCMilliseconds" => Ok(Some(number(parts.map(|parts| parts.millisecond)))),
            "toISOString" => {
                let Some(value) = to_iso_string(milliseconds) else {
                    let error = self.heap.allocate_error(
                        ErrorKind::RangeError,
                        "Invalid time value".to_string(),
                        None,
                        None,
                    )?;
                    return Err(RuntimeError::UncaughtException { value: error });
                };
                Ok(Some(Value::String(value.into())))
            }
            "toJSON" => Ok(Some(match to_iso_string(milliseconds) {
                Some(value) => Value::String(value.into()),
                None => Value::Null,
            })),
            "getFullYear" => Err(js_stdlib_error(
                "TS_DATE_LOCAL_TIME_UNSUPPORTED: getFullYear is host-timezone dependent; use getUTCFullYear()",
            )),
            "getMonth" => Err(js_stdlib_error(
                "TS_DATE_LOCAL_TIME_UNSUPPORTED: getMonth is host-timezone dependent; use getUTCMonth()",
            )),
            "getDate" => Err(js_stdlib_error(
                "TS_DATE_LOCAL_TIME_UNSUPPORTED: getDate is host-timezone dependent; use getUTCDate()",
            )),
            "getDay" => Err(js_stdlib_error(
                "TS_DATE_LOCAL_TIME_UNSUPPORTED: getDay is host-timezone dependent; use getUTCDay()",
            )),
            "getHours" => Err(js_stdlib_error(
                "TS_DATE_LOCAL_TIME_UNSUPPORTED: getHours is host-timezone dependent; use getUTCHours()",
            )),
            "getMinutes" => Err(js_stdlib_error(
                "TS_DATE_LOCAL_TIME_UNSUPPORTED: getMinutes is host-timezone dependent; use getUTCMinutes()",
            )),
            "getSeconds" => Err(js_stdlib_error(
                "TS_DATE_LOCAL_TIME_UNSUPPORTED: getSeconds is host-timezone dependent; use getUTCSeconds()",
            )),
            "getMilliseconds" => Err(js_stdlib_error(
                "TS_DATE_LOCAL_TIME_UNSUPPORTED: getMilliseconds is host-timezone dependent; use getUTCMilliseconds()",
            )),
            "setUTCFullYear" | "setUTCMonth" | "setUTCDate" | "setUTCHours" | "setUTCMinutes"
            | "setUTCSeconds" | "setUTCMilliseconds" => Err(js_stdlib_error(format!(
                "TS_DATE_IMMUTABLE: {method} is unavailable because durable Date values are immutable; use new Date(d.getTime() + n)"
            ))),
            "toString" | "toDateString" | "toTimeString" | "toUTCString" | "toGMTString"
            | "toLocaleString" | "toLocaleDateString" | "toLocaleTimeString" => {
                Err(js_stdlib_error(format!(
                    "TS_DATE_STRING_COERCION_PENDING: {method} is unavailable; use .toISOString()"
                )))
            }
            _ => Err(js_stdlib_error(format!(
                "TS_METHOD_UNSUPPORTED: Date.{method} is not in the TypeScript runtime surface"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_math_and_extended_iso_cover_the_ecma_range() {
        assert_eq!(date_utc(&[2_000.0, 1.0, 29.0]), 951_782_400_000.0);
        assert_eq!(
            to_iso_string(0.0).as_deref(),
            Some("1970-01-01T00:00:00.000Z")
        );
        assert_eq!(
            to_iso_string(MAX_TIME).as_deref(),
            Some("+275760-09-13T00:00:00.000Z")
        );
        assert_eq!(
            to_iso_string(-MAX_TIME).as_deref(),
            Some("-271821-04-20T00:00:00.000Z")
        );
    }

    #[test]
    fn iso_parser_is_utc_pinned_and_rejects_fallback_syntax() {
        assert_eq!(parse_iso_date("1970-01-01T01:00:00+01:00"), Ok(0.0));
        assert_eq!(parse_iso_date("1970-01-01T00:00"), Ok(0.0));
        assert_eq!(parse_iso_date("January 1, 1970"), Err(IsoDateError::NonIso));
        assert_eq!(parse_iso_date("1970-13-01"), Err(IsoDateError::Invalid));
    }
}
