#![cfg(feature = "serde")]

use adamantium::Bytes;
use serde::{Deserialize, Deserializer, de};

struct OwnedBytesDeserializer(Vec<u8>);

impl<'de> Deserializer<'de> for OwnedBytesDeserializer {
    type Error = de::value::Error;

    fn deserialize_any<V: de::Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom("expected deserialize_byte_buf"))
    }

    fn deserialize_byte_buf<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_byte_buf(self.0)
    }

    fn is_human_readable(&self) -> bool {
        false
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes
        option unit unit_struct newtype_struct seq tuple tuple_struct map struct
        enum identifier ignored_any
    }
}

#[test]
fn bytes_deserialize_transient_byte_arrays() {
    for bytes in [&[1, 2, 3][..], &[][..], &[0, 255][..]] {
        let de = de::value::BytesDeserializer::<de::value::Error>::new(bytes);
        assert_eq!(Bytes::deserialize(de), Ok(Bytes::from(bytes)));
    }
}

#[test]
fn bytes_deserialize_borrowed_byte_arrays() {
    for bytes in [&[][..], &[1, 2, 3][..], &[0, 255][..]] {
        let de = de::value::BorrowedBytesDeserializer::<de::value::Error>::new(bytes);
        assert_eq!(Bytes::deserialize(de), Ok(Bytes::from(bytes)));
    }
}

#[test]
fn bytes_deserialize_owned_byte_buffers() {
    for bytes in [vec![], vec![1, 2, 3], vec![0, 255]] {
        let expected = Bytes::from(bytes.clone());
        let original_allocation = bytes.as_ptr();
        let decoded = Bytes::deserialize(OwnedBytesDeserializer(bytes)).unwrap();
        assert_eq!(decoded, expected);
        assert_eq!(decoded.as_ptr(), original_allocation);
    }
}

#[test]
fn bytes_deserialize_sequences() {
    for bytes in [vec![], vec![1, 2, 3], vec![0, 255]] {
        let de = de::value::SeqDeserializer::<_, de::value::Error>::new(bytes.iter().copied());
        assert_eq!(Bytes::deserialize(de), Ok(Bytes::from(bytes)));
    }
}

#[test]
fn bytes_keep_json_representation() {
    for json in ["[]", "[1,2,3]", "[0,255]"] {
        let bytes: Bytes = serde_json::from_str(json).unwrap();
        assert_eq!(
            serde_json::from_slice::<Bytes>(json.as_bytes()).unwrap(),
            bytes
        );
        assert_eq!(
            serde_json::from_value::<Bytes>(serde_json::from_str(json).unwrap()).unwrap(),
            bytes
        );
        assert_eq!(serde_json::to_string(&bytes).unwrap(), json);
    }
    for json in [
        "[-1]",
        "[256]",
        "[1.5]",
        "[null]",
        "null",
        "\"abc\"",
        "\"aGVsbG8=\"",
    ] {
        assert!(serde_json::from_str::<Bytes>(json).is_err(), "{json}");
        assert!(
            serde_json::from_slice::<Bytes>(json.as_bytes()).is_err(),
            "{json}"
        );
        assert!(
            serde_json::from_value::<Bytes>(serde_json::from_str(json).unwrap()).is_err(),
            "{json}"
        );
    }
}

#[test]
fn bytes_deserialize_yaml_sequences() {
    for yaml in ["[]", "[1, 2, 3]", "[0, 255]"] {
        assert_eq!(
            serde_yaml::from_str::<Bytes>(yaml).unwrap(),
            serde_json::from_str::<Bytes>(yaml).unwrap()
        );
    }
}

#[test]
fn bytes_round_trip_through_cbor_byte_strings() {
    for bytes in [Bytes::new(), Bytes::from([1, 2, 3]), Bytes::from([0, 255])] {
        let encoded = cbor4ii::serde::to_vec(Vec::new(), &bytes).unwrap();
        assert_eq!(encoded[0] >> 5, 2);
        let decoded: Bytes = cbor4ii::serde::from_slice(&encoded).unwrap();
        assert_eq!(decoded, bytes);
    }
}
