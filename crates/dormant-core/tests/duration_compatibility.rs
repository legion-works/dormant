//! Duration compatibility checks against measured parser and formatter results.
//!
//! One overflow input has an intentional difference: `u64::MAX` seconds plus
//! one billion nanoseconds returns `NumberOverflow` instead of a panic.

use std::time::Duration;

use dormant_core::duration::{format_duration, parse_duration};
use serde_json::{Value, json};

#[test]
fn duration_normalization_overflow_returns_error() {
    assert_eq!(
        parse_duration("18446744073709551615s 1000000000ns"),
        Err(dormant_core::duration::Error::NumberOverflow)
    );
}

proptest::proptest! {
    #[test]
    fn duration_text_roundtrip(secs in proptest::num::u64::ANY, nanos in 0_u32..1_000_000_000) {
        let duration = Duration::new(secs, nanos);
        proptest::prop_assert_eq!(parse_duration(&format_duration(duration)), Ok(duration));
    }
}

#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Required {
    #[serde(with = "dormant_core::duration::serde")]
    duration: Duration,
}

#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Optional {
    #[serde(default, with = "dormant_core::duration::option")]
    duration: Option<Duration>,
}

#[test]
fn duration_serde_golden_table() {
    let table: Value = serde_json::from_str(include_str!("fixtures/duration_golden.json")).unwrap();
    for row in table["parse"].as_array().unwrap() {
        let value = json!({"duration": row["input"]});
        let required = serde_json::from_value::<Required>(value.clone());
        let optional = serde_json::from_value::<Optional>(value);
        if let Some(secs) = row["result"]["secs"].as_u64() {
            let expected = Duration::new(
                secs,
                row["result"]["nanos"].as_u64().unwrap().try_into().unwrap(),
            );
            assert_eq!(required.unwrap().duration, expected, "{row}");
            assert_eq!(optional.unwrap().duration, Some(expected), "{row}");
        } else {
            assert!(
                required
                    .unwrap_err()
                    .to_string()
                    .contains("expected a duration"),
                "{row}"
            );
            assert!(
                optional
                    .unwrap_err()
                    .to_string()
                    .contains("expected a duration"),
                "{row}"
            );
        }
    }
    for row in table["format"].as_array().unwrap() {
        let duration = Duration::new(
            row["secs"].as_u64().unwrap(),
            row["nanos"].as_u64().unwrap().try_into().unwrap(),
        );
        let expected = json!({"duration": row["output"]});
        assert_eq!(
            serde_json::to_value(Required { duration }).unwrap(),
            expected
        );
        assert_eq!(
            serde_json::to_value(Optional {
                duration: Some(duration)
            })
            .unwrap(),
            expected
        );
    }
    assert_eq!(
        serde_json::to_value(Optional { duration: None }).unwrap(),
        json!({"duration": null})
    );
    assert_eq!(
        serde_json::from_value::<Optional>(json!({}))
            .unwrap()
            .duration,
        None
    );
    assert_eq!(
        serde_json::from_value::<Optional>(json!({"duration": null}))
            .unwrap()
            .duration,
        None
    );
    for value in [
        json!(null),
        json!(1),
        json!(1.5),
        json!(true),
        json!([]),
        json!({}),
    ] {
        assert!(serde_json::from_value::<Required>(json!({"duration": value})).is_err());
        if !value.is_null() {
            assert!(serde_json::from_value::<Optional>(json!({"duration": value})).is_err());
        }
    }
}

#[test]
fn duration_golden_table() {
    let table: Value = serde_json::from_str(include_str!("fixtures/duration_golden.json")).unwrap();
    assert_eq!(table["parse"].as_array().unwrap().len(), 1610);
    assert_eq!(table["format"].as_array().unwrap().len(), 260);
    for row in table["parse"].as_array().unwrap() {
        let input = row["input"].as_str().unwrap();
        let actual = match parse_duration(input) {
            Ok(d) => json!({"secs": d.as_secs(), "nanos": d.subsec_nanos()}),
            Err(e) => json!({"error": format!("{e:?}"), "message": e.to_string()}),
        };
        assert_eq!(actual, row["result"], "parse {input:?}");
    }
    for row in table["format"].as_array().unwrap() {
        let duration = Duration::new(
            row["secs"].as_u64().unwrap(),
            row["nanos"].as_u64().unwrap().try_into().unwrap(),
        );
        assert_eq!(
            format_duration(duration),
            row["output"].as_str().unwrap(),
            "format {duration:?}"
        );
    }
}
