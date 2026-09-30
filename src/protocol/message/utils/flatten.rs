//! Writing one value's members into an object someone else already opened.

use serde::{
    Serialize, Serializer,
    ser::{self, Impossible, SerializeMap, SerializeStruct},
};

/// Writes a value's members into `.0` rather than into an object of its own.
///
/// A JSON-RPC object is flat (`method` and `params` sit beside `jsonrpc` and `id`), but a
/// derived [`Serialize`] opens its own object; forwarding each entry into the open
/// [`SerializeMap`] lets [`Call`](super::super::Call) declare its wire shape with serde
/// attributes and still land flat. serde's equivalent is in `__private`.
///
/// Only a map or struct is accepted: any other shape has no members, and nesting it would
/// be a shape nothing reads. Entries stream straight into the parent; building a document
/// first would copy up to [`MAX_PAYLOAD`](super::super::MAX_PAYLOAD) of payload.
///
/// `&mut` because the parent keeps writing into its object and ends it itself.
pub struct FlatMapSerializer<'a, M>(pub &'a mut M);

/// What every shape that is not a map or a struct gets.
fn not_flat<E: ser::Error>() -> E {
    E::custom("only a map or a struct has members to write into an open object")
}

/// The one-line refusals that make up most of [`Serializer`].
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

    /// One forwarder: struct fields and map entries are the same once in the parent.
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

    /// The length is dropped: the parent's object already holds other entries.
    fn serialize_map(self, _len: Option<usize>) -> Result<FlatMap<'a, M>, M::Error> {
        Ok(FlatMap(self.0))
    }

    /// What a derive reaches for; flattens like a map.
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

    /// Transparent: the newtype's name is not a member.
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), M::Error> {
        value.serialize(self)
    }

    /// Refused: the variant name would key the value, so there is no member set to
    /// flatten. An enum meant to flatten uses `tag` and `content` and arrives at
    /// [`serialize_struct`](Self::serialize_struct).
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

/// The parent's object, written into as if it were the child's own.
///
/// `end` closes nothing: only the parent may end its object.
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

    /// A skipped field is omitted, so a variant carrying nothing gets no `params` rather
    /// than a null one.
    fn skip_field(&mut self, _key: &'static str) -> Result<(), M::Error> {
        Ok(())
    }

    fn end(self) -> Result<(), M::Error> {
        Ok(())
    }
}
