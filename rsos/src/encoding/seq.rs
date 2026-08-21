// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `ser::SerializeSeq` for [`SeqSerializer`]: buffered when the length is unknown up front, so
//! the count still precedes the elements.

use serde::{ser, Serialize};

use super::{encode_into, put_len, Error, SeqSerializer, Sink};

impl<S: Sink> ser::SerializeSeq for SeqSerializer<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        match self {
            SeqSerializer::Streaming(sink) => encode_into(*sink, value),
            SeqSerializer::Buffered { buf, count, .. } => {
                *count += 1;
                encode_into(buf, value)
            }
        }
    }

    fn end(self) -> Result<(), Error> {
        match self {
            SeqSerializer::Streaming(_) => Ok(()),
            SeqSerializer::Buffered {
                sink, buf, count, ..
            } => {
                put_len(sink, count);
                sink.put(&buf);
                Ok(())
            }
        }
    }
}
