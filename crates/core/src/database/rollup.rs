//! Rollups: fold the values of related rows into one value.

use super::property::RollupFn;
use super::value::Value;

/// Fold `values` (one per related row, in relation order) with `function`.
///
/// - `Count`: number of values (related rows), empty or not
/// - `CountValues`: number of non-empty values
/// - `Sum`: sum of the numeric values (lists are flattened, non-numbers skipped); `0` when none
/// - `Average`: mean of the numeric values; `Null` when none
/// - `Min`/`Max`: smallest/largest number; when there are no numbers, the earliest/latest date;
///   `Null` when there is neither
/// - `ShowOriginal`: all non-empty values (lists flattened) as a list
/// - `PercentChecked`: share of values that are checked, `0..=100`; `Null` when there are no rows
/// - `Earliest`/`Latest`: the earliest/latest date (a date-time keeps its time); `Null` when none
pub fn rollup(function: RollupFn, values: &[Value]) -> Value {
    match function {
        RollupFn::Count => Value::Number(values.len() as f64),
        RollupFn::CountValues => {
            Value::Number(values.iter().filter(|v| !v.is_empty()).count() as f64)
        }
        RollupFn::Sum => Value::Number(numbers(values).iter().sum()),
        RollupFn::Average => {
            let nums = numbers(values);
            if nums.is_empty() {
                Value::Null
            } else {
                Value::Number(nums.iter().sum::<f64>() / nums.len() as f64)
            }
        }
        RollupFn::Min => extreme(values, true),
        RollupFn::Max => extreme(values, false),
        RollupFn::ShowOriginal => Value::List(
            flatten(values)
                .into_iter()
                .filter(|v| !v.is_empty())
                .collect(),
        ),
        RollupFn::PercentChecked => {
            if values.is_empty() {
                return Value::Null;
            }
            let checked = values.iter().filter(|v| v.as_bool() == Some(true)).count();
            Value::Number(checked as f64 * 100.0 / values.len() as f64)
        }
        RollupFn::Earliest => date_extreme(values, true),
        RollupFn::Latest => date_extreme(values, false),
    }
}

fn flatten(values: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(values.len());
    for v in values {
        match v {
            Value::List(items) => out.extend(flatten(items)),
            other => out.push(other.clone()),
        }
    }
    out
}

fn numbers(values: &[Value]) -> Vec<f64> {
    flatten(values)
        .iter()
        .filter(|v| !matches!(v, Value::Bool(_)))
        .filter_map(Value::as_number)
        .collect()
}

fn extreme(values: &[Value], smallest: bool) -> Value {
    let nums = numbers(values);
    let best = nums
        .into_iter()
        .reduce(|a, b| if (b < a) == smallest { b } else { a });
    match best {
        Some(n) => Value::Number(n),
        None => date_extreme(values, smallest),
    }
}

fn date_extreme(values: &[Value], earliest: bool) -> Value {
    let mut best: Option<(chrono::NaiveDateTime, Value)> = None;
    for v in flatten(values) {
        if !matches!(v, Value::Date(_) | Value::DateTime(_) | Value::Text(_)) {
            continue;
        }
        let Some(dt) = v.as_datetime() else { continue };
        let replace = match &best {
            None => true,
            Some((cur, _)) => (dt < *cur) == earliest && dt != *cur,
        };
        if replace {
            let kept = match &v {
                Value::Text(s) if super::value::parse_datetime(s).is_some() => Value::DateTime(dt),
                Value::Text(_) => Value::Date(dt.date()),
                _ => v.clone(),
            };
            best = Some((dt, kept));
        }
    }
    best.map_or(Value::Null, |(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn d(day: u32) -> Value {
        Value::Date(NaiveDate::from_ymd_opt(2026, 3, day).unwrap())
    }

    fn nums(v: &[f64]) -> Vec<Value> {
        v.iter().map(|n| Value::Number(*n)).collect()
    }

    #[test]
    fn counts() {
        let v = vec![
            Value::from("a"),
            Value::Null,
            Value::from(" "),
            Value::Number(0.0),
        ];
        assert_eq!(rollup(RollupFn::Count, &v), Value::Number(4.0));
        assert_eq!(rollup(RollupFn::CountValues, &v), Value::Number(2.0));
        assert_eq!(rollup(RollupFn::Count, &[]), Value::Number(0.0));
    }

    #[test]
    fn sum_average_skip_non_numbers() {
        let v = vec![
            Value::Number(1.0),
            Value::from("2"),
            Value::from("x"),
            Value::Null,
            Value::List(nums(&[3.0])),
        ];
        assert_eq!(rollup(RollupFn::Sum, &v), Value::Number(6.0));
        assert_eq!(rollup(RollupFn::Average, &v), Value::Number(2.0));
        assert_eq!(rollup(RollupFn::Sum, &[]), Value::Number(0.0));
        assert_eq!(rollup(RollupFn::Average, &[Value::Null]), Value::Null);
    }

    #[test]
    fn min_max() {
        let v = nums(&[3.0, -1.0, 2.0]);
        assert_eq!(rollup(RollupFn::Min, &v), Value::Number(-1.0));
        assert_eq!(rollup(RollupFn::Max, &v), Value::Number(3.0));
        let dates = vec![d(5), d(2), d(9)];
        assert_eq!(rollup(RollupFn::Min, &dates), d(2));
        assert_eq!(rollup(RollupFn::Max, &dates), d(9));
        assert_eq!(rollup(RollupFn::Max, &[Value::Null]), Value::Null);
    }

    #[test]
    fn show_original_flattens_and_drops_empties() {
        let v = vec![
            Value::List(vec!["a".into(), "b".into()]),
            Value::Null,
            "c".into(),
        ];
        assert_eq!(
            rollup(RollupFn::ShowOriginal, &v),
            Value::List(vec!["a".into(), "b".into(), "c".into()])
        );
    }

    #[test]
    fn percent_checked() {
        let v = vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::from("yes"),
            Value::Null,
        ];
        assert_eq!(rollup(RollupFn::PercentChecked, &v), Value::Number(50.0));
        assert_eq!(rollup(RollupFn::PercentChecked, &[]), Value::Null);
    }

    #[test]
    fn earliest_and_latest() {
        let v = vec![
            d(5),
            Value::Null,
            d(2),
            Value::from("2026-03-09"),
            Value::Number(4.0),
        ];
        assert_eq!(rollup(RollupFn::Earliest, &v), d(2));
        assert_eq!(rollup(RollupFn::Latest, &v), d(9));
        assert_eq!(rollup(RollupFn::Earliest, &[Value::Null]), Value::Null);
        let with_time = Value::DateTime(
            NaiveDate::from_ymd_opt(2026, 3, 1)
                .unwrap()
                .and_hms_opt(8, 15, 0)
                .unwrap(),
        );
        assert_eq!(
            rollup(RollupFn::Earliest, &[d(5), with_time.clone()]),
            with_time
        );
    }
}
