use bytes::{BufMut, Bytes, BytesMut};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use serde_json::Error;

pub const CONTENT_TYPE: &str = "application/json";

/// Serialize a value to JSON as `Bytes`, writing directly into a `BytesMut` buffer.
pub fn serialize(value: &impl Serialize) -> Result<Bytes, Error> {
    let mut buf = BytesMut::with_capacity(128);
    serde_json::to_writer((&mut buf).writer(), value)?;
    Ok(buf.freeze())
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
}
