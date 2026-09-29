//! Raw bytes as BSON `Binary` (subtype `Generic`), at 1.0× rather than base64's 1.37×.
//!
//! # Why this does not ask the codec
//!
//! Branching on [`is_human_readable`](serde::Serializer::is_human_readable) would silently
//! produce base64: `params` and `result` pass through a [`Bson`](bson::Bson) value first,
//! and `bson`'s value-level serializer reports itself human-readable (its
//! `SerializerOptions` is `pub(crate)`, so that cannot be changed).
//!
//! For a human-readable view, use `Bson::into_relaxed_extjson`, which spells `Binary` as
//! `{"$binary": ..}`.

use serde::{
    Deserializer, Serializer,
    de::{SeqAccess, Visitor},
};

pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_bytes(bytes)
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    d.deserialize_byte_buf(Raw)
}

struct Raw;

impl<'de> Visitor<'de> for Raw {
    type Value = Vec<u8>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("bytes")
    }

    fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
        Ok(v.to_vec())
    }

    fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
        Ok(v)
    }

    /// Bytes spelled as an array (`[104,105,10]`) are accepted: unambiguous, though
    /// nothing here writes them.
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
        let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(b) = seq.next_element()? {
            out.push(b);
        }
        Ok(out)
    }
}
