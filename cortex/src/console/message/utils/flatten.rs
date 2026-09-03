//! Writing one value's members into an object somebody else already opened.
//!
//! A JSON-RPC object is flat: `method` and `params` are siblings of `jsonrpc` and `id`,
//! not members of something nested under them. A derived [`Serialize`] is not flat — it
//! opens an object of its own and writes into that. [`FlatMapSerializer`] is the join
//! between the two: a [`Serializer`] that accepts exactly the values with members to
//! give — a map or a struct — and forwards each entry into a [`SerializeMap`] that is
//! already being filled.
//!
//! What that buys is where a shape gets to be declared. [`Call`](super::super::Call)
//! says what it puts on the wire with an attribute on itself, which is the one place a
//! reader looks for it, and still lands beside `jsonrpc` rather than under a member.
//! Without this, the choice would be between a wire that nests to suit serde and a
//! hand-written arm per method in the envelope.
//!
//! serde has one of these and keeps it in `__private`, which is the whole reason this
//! module exists. It is not a reimplementation of anything subtle: two traits forwarding
//! to a parent, and every other shape refused.
//!
//! # Why the rest is an error
//!
//! A number, a string or a sequence has no members, so there is nothing it could
//! contribute to an open object. Each is a method here that returns an error rather than
//! one that quietly nests, because a type reaching one cannot be written flat at all —
//! and a silent nesting would put it on the wire in a shape nothing is looking for.
//!
//! # Nothing is buffered
//!
//! Entries stream into the parent as the derive produces them. The alternative — build
//! the value into a document, then take it apart into the parent — costs a full copy of
//! whatever it carried, and what this channel carries is up to [`MAX_PAYLOAD`](super::super::MAX_PAYLOAD) of file
//! or output bytes.

use serde::{
    Serialize, Serializer,
    ser::{self, Impossible, SerializeMap, SerializeStruct},
};

/// Writes a value's members into `.0` rather than into an object of its own.
///
/// Held as `&mut` because the parent keeps filling its object afterwards: an `id` is
/// already in there, and the envelope's `end` is still to come.
pub struct FlatMapSerializer<'a, M>(pub &'a mut M);

/// What every shape that is not a map or a struct gets.
fn not_flat<E: ser::Error>() -> E {
    E::custom("only a map or a struct has members to write into an open object")
}

/// The one-line refusals, which are the bulk of [`Serializer`] and say nothing
/// individually. Spelled by a macro so that what is written out is what does
/// something.
macro_rules! refuse {
    ($($method:ident($($arg:ty),*);)*) => {
        $(fn $method(self $(, _: $arg)*) -> Result<Self::Ok, Self::Error> {
            Err(not_flat())
        })*
    };
}

impl<'a, M: SerializeMap> Serializer for FlatMapSerializer<'a, M> {
    type Ok = ();
    type Error = M::Error;

    /// Both are the same forwarder: a struct's fields and a map's entries are the
    /// same thing once they are in the parent's object.
    type SerializeMap = FlatMap<'a, M>;
    type SerializeStruct = FlatMap<'a, M>;

    type SerializeSeq = Impossible<(), M::Error>;
    type SerializeTuple = Impossible<(), M::Error>;
    type SerializeTupleStruct = Impossible<(), M::Error>;
    type SerializeTupleVariant = Impossible<(), M::Error>;
    type SerializeStructVariant = Impossible<(), M::Error>;

    refuse! {
        serialize_bool(bool);
        serialize_i8(i8);
        serialize_i16(i16);
        serialize_i32(i32);
        serialize_i64(i64);
        serialize_u8(u8);
        serialize_u16(u16);
        serialize_u32(u32);
        serialize_u64(u64);
        serialize_f32(f32);
        serialize_f64(f64);
        serialize_char(char);
        serialize_str(&str);
        serialize_bytes(&[u8]);
        serialize_none();
        serialize_unit();
        serialize_unit_struct(&'static str);
        serialize_unit_variant(&'static str, u32, &'static str);
    }

    /// A map is what this is for: its entries are the parent's.
    ///
    /// The length is dropped rather than passed on — the parent's object was opened
    /// with a length of its own, and is longer than this by whatever it had already
    /// written.
    fn serialize_map(self, _len: Option<usize>) -> Result<FlatMap<'a, M>, M::Error> {
        Ok(FlatMap(self.0))
    }

    /// A struct is a map whose keys are known at compile time, and flattens the
    /// same. This is the one a derive reaches for.
    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<FlatMap<'a, M>, M::Error> {
        Ok(FlatMap(self.0))
    }

    /// Transparent: a `Some` contributes whatever it holds.
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<(), M::Error> {
        value.serialize(self)
    }

    /// Transparent for the same reason: a newtype struct is its inner value with a
    /// name on it, and the name is not a member.
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), M::Error> {
        value.serialize(self)
    }

    /// Not transparent, unlike the two above: a newtype *variant* is a member — the
    /// variant name keys the value — so which member it should become is a question
    /// this cannot answer. An enum meant to flatten says so with `tag` and `content`
    /// and arrives at [`serialize_struct`](Self::serialize_struct) instead.
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<(), M::Error> {
        Err(not_flat())
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, M::Error> {
        Err(not_flat())
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, M::Error> {
        Err(not_flat())
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, M::Error> {
        Err(not_flat())
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, M::Error> {
        Err(not_flat())
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, M::Error> {
        Err(not_flat())
    }
}

/// The parent's object, being written into by somebody who thinks it is their own.
///
/// `end` closes nothing, which is the whole of what makes this flat: the object
/// stays open for whoever opened it, and only they may end it.
pub struct FlatMap<'a, M>(&'a mut M);

impl<M: SerializeMap> SerializeMap for FlatMap<'_, M> {
    type Ok = ();
    type Error = M::Error;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), M::Error> {
        self.0.serialize_key(key)
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), M::Error> {
        self.0.serialize_value(value)
    }

    fn end(self) -> Result<(), M::Error> {
        Ok(())
    }
}

impl<M: SerializeMap> SerializeStruct for FlatMap<'_, M> {
    type Ok = ();
    type Error = M::Error;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), M::Error> {
        self.0.serialize_entry(key, value)
    }

    /// A skipped field is one the parent never hears about — which is how a variant
    /// carrying nothing gets no `params` rather than a null one.
    fn skip_field(&mut self, _key: &'static str) -> Result<(), M::Error> {
        Ok(())
    }

    fn end(self) -> Result<(), M::Error> {
        Ok(())
    }
}
