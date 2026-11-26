use crate::{types::AsValueRef, Value, ValueRef};

pub mod decimal;
pub(crate) mod fp;
pub mod nonnan;

use nonnan::NonNan;

// TODO: Remove when https://github.com/rust-lang/libs-team/issues/230 is available
trait SaturatingShl {
    fn saturating_shl(self, rhs: u32) -> Self;
}

impl SaturatingShl for i64 {
    fn saturating_shl(self, rhs: u32) -> Self {
        if rhs >= Self::BITS {
            0
        } else {
            self << rhs
        }
    }
}

// TODO: Remove when https://github.com/rust-lang/libs-team/issues/230 is available
trait SaturatingShr {
    fn saturating_shr(self, rhs: u32) -> Self;
}

impl SaturatingShr for i64 {
    fn saturating_shr(self, rhs: u32) -> Self {
        if rhs >= Self::BITS {
            if self >= 0 {
                0
            } else {
                -1
            }
        } else {
            self >> rhs
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Numeric {
    Integer(i64),
    Float(NonNan),
}

impl Numeric {
    pub fn from_value<T: AsValueRef>(value: T) -> Option<Self> {
        let value = value.as_value_ref();

        match value {
            ValueRef::Null => None,
            ValueRef::Numeric(v) => Some(v),
            ValueRef::Text(text) => Some(Numeric::from(text.as_str())),
            ValueRef::Blob(blob) => {
                let text = String::from_utf8_lossy(blob);
                Some(Numeric::from(&text))
            }
        }
    }

    #[inline]
    pub fn from_value_strict(value: &Value) -> Option<Self> {
        match value {
            Value::Null | Value::Blob(_) => None,
            Value::Numeric(n) => Some(*n),
            Value::Text(text) => {
                let s = text.as_str();

                match str_to_f64(s) {
                    None
                    | Some(StrToF64::FractionalPrefix(_))
                    | Some(StrToF64::DecimalPrefix(_)) => None,
                    Some(StrToF64::Fractional(value)) => Some(Self::Float(value)),
                    Some(StrToF64::Decimal(real)) => Some(match str_to_i64_checked(s) {
                        Ok(integer) => Self::Integer(integer),
                        Err(_) => Self::Float(real),
                    }),
                }
            }
        }
    }

    /// The text of a REAL, as SQLite 3.53.4 converts a REAL to TEXT with
    /// `printf('%!.17g', value)`: 17 significant digits, or fewer when the
    /// last of them are 9s or 0s and the shorter text reads back as the same
    /// double. One is `1.0`, 10^20 is `1.0e+20`, 0.1 is `0.1`, 1.0/3 is
    /// `0.33333333333333332` and an infinity is `Inf` or `-Inf`.
    pub fn real_to_text(value: f64) -> String {
        format_float(value)
    }

    /// The REAL that SQLite 3.53.4 reads from the start of `text`, as
    /// CAST(text AS REAL) does: leading and trailing whitespace is skipped,
    /// reading stops at the first byte that does not continue the number or
    /// at a NUL byte, and a text that does not start with a number is 0.0.
    pub fn text_to_real(text: &[u8]) -> f64 {
        fp::atof(text).value
    }

    #[inline]
    pub fn to_f64(&self) -> f64 {
        match self {
            Numeric::Integer(v) => *v as _,
            Numeric::Float(v) => (*v).into(),
        }
    }

    #[inline]
    pub fn to_bool(&self) -> bool {
        match self {
            Numeric::Integer(0) => false,
            Numeric::Float(non_nan) if *non_nan == 0.0 => false,
            _ => true,
        }
    }

    #[inline]
    pub fn checked_add(self, rhs: Self) -> Option<Self> {
        match (self, rhs) {
            (Numeric::Integer(lhs), Numeric::Integer(rhs)) => match lhs.checked_add(rhs) {
                None => Numeric::Float(lhs.into()).checked_add(Numeric::Float(rhs.into())),
                Some(i) => Some(Numeric::Integer(i)),
            },
            (Numeric::Float(lhs), Numeric::Float(rhs)) => (lhs + rhs).map(Numeric::Float),
            (f @ Numeric::Float(_), Numeric::Integer(i))
            | (Numeric::Integer(i), f @ Numeric::Float(_)) => {
                f.checked_add(Numeric::Float(i.into()))
            }
        }
    }

    #[inline]
    pub fn checked_sub(self, rhs: Self) -> Option<Self> {
        match (self, rhs) {
            (Numeric::Float(lhs), Numeric::Float(rhs)) => (lhs - rhs).map(Numeric::Float),
            (Numeric::Integer(lhs), Numeric::Integer(rhs)) => match lhs.checked_sub(rhs) {
                None => Numeric::Float(lhs.into()).checked_sub(Numeric::Float(rhs.into())),
                Some(i) => Some(Numeric::Integer(i)),
            },
            (f @ Numeric::Float(_), Numeric::Integer(i)) => f.checked_sub(Numeric::Float(i.into())),
            (Numeric::Integer(i), f @ Numeric::Float(_)) => Numeric::Float(i.into()).checked_sub(f),
        }
    }

    #[inline]
    pub fn checked_mul(self, rhs: Self) -> Option<Self> {
        match (self, rhs) {
            (Numeric::Float(lhs), Numeric::Float(rhs)) => (lhs * rhs).map(Numeric::Float),
            (Numeric::Integer(lhs), Numeric::Integer(rhs)) => match lhs.checked_mul(rhs) {
                None => Numeric::Float(lhs.into()).checked_mul(Numeric::Float(rhs.into())),
                Some(i) => Some(Numeric::Integer(i)),
            },
            (f @ Numeric::Float(_), Numeric::Integer(i))
            | (Numeric::Integer(i), f @ Numeric::Float(_)) => {
                f.checked_mul(Numeric::Float(i.into()))
            }
        }
    }

    #[inline]
    pub fn checked_div(self, rhs: Self) -> Option<Self> {
        match (self, rhs) {
            (Numeric::Float(lhs), Numeric::Float(rhs)) => match lhs / rhs {
                Some(v) if rhs != 0.0 => Some(Numeric::Float(v)),
                _ => None,
            },
            (Numeric::Integer(lhs), Numeric::Integer(rhs)) => match lhs.checked_div(rhs) {
                None => Numeric::Float(lhs.into()).checked_div(Numeric::Float(rhs.into())),
                Some(v) => Some(Numeric::Integer(v)),
            },
            (f @ Numeric::Float(_), Numeric::Integer(i)) => f.checked_div(Numeric::Float(i.into())),
            (Numeric::Integer(i), f @ Numeric::Float(_)) => Numeric::Float(i.into()).checked_div(f),
        }
    }
}

impl From<Numeric> for NullableInteger {
    #[inline]
    fn from(value: Numeric) -> Self {
        match value {
            Numeric::Integer(v) => NullableInteger::Integer(v),
            Numeric::Float(v) => NullableInteger::Integer(f64::from(v) as i64),
        }
    }
}

impl From<Numeric> for Value {
    #[inline]
    fn from(value: Numeric) -> Self {
        Value::Numeric(value)
    }
}

impl From<Option<Numeric>> for Value {
    fn from(value: Option<Numeric>) -> Self {
        value.map_or_else(|| Value::Null, Value::from)
    }
}

impl<T: AsRef<str>> From<T> for Numeric {
    fn from(value: T) -> Self {
        let text = value.as_ref();

        match str_to_f64(text) {
            None => Self::Integer(0),
            Some(StrToF64::Fractional(value) | StrToF64::FractionalPrefix(value)) => {
                Self::Float(value)
            }
            Some(StrToF64::Decimal(real) | StrToF64::DecimalPrefix(real)) => {
                match str_to_i64_checked(text) {
                    Ok(integer) => Self::Integer(integer),
                    Err(_) => Self::Float(real),
                }
            }
        }
    }
}

impl From<Value> for Option<Numeric> {
    fn from(value: Value) -> Self {
        Self::from(&value)
    }
}
impl From<&Value> for Option<Numeric> {
    fn from(value: &Value) -> Self {
        match value {
            Value::Null => None,
            Value::Numeric(n) => Some(*n),
            Value::Text(text) => Some(Numeric::from(text.as_str())),
            Value::Blob(blob) => {
                let text = String::from_utf8_lossy(blob.as_slice());
                Some(Numeric::from(&text))
            }
        }
    }
}

impl std::ops::Neg for Numeric {
    type Output = Self;

    fn neg(self) -> Self::Output {
        match self {
            Numeric::Integer(v) => match v.checked_neg() {
                None => -Numeric::Float(v.into()),
                Some(i) => Numeric::Integer(i),
            },
            Numeric::Float(v) => Numeric::Float(-v),
        }
    }
}

impl PartialEq for Numeric {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Numeric {}

impl PartialOrd for Numeric {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Numeric {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Numeric::Integer(a), Numeric::Integer(b)) => a.cmp(b),
            (Numeric::Float(a), Numeric::Float(b)) => {
                let fa: f64 = (*a).into();
                let fb: f64 = (*b).into();
                // NonNan guarantees no NaN, so partial_cmp always returns Some.
                // SQLite's float-vs-float uses raw IEEE 754 < and > operators,
                // which both return false for NaN, resulting in "equal".
                fa.partial_cmp(&fb).unwrap_or(std::cmp::Ordering::Equal)
            }
            (Numeric::Integer(int), Numeric::Float(float)) => {
                sqlite_int_float_cmp(*int, f64::from(*float))
            }
            (Numeric::Float(float), Numeric::Integer(int)) => {
                sqlite_int_float_cmp(*int, f64::from(*float)).reverse()
            }
        }
    }
}

/// Compare an integer and a float following SQLite semantics.
///
/// SQLite treats NaN as NULL, so int > NaN. In practice, NonNan prevents NaN
/// from appearing in Numeric::Float, but we check defensively.
///
/// See sqlite3IntFloatCompare in src/vdbeaux.c.
fn sqlite_int_float_cmp(int_val: i64, float_val: f64) -> std::cmp::Ordering {
    if float_val.is_nan() {
        // NaN is treated as NULL; all integers are greater than NULL
        return std::cmp::Ordering::Greater;
    }

    if float_val < -9_223_372_036_854_775_808.0 {
        return std::cmp::Ordering::Greater;
    }
    if float_val >= 9_223_372_036_854_775_808.0 {
        return std::cmp::Ordering::Less;
    }

    let float_as_int = float_val as i64;
    match int_val.cmp(&float_as_int) {
        std::cmp::Ordering::Equal => {
            let int_as_float = int_val as f64;
            int_as_float
                .partial_cmp(&float_val)
                .unwrap_or(std::cmp::Ordering::Equal)
        }
        other => other,
    }
}

#[derive(Debug)]
pub enum NullableInteger {
    Null,
    Integer(i64),
}

impl From<NullableInteger> for Value {
    fn from(value: NullableInteger) -> Self {
        match value {
            NullableInteger::Null => Value::Null,
            NullableInteger::Integer(v) => Value::from_i64(v),
        }
    }
}

impl<T: AsRef<str>> From<T> for NullableInteger {
    fn from(value: T) -> Self {
        Self::Integer(str_to_i64(value.as_ref()).unwrap_or(0))
    }
}

impl From<Value> for NullableInteger {
    fn from(value: Value) -> Self {
        Self::from(&value)
    }
}

impl From<&Value> for NullableInteger {
    fn from(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Numeric(Numeric::Integer(v)) => Self::Integer(*v),
            Value::Numeric(Numeric::Float(v)) => Self::Integer(f64::from(*v) as i64),
            Value::Text(text) => Self::from(text.as_str()),
            Value::Blob(blob) => {
                let text = String::from_utf8_lossy(blob.as_slice());
                Self::from(text)
            }
        }
    }
}

impl std::ops::Not for NullableInteger {
    type Output = Self;

    fn not(self) -> Self::Output {
        match self {
            NullableInteger::Null => NullableInteger::Null,
            NullableInteger::Integer(lhs) => NullableInteger::Integer(!lhs),
        }
    }
}

impl std::ops::BitAnd for NullableInteger {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (NullableInteger::Null, _) | (_, NullableInteger::Null) => NullableInteger::Null,
            (NullableInteger::Integer(lhs), NullableInteger::Integer(rhs)) => {
                NullableInteger::Integer(lhs & rhs)
            }
        }
    }
}

impl std::ops::BitOr for NullableInteger {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (NullableInteger::Null, _) | (_, NullableInteger::Null) => NullableInteger::Null,
            (NullableInteger::Integer(lhs), NullableInteger::Integer(rhs)) => {
                NullableInteger::Integer(lhs | rhs)
            }
        }
    }
}

impl std::ops::Shl for NullableInteger {
    type Output = Self;

    fn shl(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (NullableInteger::Null, _) | (_, NullableInteger::Null) => NullableInteger::Null,
            (NullableInteger::Integer(lhs), NullableInteger::Integer(rhs)) => {
                NullableInteger::Integer(if rhs.is_positive() {
                    lhs.saturating_shl(rhs.try_into().unwrap_or(u32::MAX))
                } else {
                    lhs.saturating_shr(rhs.saturating_abs().try_into().unwrap_or(u32::MAX))
                })
            }
        }
    }
}

impl std::ops::Shr for NullableInteger {
    type Output = Self;

    fn shr(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (NullableInteger::Null, _) | (_, NullableInteger::Null) => NullableInteger::Null,
            (NullableInteger::Integer(lhs), NullableInteger::Integer(rhs)) => {
                NullableInteger::Integer(if rhs.is_positive() {
                    lhs.saturating_shr(rhs.try_into().unwrap_or(u32::MAX))
                } else {
                    lhs.saturating_shl(rhs.saturating_abs().try_into().unwrap_or(u32::MAX))
                })
            }
        }
    }
}

impl std::ops::Rem for NullableInteger {
    type Output = Self;

    fn rem(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (NullableInteger::Null, _) | (_, NullableInteger::Null) => NullableInteger::Null,
            (_, NullableInteger::Integer(0)) => NullableInteger::Null,
            (lhs, NullableInteger::Integer(-1)) => lhs % NullableInteger::Integer(1),
            (NullableInteger::Integer(lhs), NullableInteger::Integer(rhs)) => {
                NullableInteger::Integer(lhs % rhs)
            }
        }
    }
}

const VERTICAL_TAB: char = '\u{b}';

pub fn str_to_i64(input: impl AsRef<str>) -> Option<i64> {
    Some(match str_to_i64_checked(input) {
        Ok(v) => v,
        Err(IntOverflow::Positive) => i64::MAX,
        Err(IntOverflow::Negative) => i64::MIN,
    })
}

#[derive(Debug, Clone, Copy)]
pub enum IntOverflow {
    Positive,
    Negative,
}

/// Like [`str_to_i64`] but distinguishes overflow from a successful parse.
/// Returns `Err(_)` when the leading numeric prefix does not fit in i64.
pub fn str_to_i64_checked(input: impl AsRef<str>) -> Result<i64, IntOverflow> {
    let input = input
        .as_ref()
        .trim_matches(|ch: char| ch.is_ascii_whitespace() || ch == VERTICAL_TAB);

    let mut iter = input.chars().enumerate().peekable();

    iter.next_if(|(_, ch)| matches!(ch, '+' | '-'));
    let Some((end, _)) = iter.take_while(|(_, ch)| ch.is_ascii_digit()).last() else {
        return Ok(0);
    };

    input[0..=end]
        .parse::<i64>()
        .map_err(|err| match err.kind() {
            std::num::IntErrorKind::PosOverflow => IntOverflow::Positive,
            std::num::IntErrorKind::NegOverflow => IntOverflow::Negative,
            // Empty/InvalidDigit are ruled out: the early `return Ok(0)` above
            // fires when there are no digits, and the slice spans only an
            // optional sign followed by ASCII digits. Zero is produced only by
            // `NonZeroI*::from_str`, not by `i64::from_str`. `IntErrorKind` is
            // `#[non_exhaustive]`, hence the catch-all.
            _ => unreachable!("unexpected IntErrorKind from i64::from_str: {err:?}"),
        })
}

#[derive(Debug, Clone, Copy)]
pub enum StrToF64 {
    Fractional(NonNan),
    Decimal(NonNan),
    FractionalPrefix(NonNan),
    DecimalPrefix(NonNan),
}

impl From<StrToF64> for f64 {
    fn from(value: StrToF64) -> Self {
        match value {
            StrToF64::Fractional(non_nan) => non_nan.into(),
            StrToF64::Decimal(non_nan) => non_nan.into(),
            StrToF64::FractionalPrefix(non_nan) => non_nan.into(),
            StrToF64::DecimalPrefix(non_nan) => non_nan.into(),
        }
    }
}

/// The number at the start of `input`, as `sqlite3AtoF` reads it, and
/// whether it fills the text and has a decimal point or an exponent. `None`
/// when the text does not start with a number.
pub fn str_to_f64(input: impl AsRef<str>) -> Option<StrToF64> {
    let read = fp::atof(input.as_ref().as_bytes());
    if !read.has_prefix_number() {
        return None;
    }
    let value = NonNan::new(read.value).expect("sqlite3AtoF never reads a NaN");
    Some(match (read.is_number(), read.is_fractional()) {
        (true, true) => StrToF64::Fractional(value),
        (true, false) => StrToF64::Decimal(value),
        (false, true) => StrToF64::FractionalPrefix(value),
        (false, false) => StrToF64::DecimalPrefix(value),
    })
}

#[cfg(test)]
pub fn round_half_away_from_zero_tie(f: f64, precision: usize) -> Option<f64> {
    use num_traits::Float;

    let (m, k, sign): (u64, i16, i8) = f.integer_decode();
    let k = k as i32;

    let precision_i32 = precision as i32;
    let q = -(k + precision_i32);

    const MAX_TIE_PRODUCT_BITS: i32 = 127;
    if !(1..=MAX_TIE_PRODUCT_BITS).contains(&q) {
        return None;
    }

    let precision_u32 = precision as u32;
    let five_pow = 5u128.pow(precision_u32);
    let scaled = (m as u128) * five_pow;

    let low_q_bits = scaled & ((1u128 << q) - 1);
    let is_tie = low_q_bits == (1u128 << (q - 1));
    if !is_tie {
        return None;
    }

    let rounded_int = (scaled >> q) + 1;

    let ten_pow = 10u128.pow(precision_u32);
    let int_part = rounded_int / ten_pow;
    let frac_part = rounded_int % ten_pow;
    let sign_str = if sign.is_negative() { "-" } else { "" };
    let s = format!("{sign_str}{int_part}.{frac_part:0precision$}");

    let result: f64 = str_to_f64(s)
        .expect("constructed digit string should always parse")
        .into();
    Some(result)
}

/// The text of a REAL, as SQLite writes it with `%!.17g`: CAST(x AS TEXT),
/// `||`, a REAL read as TEXT.
pub fn format_float(v: f64) -> String {
    let mut out = String::with_capacity(24);
    let format = fp::RealFormat {
        alt_form_2: true,
        ..fp::RealFormat::new(fp::RealConversion::General, 17)
    };
    fp::push_real(&mut out, v, &format);
    out
}

/// The decimal text of an `i64`, the same text as `i64::to_string()`, in a
/// stack buffer.
pub(crate) struct IntegerText {
    buf: [u8; 20],
    start: usize,
}

impl IntegerText {
    // Not inlined, so that its callers, such as the operands of `||`, stay
    // small enough for the compiler to inline them.
    #[inline(never)]
    pub(crate) fn new(value: i64) -> Self {
        let mut buf = [0u8; 20];
        let digits = write_decimal_digits(&mut buf, value.unsigned_abs()).len();
        let mut start = buf.len() - digits;
        if value < 0 {
            start -= 1;
            buf[start] = b'-';
        }
        Self { buf, start }
    }

    pub(crate) fn as_str(&self) -> &str {
        // SAFETY: the text is ASCII digits, after a minus sign when negative.
        unsafe { str::from_utf8_unchecked(&self.buf[self.start..]) }
    }
}

/// Write the decimal digits of `n` at the end of `buf`, and return them.
// Not inlined: in a caller that owns `buf`, the compiler unrolls the loop for
// all 20 digits and keeps each byte of `buf` in a register. That makes the
// caller several kilobytes larger, and each byte of the result is moved one at
// a time.
#[inline(never)]
pub(crate) fn write_decimal_digits(buf: &mut [u8; 20], mut n: u64) -> &[u8] {
    let mut start = buf.len();
    // Two digits for each division, from a table of the pairs.
    while n >= 100 {
        let pair = usize::from((n % 100) as u8) * 2;
        n /= 100;
        start -= 2;
        buf[start..start + 2].copy_from_slice(&DIGIT_PAIRS.as_bytes()[pair..pair + 2]);
    }
    if n >= 10 {
        let pair = usize::from(n as u8) * 2;
        start -= 2;
        buf[start..start + 2].copy_from_slice(&DIGIT_PAIRS.as_bytes()[pair..pair + 2]);
    } else {
        start -= 1;
        buf[start] = b'0' + n as u8;
    }
    &buf[start..]
}

/// The numbers from 0 to 99, each as two ASCII digits.
pub(crate) const DIGIT_PAIRS: &str = "\
    0001020304050607080910111213141516171819\
    2021222324252627282930313233343536373839\
    4041424344454647484950515253545556575859\
    6061626364656667686970717273747576777879\
    8081828384858687888990919293949596979899";

/// The text of a REAL in an SQL statement, as quote() writes it with
/// `%!0.17g`: an infinity is 9.0e+999.
pub fn format_float_for_quote(v: f64) -> String {
    if v.is_nan() {
        return "NULL".to_string();
    }
    let mut out = String::with_capacity(24);
    let format = fp::RealFormat {
        alt_form_2: true,
        zero_pad: true,
        ..fp::RealFormat::new(fp::RealConversion::General, 17)
    };
    fp::push_real(&mut out, v, &format);
    out
}

#[test]
fn test_decode_float() {
    assert_eq!(format_float(9.93e-322), "9.9307194814090555e-322");
    assert_eq!(format_float(9.93), "9.93");
    assert_eq!(format_float(0.093), "0.093");
    assert_eq!(format_float(-0.093), "-0.093");
    assert_eq!(format_float(0.0), "0.0");
    assert_eq!(format_float(4.94e-322), "4.9406564584124654e-322");
    assert_eq!(format_float(-20228007.0), "-20228007.0");
    assert_eq!(format_float(9.999999999999998), "9.9999999999999982");
}

#[test]
fn integer_text_matches_to_string() {
    for value in (-100_000i64..100_000).chain([
        i64::MIN,
        i64::MIN + 1,
        i64::MAX,
        i64::MAX - 1,
        i64::from(i32::MIN),
        i64::from(u32::MAX),
        10_i64.pow(18),
        -(10_i64.pow(18)),
    ]) {
        assert_eq!(IntegerText::new(value).as_str(), value.to_string());
    }
}

#[test]
fn test_format_float_for_quote() {
    assert_eq!(format_float_for_quote(f64::INFINITY), "9.0e+999");
    assert_eq!(format_float_for_quote(f64::NEG_INFINITY), "-9.0e+999");
    assert_eq!(format_float_for_quote(f64::NAN), "NULL");
    assert_eq!(format_float_for_quote(12.34), "12.34");
    assert_eq!(format_float_for_quote(0.0), "0.0");
}

#[test]
fn test_round_half_away_from_zero_tie() {
    assert_eq!(round_half_away_from_zero_tie(2.25, 1), Some(2.3));
    assert_eq!(round_half_away_from_zero_tie(0.125, 2), Some(0.13));
    assert_eq!(round_half_away_from_zero_tie(1.125, 2), Some(1.13));
    assert_eq!(round_half_away_from_zero_tie(-2.25, 1), Some(-2.3));
    assert_eq!(round_half_away_from_zero_tie(2.35, 1), None);
    assert_eq!(round_half_away_from_zero_tie(1.005, 2), None);
    assert_eq!(round_half_away_from_zero_tie(5e-320, 10), None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn str_to_f64_reads_the_number_at_the_start_of_the_text() {
        let parts = |text: &str| {
            str_to_f64(text).map(|result| match result {
                StrToF64::Decimal(v) => ("Decimal", f64::from(v)),
                StrToF64::Fractional(v) => ("Fractional", f64::from(v)),
                StrToF64::DecimalPrefix(v) => ("DecimalPrefix", f64::from(v)),
                StrToF64::FractionalPrefix(v) => ("FractionalPrefix", f64::from(v)),
            })
        };
        let cases = [
            ("12", Some(("Decimal", 12.0))),
            (" +12.5e+3 ", Some(("Fractional", 12500.0))),
            ("-0.25", Some(("Fractional", -0.25))),
            ("12.5e", Some(("FractionalPrefix", 12.5))),
            ("12.5e+", Some(("FractionalPrefix", 12.5))),
            ("1\u{e9}", Some(("DecimalPrefix", 1.0))),
            ("\u{e9}1", None),
            (".e1", None),
            ("1.e1", Some(("Fractional", 10.0))),
            ("0x10", Some(("DecimalPrefix", 0.0))),
            (
                "18446744073709551616",
                Some(("Decimal", 1.8446744073709552e19)),
            ),
            ("1e400", Some(("Fractional", f64::INFINITY))),
            ("\u{b}7\u{b}", Some(("Decimal", 7.0))),
            ("", None),
            ("-", None),
            (".", None),
        ];
        for (text, expected) in cases {
            assert_eq!(parts(text), expected, "{text:?}");
        }
    }
}
