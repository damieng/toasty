//! Conversion between Toasty's [`stmt::Value`] and MongoDB's [`Bson`].
//!
//! The mapping is deliberately simple for the first cut of the driver:
//!
//! * UUIDs are stored as strings so that equality predicates compare the same
//!   way they are written. (A later revision may switch to the BSON UUID
//!   binary subtype.)
//! * Narrow integers (`i8`..`u32`) round-trip through `Int32`; 64-bit integers
//!   through `Int64`.
//! * Temporal and decimal types arrive pre-encoded as strings via the
//!   [`StorageTypes`](toasty_core::driver::StorageTypes) mapping, so they need
//!   no special handling here.

use mongodb::bson::{Bson, spec::BinarySubtype};
use toasty_core::stmt::{self, Value as CoreValue};

/// Wraps a Toasty value for conversion to and from BSON.
#[derive(Debug)]
pub struct Value(CoreValue);

impl From<CoreValue> for Value {
    fn from(value: CoreValue) -> Self {
        Self(value)
    }
}

impl Value {
    /// Converts a Toasty value into a BSON value for storage or filtering.
    pub fn to_bson(&self) -> Bson {
        Self::value_to_bson(&self.0)
    }

    fn value_to_bson(value: &CoreValue) -> Bson {
        match value {
            stmt::Value::Bool(val) => Bson::Boolean(*val),
            stmt::Value::String(val) => Bson::String(val.clone()),
            stmt::Value::I8(val) => Bson::Int32(*val as i32),
            stmt::Value::I16(val) => Bson::Int32(*val as i32),
            stmt::Value::I32(val) => Bson::Int32(*val),
            stmt::Value::I64(val) => Bson::Int64(*val),
            stmt::Value::U8(val) => Bson::Int32(*val as i32),
            stmt::Value::U16(val) => Bson::Int32(*val as i32),
            stmt::Value::U32(val) => Bson::Int64(*val as i64),
            stmt::Value::U64(val) => Bson::Int64(*val as i64),
            stmt::Value::F32(val) => Bson::Double(*val as f64),
            stmt::Value::F64(val) => Bson::Double(*val),
            stmt::Value::Uuid(val) => Bson::String(val.to_string()),
            stmt::Value::Bytes(val) => Bson::Binary(mongodb::bson::Binary {
                subtype: BinarySubtype::Generic,
                bytes: val.clone(),
            }),
            stmt::Value::List(vals) => Bson::Array(vals.iter().map(Self::value_to_bson).collect()),
            stmt::Value::Null => Bson::Null,
            _ => todo!("unsupported value -> bson: {:#?}", value),
        }
    }

    /// Converts a BSON value back into a Toasty value of the given type.
    pub fn from_bson(ty: &stmt::Type, val: &Bson) -> CoreValue {
        use stmt::Type;

        match (ty, val) {
            (_, Bson::Null) => stmt::Value::Null,
            (Type::Bool, Bson::Boolean(val)) => stmt::Value::from(*val),
            (Type::String, Bson::String(val)) => stmt::Value::from(val.clone()),
            (Type::I8, b) => stmt::Value::from(bson_as_i64(b) as i8),
            (Type::I16, b) => stmt::Value::from(bson_as_i64(b) as i16),
            (Type::I32, b) => stmt::Value::from(bson_as_i64(b) as i32),
            (Type::I64, b) => stmt::Value::from(bson_as_i64(b)),
            (Type::U8, b) => stmt::Value::from(bson_as_i64(b) as u8),
            (Type::U16, b) => stmt::Value::from(bson_as_i64(b) as u16),
            (Type::U32, b) => stmt::Value::from(bson_as_i64(b) as u32),
            (Type::U64, b) => stmt::Value::from(bson_as_i64(b) as u64),
            (Type::F32, Bson::Double(val)) => stmt::Value::from(*val as f32),
            (Type::F64, Bson::Double(val)) => stmt::Value::from(*val),
            (Type::Uuid, Bson::String(val)) => {
                stmt::Value::from(val.parse::<uuid::Uuid>().expect("invalid uuid string"))
            }
            (Type::Bytes, Bson::Binary(bin)) => stmt::Value::Bytes(bin.bytes.clone()),
            (Type::List(elem), Bson::Array(items)) => stmt::Value::List(
                items
                    .iter()
                    .map(|item| Self::from_bson(elem, item))
                    .collect(),
            ),
            _ => todo!("unsupported bson -> value: ty={ty:#?}; value={val:#?}"),
        }
    }
}

/// Coerces any BSON numeric variant into an `i64`. MongoDB may return an
/// integer field as `Int32`, `Int64`, or `Double` depending on how it was
/// written, so narrow-integer columns accept all three.
fn bson_as_i64(b: &Bson) -> i64 {
    match b {
        Bson::Int32(v) => *v as i64,
        Bson::Int64(v) => *v,
        Bson::Double(v) => *v as i64,
        _ => todo!("expected numeric bson, got {b:#?}"),
    }
}
