use std::cell::RefCell;
use std::collections::HashMap;

use bytes::{BufMut, Bytes, BytesMut};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use serde_json::Error;

pub const CONTENT_TYPE: &str = "application/json";

const DEFAULT_JSON_CAPACITY: usize = 256;
const MAX_CACHED_JSON_CAPACITY: usize = 256 * 1024;

thread_local! {
    static JSON_CAPACITY_HINTS: RefCell<HashMap<&'static str, usize>> =
        RefCell::new(HashMap::new());
}

/// Serialize a value to JSON as `Bytes`, writing directly into a `BytesMut` buffer.
pub fn serialize<T: Serialize>(value: &T) -> Result<Bytes, Error> {
    let type_name = std::any::type_name::<T>();
    let capacity = json_capacity_hint(type_name);
    let mut buf = BytesMut::with_capacity(capacity);
    serde_json::to_writer((&mut buf).writer(), value)?;
    let bytes = buf.freeze();
    update_json_capacity_hint(type_name, bytes.len());
    Ok(bytes)
}

fn json_capacity_hint(type_name: &'static str) -> usize {
    JSON_CAPACITY_HINTS.with(|hints| {
        hints
            .borrow()
            .get(type_name)
            .copied()
            .unwrap_or(DEFAULT_JSON_CAPACITY)
    })
}

fn update_json_capacity_hint(type_name: &'static str, len: usize) {
    let next_hint = next_json_capacity_hint(len);
    JSON_CAPACITY_HINTS.with(|hints| {
        let mut hints = hints.borrow_mut();
        hints
            .entry(type_name)
            .and_modify(|hint| *hint = (*hint).max(next_hint))
            .or_insert(next_hint);
    });
}

fn next_json_capacity_hint(len: usize) -> usize {
    len.max(DEFAULT_JSON_CAPACITY)
        .next_power_of_two()
        .min(MAX_CACHED_JSON_CAPACITY)
}

/// Deserialize a value from a JSON byte slice.
pub fn deserialize<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Sample {
        name: String,
        value: u32,
    }

    #[test]
    fn round_trip() {
        let input = Sample {
            name: "test".into(),
            value: 42,
        };
        let bytes = serialize(&input).unwrap();
        let output: Sample = deserialize(&bytes).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn serialize_produces_valid_json() {
        let input = Sample {
            name: "hello".into(),
            value: 1,
        };
        let bytes = serialize(&input).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["name"], "hello");
        assert_eq!(parsed["value"], 1);
    }

    #[test]
    fn deserialize_error_on_invalid_input() {
        let result = deserialize::<Sample>(b"not json");
        assert!(result.is_err());
    }

    #[test]
    fn content_type_is_correct() {
        assert_eq!(CONTENT_TYPE, "application/json");
    }

    #[derive(Serialize)]
    struct LargeSample {
        users: Vec<Sample>,
    }

    #[test]
    fn capacity_hint_grows_for_repeated_large_type() {
        let input = LargeSample {
            users: (0..128)
                .map(|i| Sample {
                    name: format!("user-{i}"),
                    value: i,
                })
                .collect(),
        };

        let type_name = std::any::type_name::<LargeSample>();
        assert_eq!(json_capacity_hint(type_name), DEFAULT_JSON_CAPACITY);

        let bytes = serialize(&input).unwrap();
        let hint = json_capacity_hint(type_name);

        assert!(hint >= bytes.len());
        assert!(hint > DEFAULT_JSON_CAPACITY);

        let bytes_again = serialize(&input).unwrap();
        assert_eq!(bytes, bytes_again);
        assert_eq!(json_capacity_hint(type_name), hint);
    }
}
