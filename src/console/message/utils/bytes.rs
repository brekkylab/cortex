//! Raw bytes, as themselves.
//!
//! BSON has a byte type — `Binary`, subtype `Generic` — so the bulk of what this channel
//! carries goes across at 1.0× rather than as text. This is the whole reason the codec is
//! BSON and not JSON: JSON has no byte type, so `stdout` had to be base64 (1.37×) to
//! avoid being an array of numbers (`[104,105,10]`, 4×).
//!
//! A module of its own because the types that carry bytes are on both sides of the
//! exchange — an [`ExecResp`](super::super::ExecResp)'s output and a [`ReadResp`](super::super::ReadResp)'s
//! data going one way, a [`WriteCall`](super::super::WriteCall)'s the other — and how bytes reach the
//! wire is the codec's business rather than either half's.
//!
//! # Why this does not ask the codec
//!
//! [`is_human_readable`](serde::Serializer::is_human_readable) is the obvious way for one
//! helper to serve a textual codec and a binary one, and it cannot be used here, because
//! it silently loses. A message's `params` and `result` are held as a
//! [`Bson`](bson::Bson) before they reach the wire — they have to be, since a `result` is
//! typed by a method only the caller knows — and `bson`'s *value-level* serializer
//! reports `is_human_readable() == true`. So the branch would encode base64 on the way
//! into the `Bson`, and the wire would faithfully carry a string: 1.37×, the byte type
//! unused, and nothing to show it had happened. `bson`'s `SerializerOptions` is
//! `pub(crate)`, so it cannot be told otherwise.
//!
//! One codec, and it has bytes. If a textual wire is ever wanted for a person to read,
//! BSON's own projection is the thing to reach for — `Bson::into_relaxed_extjson` spells
//! `Binary` as `{"$binary": ..}` — rather than a second spelling in here.

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

    /// A peer that spelled its bytes as an array is read rather than refused: BSON
    /// has arrays too, and `[104,105,10]` is unambiguous even though nothing here
    /// writes it.
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
        let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(b) = seq.next_element()? {
            out.push(b);
        }
        Ok(out)
    }
}
