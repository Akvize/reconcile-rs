// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `ser::SerializeTuple`/`SerializeTupleStruct`/`SerializeTupleVariant`/`SerializeStruct`/
//! `SerializeStructVariant` for [`Streaming`]: elements/fields straight through, no framing —
//! the entry point that produced a [`Streaming`] already wrote whatever framing that compound
//! form needs (a length, a variant index, or both).

use serde::{ser, Serialize};

use super::{encode_into, Error, Sink, Streaming};

impl<S: Sink> ser::SerializeTuple for Streaming<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        encode_into(self.sink, value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeTupleStruct for Streaming<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        encode_into(self.sink, value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeTupleVariant for Streaming<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        encode_into(self.sink, value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeStruct for Streaming<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        _key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        encode_into(self.sink, value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeStructVariant for Streaming<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        _key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        encode_into(self.sink, value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}
