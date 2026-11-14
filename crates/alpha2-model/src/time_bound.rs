//! Finds a `MediaTime` past `MAX_SERIALIZED_TICKS` by walking a value's own serialization, so
//! no list of time fields can fall out of date.

use alpha2_time::MAX_SERIALIZED_TICKS;
use serde::ser::{self, Serialize};
use std::fmt;

/// Refuses the first time in `value` past [`MAX_SERIALIZED_TICKS`] either way. A value whose
/// serialization fails otherwise has no time to name; its writer reports it.
pub(crate) fn ensure_times_are_writable(value: &impl Serialize) -> Result<(), crate::ModelError> {
    match value.serialize(Walk) {
        Err(Stop::Time(ticks)) => Err(crate::ModelError::UnwritableTime { ticks }),
        Ok(()) | Err(Stop::Other) => Ok(()),
    }
}

#[derive(Debug)]
enum Stop {
    Time(i64),
    Other,
}

impl fmt::Display for Stop {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("stopped")
    }
}

impl std::error::Error for Stop {}

impl ser::Error for Stop {
    fn custom<T: fmt::Display>(_: T) -> Self {
        Self::Other
    }
}

/// Reads one `MediaTime`'s ticks.
struct Ticks;

#[derive(Clone, Copy)]
struct Walk;

macro_rules! ignore {
    ($($method:ident($type:ty)),* $(,)?) => {
        $(fn $method(self, _: $type) -> Result<(), Stop> { Ok(()) })*
    };
}

impl ser::Serializer for Walk {
    type Ok = ();
    type Error = Stop;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    ignore!(
        serialize_bool(bool),
        serialize_i8(i8),
        serialize_i16(i16),
        serialize_i32(i32),
        serialize_i64(i64),
        serialize_u8(u8),
        serialize_u16(u16),
        serialize_u32(u32),
        serialize_u64(u64),
        serialize_f32(f32),
        serialize_f64(f64),
        serialize_char(char),
        serialize_str(&str),
        serialize_bytes(&[u8]),
    );

    fn serialize_none(self) -> Result<(), Stop> {
        Ok(())
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<(), Stop> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), Stop> {
        Ok(())
    }

    fn serialize_unit_struct(self, _: &'static str) -> Result<(), Stop> {
        Ok(())
    }

    fn serialize_unit_variant(self, _: &'static str, _: u32, _: &'static str) -> Result<(), Stop> {
        Ok(())
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<(), Stop> {
        if name != "MediaTime" {
            return value.serialize(self);
        }
        match value.serialize(Ticks) {
            Ok(ticks) if ticks.unsigned_abs() > MAX_SERIALIZED_TICKS.unsigned_abs() => {
                Err(Stop::Time(ticks))
            }
            _ => Ok(()),
        }
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        value: &T,
    ) -> Result<(), Stop> {
        value.serialize(self)
    }

    fn serialize_seq(self, _: Option<usize>) -> Result<Self, Stop> {
        Ok(self)
    }

    fn serialize_tuple(self, _: usize) -> Result<Self, Stop> {
        Ok(self)
    }

    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self, Stop> {
        Ok(self)
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self, Stop> {
        Ok(self)
    }

    fn serialize_map(self, _: Option<usize>) -> Result<Self, Stop> {
        Ok(self)
    }

    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self, Stop> {
        Ok(self)
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self, Stop> {
        Ok(self)
    }
}

impl ser::SerializeSeq for Walk {
    type Ok = ();
    type Error = Stop;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Stop> {
        value.serialize(*self)
    }

    fn end(self) -> Result<(), Stop> {
        Ok(())
    }
}

impl ser::SerializeTuple for Walk {
    type Ok = ();
    type Error = Stop;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Stop> {
        value.serialize(*self)
    }

    fn end(self) -> Result<(), Stop> {
        Ok(())
    }
}

impl ser::SerializeTupleStruct for Walk {
    type Ok = ();
    type Error = Stop;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Stop> {
        value.serialize(*self)
    }

    fn end(self) -> Result<(), Stop> {
        Ok(())
    }
}

impl ser::SerializeTupleVariant for Walk {
    type Ok = ();
    type Error = Stop;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Stop> {
        value.serialize(*self)
    }

    fn end(self) -> Result<(), Stop> {
        Ok(())
    }
}

impl ser::SerializeMap for Walk {
    type Ok = ();
    type Error = Stop;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), Stop> {
        key.serialize(*self)
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Stop> {
        value.serialize(*self)
    }

    fn end(self) -> Result<(), Stop> {
        Ok(())
    }
}

impl ser::SerializeStruct for Walk {
    type Ok = ();
    type Error = Stop;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        _: &'static str,
        value: &T,
    ) -> Result<(), Stop> {
        value.serialize(*self)
    }

    fn end(self) -> Result<(), Stop> {
        Ok(())
    }
}

impl ser::SerializeStructVariant for Walk {
    type Ok = ();
    type Error = Stop;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        _: &'static str,
        value: &T,
    ) -> Result<(), Stop> {
        value.serialize(*self)
    }

    fn end(self) -> Result<(), Stop> {
        Ok(())
    }
}

impl ser::Serializer for Ticks {
    type Ok = i64;
    type Error = Stop;
    type SerializeSeq = ser::Impossible<i64, Stop>;
    type SerializeTuple = ser::Impossible<i64, Stop>;
    type SerializeTupleStruct = ser::Impossible<i64, Stop>;
    type SerializeTupleVariant = ser::Impossible<i64, Stop>;
    type SerializeMap = ser::Impossible<i64, Stop>;
    type SerializeStruct = ser::Impossible<i64, Stop>;
    type SerializeStructVariant = ser::Impossible<i64, Stop>;

    fn serialize_i64(self, ticks: i64) -> Result<i64, Stop> {
        Ok(ticks)
    }

    fn serialize_bool(self, _: bool) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_i8(self, _: i8) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_i16(self, _: i16) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_i32(self, _: i32) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_u8(self, _: u8) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_u16(self, _: u16) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_u32(self, _: u32) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_u64(self, _: u64) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_f32(self, _: f32) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_f64(self, _: f64) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_char(self, _: char) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_str(self, _: &str) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_bytes(self, _: &[u8]) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_none(self) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_unit(self) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_unit_struct(self, _: &'static str) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_unit_variant(self, _: &'static str, _: u32, _: &'static str) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: &T,
    ) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<i64, Stop> {
        Err(Stop::Other)
    }

    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Stop> {
        Err(Stop::Other)
    }

    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Stop> {
        Err(Stop::Other)
    }

    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct, Stop> {
        Err(Stop::Other)
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, Stop> {
        Err(Stop::Other)
    }

    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Stop> {
        Err(Stop::Other)
    }

    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct, Stop> {
        Err(Stop::Other)
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, Stop> {
        Err(Stop::Other)
    }
}
