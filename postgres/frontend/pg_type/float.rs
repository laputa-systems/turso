//! PostgreSQL's float4 and float8.

use crate::error::sqlstate_error;
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use turso_core::ast;
use turso_core::schema::{OperatorArg, OperatorSignature};
use turso_core::type_ordering::{OrderingDef, TypeOrdering};
use turso_core::types::{Conversion, StorageClass, Type};
use turso_core::{LimboError, Numeric, Result, Value, ValueRef};
use turso_pg_parser::sqlstate::{
    SQLSTATE_DATATYPE_MISMATCH, SQLSTATE_INVALID_TEXT_REPRESENTATION,
    SQLSTATE_NUMERIC_VALUE_OUT_OF_RANGE,
};

/// PostgreSQL's float8, a 64-bit float. A value is stored as the 8 bytes of
/// the float (big endian), which can be a NaN and keeps the sign of a zero.
/// A value is read as a number, and as the text `NaN` for a NaN, because a
/// number of core cannot be a NaN.
#[derive(Debug)]
pub(super) struct Float8;

/// PostgreSQL's float4, a 32-bit float. It is stored as a float8 that holds
/// a float4, so it has the same 8 bytes.
#[derive(Debug)]
pub(super) struct Float4;

impl Type for Float8 {
    fn name(&self) -> &str {
        "float8"
    }

    fn params(&self) -> Vec<ast::TypeParam> {
        float_params()
    }

    fn base(&self) -> &str {
        "blob"
    }

    fn encode(&self) -> Option<Conversion> {
        Some(Conversion::Function(|value, _params| {
            encode(value, Precision::Double)
        }))
    }

    fn decode(&self) -> Option<Conversion> {
        Some(Conversion::Function(decode))
    }

    fn reads_as(&self) -> Option<StorageClass> {
        Some(StorageClass::Real)
    }

    fn ordering(&self) -> Option<TypeOrdering> {
        Some(TypeOrdering::new(&PG_FLOAT))
    }

    fn operator_signatures(&self) -> Vec<OperatorSignature> {
        float_operators()
    }
}

impl Type for Float4 {
    fn name(&self) -> &str {
        "float4"
    }

    fn params(&self) -> Vec<ast::TypeParam> {
        float_params()
    }

    fn base(&self) -> &str {
        "blob"
    }

    fn encode(&self) -> Option<Conversion> {
        Some(Conversion::Function(|value, _params| {
            encode(value, Precision::Single)
        }))
    }

    fn decode(&self) -> Option<Conversion> {
        Some(Conversion::Function(decode))
    }

    fn reads_as(&self) -> Option<StorageClass> {
        Some(StorageClass::Real)
    }

    fn ordering(&self) -> Option<TypeOrdering> {
        Some(TypeOrdering::new(&PG_FLOAT))
    }

    fn operator_signatures(&self) -> Vec<OperatorSignature> {
        float_operators()
    }
}

fn float_params() -> Vec<ast::TypeParam> {
    vec![ast::TypeParam {
        name: "value".to_string(),
        ty: Some("any".to_string()),
    }]
}

/// `+`, `-`, `*`, `/` and unary `-` run as SQLite's operators on the numbers
/// that a float is read as. PostgreSQL has no other operator for floats.
fn float_operators() -> Vec<OperatorSignature> {
    use OperatorArg::{PlainValue, Value};
    let mut signatures = Vec::new();
    for op in ["+", "-", "*", "/"] {
        for args in [[Value, PlainValue], [PlainValue, Value]] {
            signatures.push(OperatorSignature {
                op,
                args: args.to_vec(),
                func: None,
                result: None,
            });
        }
    }
    signatures.push(OperatorSignature {
        op: "-",
        args: vec![Value],
        func: None,
        result: None,
    });
    signatures
}

#[derive(Clone, Copy, PartialEq)]
enum Precision {
    Single,
    Double,
}

impl Precision {
    fn type_name(self) -> &'static str {
        match self {
            Precision::Single => "real",
            Precision::Double => "double precision",
        }
    }

    /// The nearest number that the type holds. PostgreSQL refuses a finite
    /// number that is too large for a float4 or becomes zero.
    fn round(self, number: f64) -> Result<f64> {
        if self == Precision::Double {
            return Ok(number);
        }
        let rounded = number as f32;
        if rounded.is_infinite() && number.is_finite() {
            return Err(out_of_range("value out of range: overflow"));
        }
        if rounded == 0.0 && number != 0.0 {
            return Err(out_of_range("value out of range: underflow"));
        }
        Ok(f64::from(rounded))
    }
}

/// The bytes of a NaN that is stored, so that every NaN is stored alike.
const NAN_BITS: u64 = 0x7FF8_0000_0000_0000;

/// What a column of the type stores for a value that is written: the 8 bytes
/// of the float. Text is read as PostgreSQL's float input does.
fn encode(value: &Value, precision: Precision) -> Result<Value> {
    let number = match value {
        Value::Text(text) => parse_float_text(text.as_str(), precision)?,
        Value::Numeric(Numeric::Integer(integer)) => precision.round(*integer as f64)?,
        Value::Numeric(Numeric::Float(float)) => precision.round(f64::from(*float))?,
        other => {
            return Err(sqlstate_error(
                SQLSTATE_DATATYPE_MISMATCH,
                format!("the value {other} is not of type {}", precision.type_name()),
                None,
            ))
        }
    };
    let number = if number.is_nan() {
        f64::from_bits(NAN_BITS)
    } else {
        number
    };
    Ok(Value::from_blob(number.to_be_bytes().to_vec()))
}

/// The number that the stored bytes are, or the text `NaN` for a NaN.
fn decode(value: &Value, _params: &[Value]) -> Result<Value> {
    match value {
        Value::Blob(bytes) => match <[u8; 8]>::try_from(bytes.as_slice()) {
            Ok(bytes) => {
                let number = f64::from_be_bytes(bytes);
                Ok(if number.is_nan() {
                    Value::build_text("NaN")
                } else {
                    Value::from_f64(number)
                })
            }
            Err(_) => Err(LimboError::Corrupt(
                "a stored float is not 8 bytes".to_string(),
            )),
        },
        other => Ok(other.clone()),
    }
}

/// Reads text as PostgreSQL's float input does: spaces around the number are
/// allowed, and `NaN`, `Infinity` and `-Infinity` (also `inf`) are numbers in
/// any case. A number that is too large or too small for the type is an
/// error, and so is anything else that is not a number.
fn parse_float_text(input: &str, precision: Precision) -> Result<f64> {
    read_pg_float(input, precision == Precision::Single)
        .map_err(|error| sqlstate_error(error.sqlstate, error.message, None))
}

/// Why text is not a float.
pub struct FloatInputError {
    pub message: String,
    pub sqlstate: &'static str,
}

/// Reads `input` as the input function of float4 (`single`) or of float8
/// does. See [parse_float_text].
pub fn read_pg_float(input: &str, single: bool) -> std::result::Result<f64, FloatInputError> {
    let precision = if single {
        Precision::Single
    } else {
        Precision::Double
    };
    let trimmed = input.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\u{b}');
    let parsed = match precision {
        Precision::Single => trimmed.parse::<f32>().map(f64::from),
        Precision::Double => trimmed.parse::<f64>(),
    };
    let Ok(number) = parsed else {
        return Err(FloatInputError {
            message: format!(
                "invalid input syntax for type {}: \"{input}\"",
                precision.type_name()
            ),
            sqlstate: SQLSTATE_INVALID_TEXT_REPRESENTATION,
        });
    };
    let is_infinity_word = {
        let word = trimmed.trim_start_matches(['+', '-']);
        word.eq_ignore_ascii_case("inf") || word.eq_ignore_ascii_case("infinity")
    };
    let has_nonzero_digit = trimmed
        .split(['e', 'E'])
        .next()
        .is_some_and(|mantissa| mantissa.chars().any(|c| ('1'..='9').contains(&c)));
    if (number.is_infinite() && !is_infinity_word) || (number == 0.0 && has_nonzero_digit) {
        return Err(FloatInputError {
            message: format!(
                "\"{input}\" is out of range for type {}",
                precision.type_name()
            ),
            sqlstate: SQLSTATE_NUMERIC_VALUE_OUT_OF_RANGE,
        });
    }
    Ok(number)
}

fn out_of_range(message: &str) -> LimboError {
    sqlstate_error(SQLSTATE_NUMERIC_VALUE_OUT_OF_RANGE, message, None)
}

/// How floats compare, as PostgreSQL's float8 and float4 do: NaN equals NaN
/// and is greater than every number, and -0 equals 0. It reads the stored
/// bytes, a number and text that is a number, so a comparison with a literal
/// such as 'Infinity' works.
static PG_FLOAT: OrderingDef = OrderingDef {
    name: "pg_float",
    compare: float_compare,
    compare_operands: None,
    hash: float_hash,
};

fn float_compare(left: ValueRef, right: ValueRef) -> Ordering {
    FloatKey::of(left).cmp(&FloatKey::of(right))
}

fn float_hash(value: ValueRef, mut state: &mut dyn Hasher) {
    match FloatKey::of(value) {
        FloatKey::Null => 0u8.hash(&mut state),
        FloatKey::Number(PgFloat(number)) => {
            1u8.hash(&mut state);
            let bits = if number.is_nan() {
                NAN_BITS
            } else if number == 0.0 {
                0
            } else {
                number.to_bits()
            };
            bits.hash(&mut state);
        }
        FloatKey::Text(text) => {
            2u8.hash(&mut state);
            text.hash(&mut state);
        }
        FloatKey::Blob(bytes) => {
            3u8.hash(&mut state);
            bytes.hash(&mut state);
        }
    }
}

/// NULL first, then numbers, then text that is not a number, then other
/// blobs. A value that is not a float still has a place.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum FloatKey<'a> {
    Null,
    Number(PgFloat),
    Text(&'a str),
    Blob(&'a [u8]),
}

impl<'a> FloatKey<'a> {
    fn of(value: ValueRef<'a>) -> Self {
        match value {
            ValueRef::Null => Self::Null,
            ValueRef::Numeric(Numeric::Integer(integer)) => Self::Number(PgFloat(integer as f64)),
            ValueRef::Numeric(Numeric::Float(float)) => Self::Number(PgFloat(f64::from(float))),
            ValueRef::Blob(bytes) => match <[u8; 8]>::try_from(bytes) {
                Ok(bytes) => Self::Number(PgFloat(f64::from_be_bytes(bytes))),
                Err(_) => Self::Blob(bytes),
            },
            ValueRef::Text(text) => match parse_float_text(text.as_str(), Precision::Double) {
                Ok(number) => Self::Number(PgFloat(number)),
                Err(_) => Self::Text(text.as_str()),
            },
        }
    }
}

#[derive(Clone, Copy)]
struct PgFloat(f64);

impl Ord for PgFloat {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.0.is_nan(), other.0.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => self.0.partial_cmp(&other.0).expect("neither number is NaN"),
        }
    }
}

impl PartialOrd for PgFloat {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for PgFloat {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for PgFloat {}

#[cfg(test)]
mod tests {
    use super::*;
    use turso_core::types::AsValueRef;

    fn stored(text: &str, precision: Precision) -> f64 {
        let Value::Blob(bytes) = encode(&Value::from_text(text.to_string()), precision).unwrap()
        else {
            panic!("a float is stored as a blob");
        };
        f64::from_be_bytes(bytes.as_slice().try_into().unwrap())
    }

    fn key(value: &Value) -> ValueRef<'_> {
        value.as_value_ref()
    }

    fn error_of(text: &str, precision: Precision) -> String {
        encode(&Value::from_text(text.to_string()), precision)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn text_is_read_as_postgresql_reads_it() {
        assert!(stored(" NaN ", Precision::Double).is_nan());
        assert!(stored("nan", Precision::Single).is_nan());
        assert_eq!(stored("inf", Precision::Double), f64::INFINITY);
        assert_eq!(stored("-Infinity", Precision::Single), f64::NEG_INFINITY);
        assert_eq!(stored("  1.5  ", Precision::Double), 1.5);
        assert_eq!(stored("1e3", Precision::Double), 1000.0);
        assert!(stored("-0", Precision::Double).is_sign_negative());
    }

    #[test]
    fn a_float4_holds_what_an_f32_holds() {
        assert_eq!(stored("0.1", Precision::Single), f64::from(0.1_f32));
        assert_ne!(stored("0.1", Precision::Single), 0.1);
        assert_eq!(stored("1e-45", Precision::Single), f64::from(1e-45_f32));
    }

    #[test]
    fn a_number_out_of_range_is_refused() {
        assert!(
            error_of("1e39", Precision::Single).contains("\"1e39\" is out of range for type real")
        );
        assert!(error_of("1e400", Precision::Double)
            .contains("is out of range for type double precision"));
        assert!(error_of("1e-400", Precision::Double).contains("is out of range"));
        assert!(error_of("1e-46", Precision::Single).contains("is out of range"));
        assert!(error_of("abc", Precision::Double)
            .contains("invalid input syntax for type double precision: \"abc\""));
        assert!(error_of("", Precision::Single).contains("invalid input syntax for type real"));
    }

    #[test]
    fn nan_is_read_back_as_text_and_the_rest_as_numbers() {
        let nan = encode(&Value::from_text("NaN"), Precision::Double).unwrap();
        assert_eq!(decode(&nan, &[]).unwrap(), Value::build_text("NaN"));
        let zero = encode(&Value::from_text("-0"), Precision::Double).unwrap();
        let Value::Numeric(Numeric::Float(read)) = decode(&zero, &[]).unwrap() else {
            panic!("a zero is read as a number");
        };
        assert!(f64::from(read).is_sign_negative());
    }

    #[test]
    fn floats_order_as_postgresql_orders_them() {
        let bytes =
            |text: &str| encode(&Value::from_text(text.to_string()), Precision::Double).unwrap();
        let (nan, other_nan, infinity, minus_zero, zero) = (
            bytes("NaN"),
            bytes(" nan"),
            bytes("Infinity"),
            bytes("-0"),
            bytes("0"),
        );
        assert_eq!(float_compare(key(&nan), key(&other_nan)), Ordering::Equal);
        assert_eq!(float_compare(key(&nan), key(&infinity)), Ordering::Greater);
        assert_eq!(float_compare(key(&minus_zero), key(&zero)), Ordering::Equal);
        assert_eq!(
            float_compare(key(&nan), Value::from_text("Infinity").as_value_ref()),
            Ordering::Greater
        );
        let hash = |value: &Value| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            float_hash(value.as_value_ref(), &mut hasher);
            hasher.finish()
        };
        assert_eq!(hash(&nan), hash(&other_nan));
        assert_eq!(hash(&minus_zero), hash(&zero));
        assert_eq!(hash(&zero), hash(&Value::from_i64(0)));
    }
}
