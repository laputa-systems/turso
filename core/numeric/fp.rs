//! The conversions between REAL and decimal text of SQLite 3.53.4:
//! `sqlite3AtoF`, `sqlite3FpDecode` and the floating-point conversions of
//! `sqlite3_str_vappendf`. They give the same bits and the same text as
//! SQLite, so they keep its tables and its integer arithmetic.

use super::write_decimal_digits;

/// The kind of a floating-point conversion of printf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RealConversion {
    /// `%f`
    Fixed,
    /// `%e` and `%E`
    Exponent,
    /// `%g` and `%G`
    General,
}

/// A floating-point conversion of printf with its flags, width and
/// precision.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RealFormat {
    pub conversion: RealConversion,
    pub uppercase: bool,
    pub precision: Option<usize>,
    pub width: usize,
    pub left_justify: bool,
    /// `+` or a space before a number that is not negative.
    pub sign: Option<u8>,
    /// The `#` flag.
    pub alternate: bool,
    /// The `!` flag.
    pub alt_form_2: bool,
    pub zero_pad: bool,
    /// The `,` flag.
    pub thousands: bool,
}

impl RealFormat {
    /// The conversion with `precision` and no flags or width.
    pub const fn new(conversion: RealConversion, precision: usize) -> Self {
        Self {
            conversion,
            uppercase: false,
            precision: Some(precision),
            width: 0,
            left_justify: false,
            sign: None,
            alternate: false,
            alt_form_2: false,
            zero_pad: false,
            thousands: false,
        }
    }
}

/// Append `value` as the etFLOAT, etEXP and etGENERIC conversions of
/// `sqlite3_str_vappendf` write it.
pub(crate) fn push_real(out: &mut String, value: f64, format: &RealFormat) {
    let mut text = Vec::with_capacity(32);
    write_real(&mut text, value, format);
    out.push_str(std::str::from_utf8(&text).expect("write_real writes only ASCII bytes"));
}

fn write_real(out: &mut Vec<u8>, value: f64, format: &RealFormat) {
    let mut conversion = format.conversion;
    let mut precision = format.precision.map_or(6, |p| p as i64);
    let i_round = match conversion {
        RealConversion::Fixed => -precision,
        RealConversion::General => {
            if precision == 0 {
                precision = 1;
            }
            precision
        }
        RealConversion::Exponent => precision + 1,
    };
    let i_round = i_round.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    let mut s = FpDecode::new(value, i_round, if format.alt_form_2 { 20 } else { 16 });
    match s.special {
        Some(FpSpecial::NaN) => {
            let text: &[u8] = if format.zero_pad { b"null" } else { b"NaN" };
            pad_with_spaces(out, text, format);
            return;
        }
        Some(FpSpecial::Infinity) if format.zero_pad => {
            s.buf[0] = b'9';
            s.start = 0;
            s.i_dp = 1000;
            s.n = 1;
        }
        Some(FpSpecial::Infinity) => {
            let text: &[u8] = match (s.negative, format.sign) {
                (true, _) => b"-Inf",
                (false, Some(b'+')) => b"+Inf",
                (false, Some(_)) => b" Inf",
                (false, None) => b"Inf",
            };
            pad_with_spaces(out, text, format);
            return;
        }
        None => {}
    }
    let prefix = if s.negative {
        if format.alternate
            && format.sign.is_none()
            && conversion == RealConversion::Fixed
            && s.i_dp <= i_round
        {
            None
        } else {
            Some(b'-')
        }
    } else {
        format.sign
    };
    let exp = s.i_dp - 1;
    let flag_rtz;
    if conversion == RealConversion::General {
        precision -= 1;
        flag_rtz = !format.alternate;
        if exp < -4 || i64::from(exp) > precision {
            conversion = RealConversion::Exponent;
        } else {
            precision -= i64::from(exp);
            conversion = RealConversion::Fixed;
        }
    } else {
        flag_rtz = format.alt_form_2;
    }
    let mut e2 = if conversion == RealConversion::Exponent {
        0
    } else {
        s.i_dp - 1
    };
    let start = out.len();
    let flag_dp = precision > 0 || format.alternate || format.alt_form_2;
    if let Some(prefix) = prefix {
        out.push(prefix);
    }
    let digits = s.digits();
    let mut j = 0;
    if e2 < 0 {
        out.push(b'0');
    } else if format.thousands {
        while e2 >= 0 {
            out.push(if j < digits.len() {
                j += 1;
                digits[j - 1]
            } else {
                b'0'
            });
            if e2 % 3 == 0 && e2 > 1 {
                out.push(b',');
            }
            e2 -= 1;
        }
    } else {
        j = (e2 as usize + 1).min(digits.len());
        out.extend_from_slice(&digits[..j]);
        e2 -= j as i32;
        if e2 >= 0 {
            out.resize(out.len() + e2 as usize + 1, b'0');
            e2 = -1;
        }
    }
    if flag_dp {
        out.push(b'.');
    }
    if e2 < -1 && precision > 0 {
        let nn = i64::from(-1 - e2).min(precision);
        out.resize(out.len() + nn as usize, b'0');
        precision -= nn;
    }
    if precision > 0 {
        let nn = ((digits.len() - j) as i64).min(precision);
        if nn > 0 {
            out.extend_from_slice(&digits[j..j + nn as usize]);
            precision -= nn;
        }
        if precision > 0 && !flag_rtz {
            out.resize(out.len() + precision as usize, b'0');
        }
    }
    if flag_rtz && flag_dp {
        while out.last() == Some(&b'0') {
            out.pop();
        }
        if out.last() == Some(&b'.') {
            if format.alt_form_2 {
                out.push(b'0');
            } else {
                out.pop();
            }
        }
    }
    if conversion == RealConversion::Exponent {
        let mut exp = s.i_dp - 1;
        out.push(if format.uppercase { b'E' } else { b'e' });
        if exp < 0 {
            out.push(b'-');
            exp = -exp;
        } else {
            out.push(b'+');
        }
        if exp >= 100 {
            out.push((exp / 100) as u8 + b'0');
            exp %= 100;
        }
        out.push((exp / 10) as u8 + b'0');
        out.push((exp % 10) as u8 + b'0');
    }
    let length = out.len() - start;
    if length < format.width {
        let pad = format.width - length;
        if format.left_justify {
            out.resize(out.len() + pad, b' ');
        } else {
            let at = start + usize::from(format.zero_pad && prefix.is_some());
            let fill = if format.zero_pad { b'0' } else { b' ' };
            out.splice(at..at, std::iter::repeat_n(fill, pad));
        }
    }
}

fn pad_with_spaces(out: &mut Vec<u8>, text: &[u8], format: &RealFormat) {
    let pad = format.width.saturating_sub(text.len());
    if !format.left_justify {
        out.resize(out.len() + pad, b' ');
    }
    out.extend_from_slice(text);
    if format.left_justify {
        out.resize(out.len() + pad, b' ');
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FpSpecial {
    Infinity,
    NaN,
}

/// `FpDecode`: the significant decimal digits of a double and the position
/// of its decimal point, after the rounding of `sqlite3FpDecode`.
struct FpDecode {
    negative: bool,
    special: Option<FpSpecial>,
    buf: [u8; 21],
    start: usize,
    n: usize,
    i_dp: i32,
}

impl FpDecode {
    fn digits(&self) -> &[u8] {
        &self.buf[self.start..self.start + self.n]
    }

    /// `sqlite3FpDecode(p, r, iRound, mxRound)`. When `i_round` is zero or
    /// less, the digits are rounded to `-i_round` places after the decimal
    /// point, or to `max_round` significant digits. Otherwise they are
    /// rounded to `min(i_round, max_round)` significant digits.
    fn new(r: f64, mut i_round: i32, max_round: i32) -> Self {
        debug_assert!(max_round > 0);
        let mut p = FpDecode {
            negative: false,
            special: None,
            buf: [0; 21],
            start: 0,
            n: 0,
            i_dp: 0,
        };
        let mut r = r;
        if r < 0.0 {
            p.negative = true;
            r = -r;
        } else if r == 0.0 {
            p.buf[0] = b'0';
            p.n = 1;
            p.i_dp = 1;
            return p;
        }
        let bits = r.to_bits();
        let biased_exponent = ((bits >> 52) & 0x7ff) as i32;
        if biased_exponent == 0x7ff {
            p.special = Some(if bits == 0x7ff0000000000000 {
                FpSpecial::Infinity
            } else {
                FpSpecial::NaN
            });
            return p;
        }
        let mut v = bits & 0x000fffffffffffff;
        let e = if biased_exponent == 0 {
            let nn = v.leading_zeros() as i32;
            v <<= nn;
            -1074 - nn
        } else {
            v = (v << 11) | (1 << 63);
            biased_exponent - 1086
        };
        let digits_wanted = if i_round <= 0 || i_round >= 18 {
            18
        } else {
            i_round + 1
        };
        let (v, exp) = fp2_convert10(v, e, digits_wanted);
        debug_assert!(v > 0);
        let digits: &mut [u8; 20] = (&mut p.buf[..20])
            .try_into()
            .expect("the buffer has 20 digit bytes before its last byte");
        let mut n = write_decimal_digits(digits, v).len();
        let mut i = 20 - n;
        p.i_dp = n as i32 + exp;
        if i_round <= 0 {
            i_round = p.i_dp - i_round;
            if i_round == 0 && p.buf[i] >= b'5' {
                i_round = 1;
                i -= 1;
                p.buf[i] = b'0';
                n += 1;
                p.i_dp += 1;
            }
        }
        let mut z = i;
        if i_round > 0 && ((i_round as usize) < n || n as i32 > max_round) {
            if i_round > max_round {
                i_round = max_round;
            }
            if i_round == 17 {
                // With precision 17, which only the "!" flag allows, SQLite
                // drops the last digits when fewer digits read back as the
                // same double, so that 49.47 is not 49.469999999999999.
                let digit_at = |k: usize| p.buf[z + k];
                let digit_value = |k: usize| u64::from(p.buf[z + k] - b'0');
                if digit_at(15) == b'9' && digit_at(14) == b'9' {
                    let mut jj = 14;
                    while jj > 0 && digit_at(jj - 1) == b'9' {
                        jj -= 1;
                    }
                    let v2 = if jj == 0 {
                        1
                    } else {
                        (1..jj).fold(digit_value(0), |acc, k| acc * 10 + digit_value(k)) + 1
                    };
                    if r == fp10_convert2(v2, exp + n as i32 - jj as i32) {
                        i_round = jj as i32 + 1;
                    }
                } else if p.i_dp >= n as i32
                    || (digit_at(15) == b'0' && digit_at(14) == b'0' && digit_at(13) == b'0')
                {
                    debug_assert!(digit_at(0) != b'0');
                    let mut jj = 13;
                    while digit_at(jj - 1) == b'0' {
                        jj -= 1;
                    }
                    let v2 = (1..jj).fold(digit_value(0), |acc, k| acc * 10 + digit_value(k));
                    if r == fp10_convert2(v2, exp + n as i32 - jj as i32) {
                        i_round = jj as i32 + 1;
                    }
                }
            }
            n = i_round as usize;
            if p.buf[z + n] >= b'5' {
                let mut j = n - 1;
                loop {
                    p.buf[z + j] += 1;
                    if p.buf[z + j] <= b'9' {
                        break;
                    }
                    p.buf[z + j] = b'0';
                    if j == 0 {
                        z -= 1;
                        p.buf[z] = b'1';
                        n += 1;
                        p.i_dp += 1;
                        break;
                    }
                    j -= 1;
                }
            }
        }
        debug_assert!(n > 0);
        while p.buf[z + n - 1] == b'0' {
            n -= 1;
            debug_assert!(n > 0);
        }
        p.start = z;
        p.n = n;
        p
    }
}

/// What [atof] read from the start of a text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AtoF {
    pub value: f64,
    /// The result code of `sqlite3AtoF`: 0 when no prefix of the text is a
    /// number, negative when only a prefix is, positive when all of it is.
    /// Bit 1 is set when the number has a decimal point or an exponent.
    pub rc: i32,
}

impl AtoF {
    pub fn is_number(&self) -> bool {
        self.rc > 0
    }

    pub fn has_prefix_number(&self) -> bool {
        self.rc != 0
    }

    pub fn is_fractional(&self) -> bool {
        self.rc & 2 != 0
    }
}

/// `sqlite3AtoF`: the REAL that the text starts with. The text ends at its
/// first NUL byte, as the C string that SQLite reads does. Leading and
/// trailing whitespace is ignored.
pub(crate) fn atof(text: &[u8]) -> AtoF {
    const LIMIT: u64 = (u64::MAX - 9) / 10;
    let at = |i: usize| match text.get(i) {
        Some(&byte) => byte,
        None => 0,
    };
    let digit = |byte: u8| byte.is_ascii_digit().then(|| u64::from(byte - b'0'));
    let mut z = 0;
    let mut negative = false;
    let mut s: u64 = 0;
    let mut d: i32 = 0;
    let mut state: i32 = 0;
    let mut integer_digit = None;
    loop {
        let c = at(z);
        if let Some(v) = digit(c) {
            integer_digit = Some(v);
        } else if c == b'-' || c == b'+' {
            negative = c == b'-';
            z += 1;
            integer_digit = digit(at(z));
        } else if is_space(c) {
            while is_space(at(z)) {
                z += 1;
            }
            continue;
        }
        break;
    }
    if let Some(v) = integer_digit {
        state = 1;
        s = v;
        z += 1;
        while let Some(v) = digit(at(z)) {
            s = s * 10 + v;
            z += 1;
            if s >= LIMIT {
                state = 9;
                while at(z).is_ascii_digit() {
                    z += 1;
                    d += 1;
                }
                break;
            }
        }
    }
    if at(z) == b'.' {
        z += 1;
        if at(z).is_ascii_digit() {
            state |= 1;
            while let Some(v) = digit(at(z)) {
                if s < LIMIT {
                    s = s * 10 + v;
                    d -= 1;
                } else {
                    state = 11;
                }
                z += 1;
            }
        } else if state == 0 {
            return AtoF { value: 0.0, rc: 0 };
        }
        state |= 2;
    } else if state == 0 {
        return AtoF { value: 0.0, rc: 0 };
    }
    if matches!(at(z), b'e' | b'E') {
        z += 1;
        let exponent_sign = if at(z) == b'-' {
            z += 1;
            -1
        } else {
            if at(z) == b'+' {
                z += 1;
            }
            1
        };
        if let Some(v) = digit(at(z)) {
            let mut exponent = v as i32;
            z += 1;
            state |= 2;
            while let Some(v) = digit(at(z)) {
                exponent = if exponent < 10000 {
                    exponent * 10 + v as i32
                } else {
                    10000
                };
                z += 1;
            }
            d += exponent_sign * exponent;
        } else {
            z -= 1;
        }
    }
    let mut value = if s == 0 {
        state |= 4;
        0.0
    } else {
        fp10_convert2(s, d)
    };
    if negative {
        value = -value;
    }
    if at(z) == 0 {
        return AtoF { value, rc: state };
    }
    if is_space(at(z)) {
        while is_space(at(z)) {
            z += 1;
        }
        if at(z) == 0 {
            return AtoF { value, rc: state };
        }
    }
    AtoF {
        value,
        rc: (0xfffffff0u32 | state as u32) as i32,
    }
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// For `r = m * 2^e`, where `m` has its highest bit set, a `d` with at least
/// `n` significant digits and a `p` such that `r` is about `d * 10^p`.
fn fp2_convert10(m: u64, e: i32, n: i32) -> (u64, i32) {
    debug_assert!((1..=18).contains(&n));
    let p = n - 1 - pwr2to10(e + 63);
    let (mut h, _) = multiply128(m, power_of_ten(p).0);
    let d = if n == 18 {
        h >>= -(e + pwr10to2(p) + 2);
        h.wrapping_add((h << 1) & 2) >> 1
    } else {
        h >> -(e + pwr10to2(p) + 1)
    };
    (d, -p)
}

/// The double closest to `d * 10^p`, by the algorithm of
/// <https://github.com/rsc/fpfmt> that SQLite adapts. `d` is not zero.
pub(crate) fn fp10_convert2(d: u64, p: i32) -> f64 {
    debug_assert!(d != 0);
    if p < POWERS_OF_10_FIRST {
        return 0.0;
    }
    if p > POWERS_OF_10_LAST {
        return f64::INFINITY;
    }
    let b = 64 - d.leading_zeros() as i32;
    let lp = pwr10to2(p);
    let mut e = 53 - b - lp;
    if e > 1074 {
        if e >= 1130 {
            return 0.0;
        }
        e = 1074;
    }
    let s = -(e - (64 - b) + lp + 3);
    let (mut pwr10h, mut pwr10l) = power_of_ten(p);
    if pwr10l != 0 {
        pwr10h = pwr10h.wrapping_add(1);
        pwr10l = !pwr10l;
    }
    let x = d << (64 - b);
    let (mut hi, lo) = multiply128(x, pwr10h);
    let mid1 = (lo >> 32) as u32;
    let mut sticky = 1;
    if hi & ((1u64 << s) - 1) == 0 {
        let mid2 = (multiply128(x, u64::from(pwr10l) << 32).0 >> 32) as u32;
        sticky = u64::from(mid1.wrapping_sub(mid2) > 1);
        hi = hi.wrapping_sub(u64::from(mid1 < mid2));
    }
    let mut u = (hi >> s) | sticky;
    if u >= (1 << 55) - 2 {
        u = (u >> 1) | (u & 1);
        e -= 1;
    }
    let mut m = (u + 1 + ((u >> 2) & 1)) >> 2;
    if e <= -972 {
        return f64::INFINITY;
    }
    if m & (1 << 52) != 0 {
        m = (m & !(1 << 52)) | ((1075 - e) as u64) << 52;
    }
    f64::from_bits(m)
}

/// `floor(log2(10^p))`, approximately.
fn pwr10to2(p: i32) -> i32 {
    (p * 108853) >> 15
}

/// `floor(log10(2^p))`, approximately.
fn pwr2to10(p: i32) -> i32 {
    (p * 78913) >> 18
}

const POWERS_OF_10_FIRST: i32 = -348;
const POWERS_OF_10_LAST: i32 = 347;

const POWER_OF_TEN_BASE: [u64; 27] = [
    0x8000000000000000,
    0xa000000000000000,
    0xc800000000000000,
    0xfa00000000000000,
    0x9c40000000000000,
    0xc350000000000000,
    0xf424000000000000,
    0x9896800000000000,
    0xbebc200000000000,
    0xee6b280000000000,
    0x9502f90000000000,
    0xba43b74000000000,
    0xe8d4a51000000000,
    0x9184e72a00000000,
    0xb5e620f480000000,
    0xe35fa931a0000000,
    0x8e1bc9bf04000000,
    0xb1a2bc2ec5000000,
    0xde0b6b3a76400000,
    0x8ac7230489e80000,
    0xad78ebc5ac620000,
    0xd8d726b7177a8000,
    0x878678326eac9000,
    0xa968163f0a57b400,
    0xd3c21bcecceda100,
    0x84595161401484a0,
    0xa56fa5b99019a5c8,
];

const POWER_OF_TEN_SCALE: [u64; 26] = [
    0x8049a4ac0c5811ae,
    0xcf42894a5dce35ea,
    0xa76c582338ed2621,
    0x873e4f75e2224e68,
    0xda7f5bf590966848,
    0xb080392cc4349dec,
    0x8e938662882af53e,
    0xe65829b3046b0afa,
    0xba121a4650e4ddeb,
    0x964e858c91ba2655,
    0xf2d56790ab41c2a2,
    0xc428d05aa4751e4c,
    0x9e74d1b791e07e48,
    0xcccccccccccccccc,
    0xcecb8f27f4200f3a,
    0xa70c3c40a64e6c51,
    0x86f0ac99b4e8dafd,
    0xda01ee641a708de9,
    0xb01ae745b101e9e4,
    0x8e41ade9fbebc27d,
    0xe5d3ef282a242e81,
    0xb9a74a0637ce2ee1,
    0x95f83d0a1fb69cd9,
    0xf24a01a73cf2dccf,
    0xc3b8358109e84f07,
    0x9e19db92b4e31ba9,
];

const POWER_OF_TEN_SCALE_LO: [u32; 26] = [
    0x205b896d, 0x52064cad, 0xaf2af2b8, 0x5a7744a7, 0xaf39a475, 0xbd8d794e, 0x547eb47b,
    0x0cb4a5a3, 0x92f34d62, 0x3a6a07f9, 0xfae27299, 0xaa97e14c, 0x775ea265, 0xcccccccc,
    0x00000000, 0x999090b6, 0x69a028bb, 0xe80e6f48, 0x5ec05dd0, 0x14588f14, 0x8f1668c9,
    0x6d953e2c, 0x4abdaf10, 0xbc633b39, 0x0a862f81, 0x6c07a2c2,
];

/// The 64 most significant bits of `10^p`, and the 32 bits after them, for
/// `p` from -348 to 347.
fn power_of_ten(p: i32) -> (u64, u32) {
    debug_assert!((POWERS_OF_10_FIRST..=POWERS_OF_10_LAST).contains(&p));
    let (g, n) = if p < 0 {
        if p == -1 {
            return (POWER_OF_TEN_SCALE[13], POWER_OF_TEN_SCALE_LO[13]);
        }
        let (g, n) = (p / 27, p % 27);
        if n != 0 {
            (g - 1, n + 27)
        } else {
            (g, n)
        }
    } else if p < 27 {
        return (POWER_OF_TEN_BASE[p as usize], 0);
    } else {
        (p / 27, p % 27)
    };
    let index = (g + 13) as usize;
    let s = POWER_OF_TEN_SCALE[index];
    if n == 0 {
        return (s, POWER_OF_TEN_SCALE_LO[index]);
    }
    let (mut x, mut lo) = multiply160(
        s,
        POWER_OF_TEN_SCALE_LO[index],
        POWER_OF_TEN_BASE[n as usize],
    );
    if x & (1 << 63) == 0 {
        x = (x << 1) | u64::from((lo >> 31) & 1);
        lo = (lo << 1) | 1;
    }
    (x, lo)
}

/// The high 64 bits of `a * b`, and the low 64 bits.
fn multiply128(a: u64, b: u64) -> (u64, u64) {
    let r = u128::from(a) * u128::from(b);
    ((r >> 64) as u64, r as u64)
}

/// The upper 96 bits of the 160-bit product of the 96-bit `(a << 32) + a_lo`
/// and `b`: the upper 64 bits, and the 32 bits after them.
fn multiply160(a: u64, a_lo: u32, b: u64) -> (u64, u32) {
    let mut r = u128::from(a) * u128::from(b);
    r += (u128::from(a_lo) * u128::from(b)) >> 32;
    ((r >> 64) as u64, (r >> 32) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn printf(format: &RealFormat, value: f64) -> String {
        let mut out = String::new();
        push_real(&mut out, value, format);
        out
    }

    #[test]
    fn real_text_has_at_most_17_digits_and_drops_noise_digits_that_read_back() {
        let format = RealFormat {
            alt_form_2: true,
            ..RealFormat::new(RealConversion::General, 17)
        };
        let cases = [
            (0.1, "0.1"),
            (1.0 / 3.0, "0.33333333333333332"),
            (49.47, "49.47"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1.0, "1.0"),
            (100.0, "100.0"),
            (1e16, "10000000000000000.0"),
            (1e17, "1.0e+17"),
            (1e20, "1.0e+20"),
            (1e-5, "1.0e-05"),
            (2.5e300, "2.5e+300"),
            (-0.0, "0.0"),
            (-20228007.0, "-20228007.0"),
            (9.999999999999998, "9.9999999999999982"),
            (std::f64::consts::PI, "3.1415926535897931"),
            (5e-324, "4.9406564584124654e-324"),
            (9.93e-322, "9.9307194814090555e-322"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (f64::INFINITY, "Inf"),
            (f64::NEG_INFINITY, "-Inf"),
        ];
        for (value, text) in cases {
            assert_eq!(printf(&format, value), text, "%!.17g of {value:e}");
        }
    }

    #[test]
    fn printf_conversions_round_the_decimal_digits_of_the_double() {
        let general = |precision| RealFormat::new(RealConversion::General, precision);
        let fixed = |precision| RealFormat::new(RealConversion::Fixed, precision);
        let exponent = |precision| RealFormat::new(RealConversion::Exponent, precision);
        let cases = [
            (general(15), 9.93e-322, "9.93071948140906e-322"),
            (general(15), 1e15 + 0.3, "1e+15"),
            (
                RealFormat {
                    alt_form_2: true,
                    ..exponent(20)
                },
                9.93,
                "9.92999999999999972e+00",
            ),
            (
                RealFormat {
                    alternate: true,
                    ..general(6)
                },
                204274.7795102219,
                "204275.",
            ),
            (
                RealFormat {
                    thousands: true,
                    ..fixed(2)
                },
                -20228007.0,
                "-20,228,007.00",
            ),
            (
                RealFormat {
                    zero_pad: true,
                    width: 10,
                    ..exponent(4)
                },
                -0.093,
                "-9.3000e-02",
            ),
            (
                RealFormat {
                    left_justify: true,
                    width: 8,
                    ..fixed(1)
                },
                0.093,
                "0.1     ",
            ),
            (
                RealFormat {
                    sign: Some(b'+'),
                    ..exponent(3)
                },
                0.0,
                "+0.000e+00",
            ),
            (
                fixed(2),
                1.7976931348623157e308,
                &format!("1797693134862316{}.00", "0".repeat(309 - 16)),
            ),
            (
                RealFormat {
                    zero_pad: true,
                    alt_form_2: true,
                    ..general(17)
                },
                f64::NEG_INFINITY,
                "-9.0e+999",
            ),
        ];
        for (format, value, text) in cases {
            assert_eq!(printf(&format, value), text, "{format:?} of {value:e}");
        }
    }

    #[test]
    fn atof_reads_at_most_19_digits_of_the_text() {
        let read = |text: &[u8]| atof(text);
        assert_eq!(read(b"0.1").value, 0.1);
        assert_eq!(read(b"49.47").value, 49.47);
        // The 23rd significant digit would round this up to ...0.5.
        assert_eq!(read(b"3500000000000000.2500001").value, 3500000000000000.0);
        assert_eq!(read(b"1e400").value, f64::INFINITY);
        assert_eq!(read(b"1e-400").value, 0.0);
        assert_eq!(read(b"-0").value.to_bits(), (-0.0f64).to_bits());
        assert_eq!(read(b"4.9e-324").value, 5e-324);
    }

    #[test]
    fn atof_result_code_tells_whole_numbers_from_prefixes() {
        let rc = |text: &[u8]| atof(text).rc;
        assert_eq!(rc(b" 12 "), 1);
        assert_eq!(rc(b"12.5"), 3);
        assert_eq!(rc(b"0"), 5);
        assert_eq!(rc(b"18446744073709551616"), 9);
        assert_eq!(rc(b"12\0junk"), 1);
        assert!(rc(b"12abc") < 0);
        assert!(rc(b"1e+") < 0);
        assert_eq!(rc(b"abc"), 0);
        assert_eq!(rc(b"."), 0);
        assert_eq!(rc(b""), 0);
    }
}
