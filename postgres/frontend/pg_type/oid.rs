//! PostgreSQL's oid type.

use crate::error::sqlstate_error;
use turso_core::ast;
use turso_core::types::{Conversion, StorageClass, Type};
use turso_core::{Numeric, Result, Value};
use turso_pg_parser::sqlstate::{
    SQLSTATE_CANNOT_COERCE, SQLSTATE_DATATYPE_MISMATCH, SQLSTATE_INVALID_TEXT_REPRESENTATION,
    SQLSTATE_NUMERIC_VALUE_OUT_OF_RANGE,
};

/// PostgreSQL's oid: a number from 0 to 4294967295, stored as that integer.
#[derive(Debug)]
pub(super) struct Oid;

impl Type for Oid {
    fn name(&self) -> &str {
        "oid"
    }

    fn params(&self) -> Vec<ast::TypeParam> {
        vec![ast::TypeParam {
            name: "value".to_string(),
            ty: Some("any".to_string()),
        }]
    }

    fn base(&self) -> &str {
        "integer"
    }

    fn encode(&self) -> Option<Conversion> {
        Some(Conversion::Function(oid_encode))
    }

    fn cast(&self) -> Option<Conversion> {
        Some(Conversion::Function(oid_cast))
    }

    fn reads_as(&self) -> Option<StorageClass> {
        Some(StorageClass::Integer)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OidInputError {
    InvalidSyntax,
    OutOfRange,
}

/// Reads the text of an oid as PostgreSQL does: white space around the
/// number, an optional sign, and the decimal, hex (`0x`) and octal (leading
/// `0`) spellings. A negative number from -2147483648 up wraps around, so -1
/// is 4294967295.
pub(crate) fn parse_pg_oid(text: &str) -> std::result::Result<i64, OidInputError> {
    let text = text.trim_matches(|c: char| c.is_ascii_whitespace());
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (radix, digits) = match digits.as_bytes() {
        [b'0', b'x' | b'X', rest @ ..] if rest.first().is_some_and(u8::is_ascii_hexdigit) => {
            (16, &digits[2..])
        }
        [b'0', _, ..] => (8, &digits[1..]),
        _ => (10, digits),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(OidInputError::InvalidSyntax);
    }
    let magnitude = u64::from_str_radix(digits, radix).map_err(|_| OidInputError::OutOfRange)?;
    if negative {
        match magnitude {
            0 => Ok(0),
            1..=0x8000_0000 => Ok((1 << 32) - magnitude as i64),
            _ => Err(OidInputError::OutOfRange),
        }
    } else {
        i64::try_from(magnitude)
            .ok()
            .filter(|value| *value <= i64::from(u32::MAX))
            .ok_or(OidInputError::OutOfRange)
    }
}

/// Stores an integer as PostgreSQL's casts to oid do: a negative integer of
/// four bytes wraps around, and one that is out of range is an error.
fn oid_from_integer(integer: i64) -> Result<Value> {
    match integer {
        0..=0xFFFF_FFFF => Ok(Value::from_i64(integer)),
        -0x8000_0000..=-1 => Ok(Value::from_i64(integer + (1 << 32))),
        _ => Err(sqlstate_error(
            SQLSTATE_NUMERIC_VALUE_OUT_OF_RANGE,
            "OID out of range",
            None,
        )),
    }
}

fn oid_from_text(text: &str) -> Result<Value> {
    match parse_pg_oid(text) {
        Ok(oid) => Ok(Value::from_i64(oid)),
        Err(OidInputError::InvalidSyntax) => Err(sqlstate_error(
            SQLSTATE_INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type oid: \"{text}\""),
            None,
        )),
        Err(OidInputError::OutOfRange) => Err(sqlstate_error(
            SQLSTATE_NUMERIC_VALUE_OUT_OF_RANGE,
            format!("value \"{text}\" is out of range for type oid"),
            None,
        )),
    }
}

fn oid_encode(value: &Value, _params: &[Value]) -> Result<Value> {
    match value {
        Value::Numeric(Numeric::Integer(integer)) => oid_from_integer(*integer),
        Value::Text(text) => oid_from_text(text.as_str()),
        other => Err(sqlstate_error(
            SQLSTATE_DATATYPE_MISMATCH,
            format!("the value {other} is not of type oid"),
            None,
        )),
    }
}

/// PostgreSQL has no cast from a decimal number to oid, and says so with a
/// different SQLSTATE than it uses for a value written to an oid column.
fn oid_cast(value: &Value, _params: &[Value]) -> Result<Value> {
    match value {
        Value::Numeric(Numeric::Float(_)) => Err(sqlstate_error(
            SQLSTATE_CANNOT_COERCE,
            "cannot cast a decimal number to type oid",
            None,
        )),
        other => Ok(other.clone()),
    }
}
