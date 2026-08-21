// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `ser::Serializer` for [`Serializer`]: every scalar, plus the entry point into each compound
//! form (which of [`SeqSerializer`], [`Streaming`] or [`MapSerializer`] continues the encoding).

use serde::{ser, Serialize};

use super::{
    encode_into, put_len, put_variant, Error, MapSerializer, SeqSerializer, Serializer, Sink,
    Streaming,
};

impl<'a, S: Sink> ser::Serializer for Serializer<'a, S> {
    type Ok = ();
    type Error = Error;

    type SerializeSeq = SeqSerializer<'a, S>;
    type SerializeTuple = Streaming<'a, S>;
    type SerializeTupleStruct = Streaming<'a, S>;
    type SerializeTupleVariant = Streaming<'a, S>;
    type SerializeMap = MapSerializer<'a, S>;
    type SerializeStruct = Streaming<'a, S>;
    type SerializeStructVariant = Streaming<'a, S>;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn serialize_bool(self, v: bool) -> Result<(), Error> {
        self.sink.put(&[u8::from(v)]);
        Ok(())
    }

    fn serialize_i8(self, v: i8) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_i16(self, v: i16) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_i32(self, v: i32) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_i64(self, v: i64) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_i128(self, v: i128) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u8(self, v: u8) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u16(self, v: u16) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u32(self, v: u32) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u64(self, v: u64) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u128(self, v: u128) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_f32(self, v: f32) -> Result<(), Error> {
        self.sink.put(&v.to_bits().to_le_bytes());
        Ok(())
    }

    fn serialize_f64(self, v: f64) -> Result<(), Error> {
        self.sink.put(&v.to_bits().to_le_bytes());
        Ok(())
    }

    fn serialize_char(self, v: char) -> Result<(), Error> {
        self.sink.put(&(v as u32).to_le_bytes());
        Ok(())
    }

    fn serialize_str(self, v: &str) -> Result<(), Error> {
        self.serialize_bytes(v.as_bytes())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<(), Error> {
        put_len(self.sink, v.len() as u64);
        self.sink.put(v);
        Ok(())
    }

    fn serialize_none(self) -> Result<(), Error> {
        self.sink.put(&[0]);
        Ok(())
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), Error> {
        self.sink.put(&[1]);
        encode_into(self.sink, value)
    }

    fn serialize_unit(self) -> Result<(), Error> {
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Error> {
        Ok(())
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
    ) -> Result<(), Error> {
        put_variant(self.sink, variant_index);
        Ok(())
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        encode_into(self.sink, value)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        put_variant(self.sink, variant_index);
        encode_into(self.sink, value)
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<SeqSerializer<'a, S>, Error> {
        match len {
            Some(len) => {
                put_len(self.sink, len as u64);
                Ok(SeqSerializer::Streaming(self.sink))
            }
            None => Ok(SeqSerializer::Buffered {
                sink: self.sink,
                buf: Vec::new(),
                count: 0,
            }),
        }
    }

    fn serialize_tuple(self, len: usize) -> Result<Streaming<'a, S>, Error> {
        put_len(self.sink, len as u64);
        Ok(Streaming { sink: self.sink })
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Streaming<'a, S>, Error> {
        put_len(self.sink, len as u64);
        Ok(Streaming { sink: self.sink })
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        len: usize,
    ) -> Result<Streaming<'a, S>, Error> {
        put_variant(self.sink, variant_index);
        put_len(self.sink, len as u64);
        Ok(Streaming { sink: self.sink })
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<MapSerializer<'a, S>, Error> {
        Ok(MapSerializer {
            sink: self.sink,
            entries: Vec::new(),
            pending_key: None,
        })
    }

    fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Streaming<'a, S>, Error> {
        Ok(Streaming { sink: self.sink })
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Streaming<'a, S>, Error> {
        put_variant(self.sink, variant_index);
        Ok(Streaming { sink: self.sink })
    }
}
