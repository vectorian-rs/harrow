use bytes::Bytes;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use rmp_serde::decode::Error as DecodeError;
pub use rmp_serde::encode::Error as EncodeError;

pub const CONTENT_TYPE: &str = "application/msgpack";

/// Serialize a value to MessagePack as `Bytes`.
pub fn serialize(value: &impl Serialize) -> Result<Bytes, EncodeError> {
    let vec = rmp_serde::to_vec(value)?;
    Ok(Bytes::from(vec))
}

/// Deserialize a value from a MessagePack byte slice.
pub fn deserialize<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
    rmp_serde::from_slice(bytes)
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
    fn serialize_produces_bytes() {
        let input = Sample {
            name: "hello".into(),
            value: 1,
        };
        let bytes = serialize(&input).unwrap();
        assert!(!bytes.is_empty());
        // MessagePack is binary, not valid UTF-8 text in general
        let output: Sample = deserialize(&bytes).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn deserialize_error_on_invalid_input() {
        let result = deserialize::<Sample>(b"not msgpack");
        assert!(result.is_err());
    }

    #[test]
    fn content_type_is_correct() {
        assert_eq!(CONTENT_TYPE, "application/msgpack");
    }
}
