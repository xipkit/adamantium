#![cfg(feature = "serde")]

use adamantium::Bytes;
use serde::{Deserialize, Deserializer, de};

struct OwnedBytesDeserializer(Vec<u8>);

impl<'de> Deserializer<'de> for OwnedBytesDeserializer {
    type Error = de::value::Error;

    fn deserialize_any<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_byte_buf(self.0)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf
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
        assert_eq!(serde_json::to_string(&bytes).unwrap(), json);
    }
    for json in ["[-1]", "[256]", "[1.5]", "[null]", "null"] {
        assert!(serde_json::from_str::<Bytes>(json).is_err(), "{json}");
    }
}

#[test]
fn bytes_round_trip_through_cbor_byte_strings() {
    for bytes in [Bytes::new(), Bytes::from([1, 2, 3]), Bytes::from([0, 255])] {
        let mut encoded = Vec::new();
        ciborium::into_writer(&bytes, &mut encoded).unwrap();
        let value: ciborium::Value = ciborium::from_reader(encoded.as_slice()).unwrap();
        assert_eq!(value, ciborium::Value::Bytes(bytes.as_slice().to_vec()));
        let decoded: Bytes = ciborium::from_reader(encoded.as_slice()).unwrap();
        assert_eq!(decoded, bytes);
    }
}
