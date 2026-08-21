// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `ser::SerializeMap` for [`MapSerializer`]: buffered, since the entries' order is known only
//! once every key is encoded and sorted.

use serde::{ser, Serialize};

use super::{encode_to_vec, put_len, Error, MapSerializer, Sink};

impl<S: Sink> ser::SerializeMap for MapSerializer<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        self.pending_key = Some(encode_to_vec(key)?);
        Ok(())
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        let key = self
            .pending_key
            .take()
            .ok_or_else(|| <Error as ser::Error>::custom("map value serialized before its key"))?;
        self.entries.push((key, encode_to_vec(value)?));
        Ok(())
    }

    fn end(mut self) -> Result<(), Error> {
        // Sort on the encoded key: canonical without an `Ord` bound.
        self.entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        put_len(self.sink, self.entries.len() as u64);
        for (key, value) in &self.entries {
            self.sink.put(key);
            self.sink.put(value);
        }
        Ok(())
    }
}
