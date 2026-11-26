use std::fmt::Write;
use std::iter::{repeat_n, Peekable};
use std::str;
use std::str::Chars;

use crate::numeric::fp::{self, RealConversion, RealFormat};
use crate::numeric::{format_float, str_to_i64, Numeric};
use crate::types::Value;
use crate::vdbe::Register;

#[derive(Default, Clone)]
struct FormatFlags {
    left_justify: bool,
    force_sign: bool,
    space_sign: bool,
    zero_pad: bool,
    alternate: bool,
    comma_sep: bool,
    alt_form_2: bool,
}

struct FormatSpec {
    flags: FormatFlags,
    width: Option<usize>,
    precision: Option<usize>,
    spec_type: char,
}

/// Consume flag characters after '%', returning the FormatFlags.
/// In SQLite, later flags override earlier ones for +/space conflicts.
fn parse_flags(chars: &mut Peekable<Chars>) -> FormatFlags {
    let mut flags = FormatFlags::default();
    while let Some(&c) = chars.peek() {
        match c {
            '-' => {
                flags.left_justify = true;
            }
            '+' => {
                flags.force_sign = true;
                flags.space_sign = false;
            }
            ' ' => {
                flags.space_sign = true;
                flags.force_sign = false;
            }
            '0' => {
                flags.zero_pad = true;
            }
            '#' => {
                flags.alternate = true;
            }
            ',' => {
                flags.comma_sep = true;
            }
            '!' => {
                flags.alt_form_2 = true;
            }
            _ => break,
        }
        chars.next();
    }
    flags
}

/// Parse a decimal integer from the character stream.
fn parse_number(chars: &mut Peekable<Chars>) -> Option<usize> {
    let mut n: usize = 0;
    let mut found = false;
    while let Some(&c) = chars.peek() {
        if let Some(d) = c.to_digit(10) {
            n = n.saturating_mul(10).saturating_add(d as usize);
            found = true;
            chars.next();
        } else {
            break;
        }
    }
    if found {
        Some(n)
    } else {
        None
    }
}

/// Maximum width/precision to prevent OOM from adversarial format strings.
/// SQLite uses int for width and caps output at SQLITE_MAX_LENGTH (1 billion),
/// but we use a tighter limit since f64 only has ~17 meaningful digits anyway.
const MAX_WIDTH: usize = 1_000_000;

/// Parse a full format specifier after the initial '%'.
/// May consume arguments from `values` for `*` width/precision.
fn parse_format_spec(
    chars: &mut Peekable<Chars>,
    values: &[&Value],
    args_index: &mut usize,
) -> FormatSpec {
    let mut flags = parse_flags(chars);

    // Width — SQLite casts * width to C int (int32)
    let width = if chars.peek() == Some(&'*') {
        chars.next();
        let w = get_arg_i64(values, args_index) as i32;
        if w < 0 {
            // Negative width means left-justify
            flags.left_justify = true;
            // SQLite: width = width >= -2147483647 ? -width : 0
            if w > i32::MIN {
                Some(((-w) as usize).min(MAX_WIDTH))
            } else {
                Some(0)
            }
        } else {
            Some((w as usize).min(MAX_WIDTH))
        }
    } else {
        parse_number(chars).map(|w| w.min(MAX_WIDTH))
    };

    // Precision — SQLite casts * precision to C int (int32), then negates if negative.
    // For i32::MIN, negation wraps back to i32::MIN (still negative), treated as 0.
    let precision = if chars.peek() == Some(&'.') {
        chars.next();
        if chars.peek() == Some(&'*') {
            chars.next();
            // SQLite negates a negative precision, and uses the default
            // precision for i32::MIN, whose negation does not fit.
            let p = get_arg_i64(values, args_index) as i32;
            p.checked_abs().map(|p| (p as usize).min(MAX_WIDTH))
        } else {
            Some(parse_number(chars).unwrap_or(0).min(MAX_WIDTH))
        }
    } else {
        None
    };

    // Skip length modifiers (l, ll, h, hh) - ignored in SQL context
    while matches!(chars.peek(), Some(&'l') | Some(&'h')) {
        chars.next();
    }

    // Type character
    let spec_type = chars.next().unwrap_or('\0');

    FormatSpec {
        flags,
        width,
        precision,
        spec_type,
    }
}

// ── Coercion helpers ────────────────────────────────────────────

/// Get the next argument as i64 (for * width/precision), advancing args_index.
fn get_arg_i64(args: &[&Value], args_index: &mut usize) -> i64 {
    if *args_index >= args.len() {
        return 0;
    }
    let val = coerce_to_i64(args[*args_index]);
    *args_index += 1;
    val
}

/// Coerce a Value to i64 for integer specifiers.
fn coerce_to_i64(value: &Value) -> i64 {
    match value {
        Value::Numeric(Numeric::Integer(i)) => *i,
        Value::Numeric(Numeric::Float(f)) => f64::from(*f) as i64,
        Value::Text(t) => str_to_i64(t.as_str()).unwrap_or(0),
        Value::Blob(b) => {
            let s = String::from_utf8_lossy(b);
            str_to_i64(s.as_ref()).unwrap_or(0)
        }
        Value::Null => 0,
    }
}

/// Coerce a Value to f64 for float specifiers, as sqlite3_value_double does.
fn coerce_to_f64(value: &Value) -> f64 {
    match value {
        Value::Numeric(Numeric::Float(f)) => f64::from(*f),
        Value::Numeric(Numeric::Integer(i)) => *i as f64,
        Value::Text(t) => Numeric::text_to_real(t.as_str().as_bytes()),
        Value::Blob(b) => Numeric::text_to_real(b),
        Value::Null => 0.0,
    }
}

/// The part of `s` before the first NUL character. SQLite reads the format
/// string and string arguments as C strings, so they end at the first NUL.
fn until_nul(s: &str) -> &str {
    s.find('\0').map_or(s, |end| &s[..end])
}

/// Coerce a Value to String for string specifiers.
fn coerce_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Numeric(Numeric::Integer(i)) => i.to_string(),
        Value::Numeric(Numeric::Float(f)) => format_float(f64::from(*f)),
        Value::Text(t) => until_nul(t.as_str()).to_string(),
        Value::Blob(b) => until_nul(&String::from_utf8_lossy(b)).to_string(),
    }
}

// ── Formatting helpers ──────────────────────────────────────────

/// Insert comma separators into a digit string (e.g. "1234567" → "1,234,567").
fn insert_commas(digits: &str) -> String {
    let bytes = digits.as_bytes();
    let len = bytes.len();
    if len <= 3 {
        return digits.to_string();
    }
    let mut result = String::with_capacity(len + len / 3);
    let first_group = len % 3;
    if first_group > 0 {
        result.push_str(&digits[..first_group]);
    }
    for chunk in digits.as_bytes()[first_group..].chunks(3) {
        if !result.is_empty() {
            result.push(',');
        }
        // digits is always ASCII [0-9], guaranteed valid UTF-8
        result.push_str(str::from_utf8(chunk).expect("digit string is ASCII"));
    }
    result
}

/// Apply width padding to content. `sign_prefix` is prepended before content
/// and is part of the total width calculation.
/// `zero_overrides_left`: when true, the `0` flag takes priority over `-` (SQLite
/// integer specifiers %d/%u/%x/%o). For float specifiers, pass false so that `-`
/// overrides `0` per standard C behavior.
fn apply_width(
    output: &mut String,
    sign_prefix: &str,
    content: &str,
    width: Option<usize>,
    flags: &FormatFlags,
    zero_overrides_left: bool,
) {
    // Width counts bytes by default and characters with the ! flag, as
    // SQLite measures it.
    let measure = |t: &str| {
        if flags.alt_form_2 {
            t.chars().count()
        } else {
            t.len()
        }
    };
    let total_len = measure(sign_prefix) + measure(content);
    let w = width.unwrap_or(0);
    if total_len >= w {
        output.push_str(sign_prefix);
        output.push_str(content);
        return;
    }
    let pad_len = w - total_len;
    if flags.zero_pad && (zero_overrides_left || !flags.left_justify) {
        output.push_str(sign_prefix);
        for _ in 0..pad_len {
            output.push('0');
        }
        output.push_str(content);
    } else if flags.left_justify {
        output.push_str(sign_prefix);
        output.push_str(content);
        for _ in 0..pad_len {
            output.push(' ');
        }
    } else {
        for _ in 0..pad_len {
            output.push(' ');
        }
        output.push_str(sign_prefix);
        output.push_str(content);
    }
}

/// Compute sign prefix for a numeric value.
fn sign_prefix(negative: bool, flags: &FormatFlags) -> &'static str {
    if negative {
        "-"
    } else if flags.force_sign {
        "+"
    } else if flags.space_sign {
        " "
    } else {
        ""
    }
}

/// Pad a digit string to at least `min_digits` characters with leading zeros.
fn pad_with_precision(digits: String, precision: Option<usize>) -> String {
    let min_digits = precision.unwrap_or(1);
    if digits.len() < min_digits {
        "0".repeat(min_digits - digits.len()) + &digits
    } else {
        digits
    }
}

/// Limit a formatted numeric string to `max_sig` significant digits, rounding
/// at the boundary. This matches SQLite's behavior of not showing IEEE 754
/// mantissa noise beyond the float's representable precision.
#[cfg(test)]
fn limit_significant_digits(s: &str, max_sig: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut result: Vec<char> = chars.clone();

    // Find positions of all digits and track significant digit count
    let mut digit_positions: Vec<usize> = Vec::new();
    let mut sig_count = 0;
    let mut first_nonzero = false;
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_ascii_digit() {
            continue;
        }
        if c != '0' || first_nonzero {
            first_nonzero = true;
            sig_count += 1;
        }
        digit_positions.push(i);
    }

    if sig_count <= max_sig {
        return s.to_string();
    }

    // Find the index in digit_positions where the (max_sig+1)th significant digit is
    let mut sig_seen = 0;
    let mut round_pos = None; // position of the (max_sig+1)th sig digit
    let mut last_sig_pos = None; // position of the max_sig-th sig digit
    let mut first_nonzero2 = false;
    for &pos in &digit_positions {
        let c = chars[pos];
        if c != '0' || first_nonzero2 {
            first_nonzero2 = true;
            sig_seen += 1;
        }
        if sig_seen == max_sig {
            last_sig_pos = Some(pos);
        }
        if sig_seen == max_sig + 1 {
            round_pos = Some(pos);
            break;
        }
    }

    let (Some(round_pos), Some(_last_sig_pos)) = (round_pos, last_sig_pos) else {
        return s.to_string();
    };

    // Check if we need to round up (digit at round_pos >= 5)
    let round_digit = chars[round_pos].to_digit(10).unwrap();

    // Zero out all digits from round_pos onward
    for &pos in &digit_positions {
        if pos >= round_pos {
            result[pos] = '0';
        }
    }

    // If round digit >= 5, propagate carry backwards
    if round_digit >= 5 {
        // Walk backwards through digit positions before round_pos
        let mut carry = true;
        for &pos in digit_positions.iter().rev() {
            if pos >= round_pos {
                continue;
            }
            if !carry {
                break;
            }
            let d = result[pos].to_digit(10).unwrap() + 1;
            if d >= 10 {
                result[pos] = '0';
            } else {
                result[pos] = char::from_digit(d, 10).unwrap();
                carry = false;
            }
        }
        // If carry propagated past all digits, insert a '1' before the first digit
        if carry {
            let first_digit_pos = digit_positions[0];
            result.insert(first_digit_pos, '1');
        }
    }

    result.into_iter().collect()
}

// ── Per-specifier formatters ────────────────────────────────────

fn format_signed_int(output: &mut String, value: &Value, spec: &FormatSpec) {
    let i = coerce_to_i64(value);
    let negative = i < 0;
    let digits = i.unsigned_abs().to_string();

    let mut padded = pad_with_precision(digits, spec.precision);

    let prefix = sign_prefix(negative, &spec.flags);

    if spec.flags.comma_sep && spec.flags.zero_pad {
        // SQLite: zero-pad digits to (width - prefix.len()), then insert commas.
        // Commas are not counted in the width. Left-justify is ignored when
        // both comma and zero-pad are set.
        let w = spec.width.unwrap_or(0);
        let digit_target = w.saturating_sub(prefix.len());
        if padded.len() < digit_target {
            padded = "0".repeat(digit_target - padded.len()) + &padded;
        }
        output.push_str(prefix);
        output.push_str(&insert_commas(&padded));
    } else if spec.flags.comma_sep {
        padded = insert_commas(&padded);
        apply_width(output, prefix, &padded, spec.width, &spec.flags, true);
    } else {
        apply_width(output, prefix, &padded, spec.width, &spec.flags, true);
    }
}

fn format_unsigned_int(output: &mut String, value: &Value, spec: &FormatSpec) {
    let i = coerce_to_i64(value);
    let u = i as u64;
    let digits = u.to_string();

    let mut padded = pad_with_precision(digits, spec.precision);

    if spec.flags.comma_sep && spec.flags.zero_pad {
        // SQLite: zero-pad digits to width, then insert commas.
        // Commas are not counted in the width. Left-justify is ignored when
        // both comma and zero-pad are set.
        let w = spec.width.unwrap_or(0);
        if padded.len() < w {
            padded = "0".repeat(w - padded.len()) + &padded;
        }
        output.push_str(&insert_commas(&padded));
    } else if spec.flags.comma_sep {
        padded = insert_commas(&padded);
        apply_width(output, "", &padded, spec.width, &spec.flags, true);
    } else {
        apply_width(output, "", &padded, spec.width, &spec.flags, true);
    }
}

fn format_hex(output: &mut String, value: &Value, spec: &FormatSpec, uppercase: bool) {
    let i = coerce_to_i64(value);
    let u = i as u64;
    let digits = if uppercase {
        format!("{u:X}")
    } else {
        format!("{u:x}")
    };

    let padded = pad_with_precision(digits, spec.precision);

    let prefix = if spec.flags.alternate && u != 0 {
        // SQLite: %p always uses lowercase "0x" prefix even with uppercase digits.
        // %X uses "0X". Both from aPrefix[] in sqlite3.c:32037.
        if uppercase && spec.spec_type != 'p' {
            "0X"
        } else {
            "0x"
        }
    } else {
        ""
    };

    // In SQLite, when # and 0 flags are both set, width applies to digits only
    // and the prefix is added on top (not counted in width).
    if spec.flags.alternate && spec.flags.zero_pad && !prefix.is_empty() {
        let w = spec.width.unwrap_or(0);
        let zero_padded = if padded.len() < w {
            "0".repeat(w - padded.len()) + &padded
        } else {
            padded
        };
        output.push_str(prefix);
        output.push_str(&zero_padded);
    } else {
        apply_width(output, prefix, &padded, spec.width, &spec.flags, true);
    }
}

fn format_octal(output: &mut String, value: &Value, spec: &FormatSpec) {
    let i = coerce_to_i64(value);
    let u = i as u64;
    let digits = format!("{u:o}");

    let padded = pad_with_precision(digits, spec.precision);

    // SQLite always adds "0" prefix for octal with # flag when value is non-zero,
    // even if precision padding already added leading zeros.
    let prefix = if spec.flags.alternate && u != 0 {
        "0"
    } else {
        ""
    };

    // In SQLite, when # and 0 flags are both set, width applies to digits only
    // and the prefix is added on top (not counted in width).
    if spec.flags.alternate && spec.flags.zero_pad && !prefix.is_empty() {
        let w = spec.width.unwrap_or(0);
        let zero_padded = if padded.len() < w {
            "0".repeat(w - padded.len()) + &padded
        } else {
            padded
        };
        output.push_str(prefix);
        output.push_str(&zero_padded);
    } else {
        apply_width(output, prefix, &padded, spec.width, &spec.flags, true);
    }
}

fn format_real(output: &mut String, value: &Value, spec: &FormatSpec) {
    let conversion = match spec.spec_type {
        'f' => RealConversion::Fixed,
        'e' | 'E' => RealConversion::Exponent,
        'g' | 'G' => RealConversion::General,
        other => unreachable!("%{other} is not a floating-point conversion"),
    };
    let flags = &spec.flags;
    let format = RealFormat {
        conversion,
        uppercase: spec.spec_type.is_ascii_uppercase(),
        precision: spec.precision,
        width: spec.width.unwrap_or(0),
        left_justify: flags.left_justify,
        sign: if flags.force_sign {
            Some(b'+')
        } else if flags.space_sign {
            Some(b' ')
        } else {
            None
        },
        alternate: flags.alternate,
        alt_form_2: flags.alt_form_2,
        zero_pad: flags.zero_pad,
        thousands: flags.comma_sep,
    };
    fp::push_real(output, coerce_to_f64(value), &format);
}

fn format_string(output: &mut String, value: &Value, spec: &FormatSpec) {
    // For blobs, truncate at first NUL byte (SQLite behavior)
    let s = match value {
        Value::Blob(b) => {
            let end = b.iter().position(|&byte| byte == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).to_string()
        }
        _ => coerce_to_string(value),
    };
    let truncated = truncate_to_precision(&s, spec.precision, &spec.flags);

    // Zero-pad flag is ignored for string specifiers
    let mut flags = spec.flags.clone();
    flags.zero_pad = false;
    apply_width(output, "", truncated, spec.width, &flags, false);
}

fn format_char(output: &mut String, value: &Value, spec: &FormatSpec) {
    // In SQLite SQL context, %c takes the first character of the string representation
    let c = match value {
        Value::Text(t) => t.value.chars().next().unwrap_or('\0'),
        _ => {
            let s = coerce_to_string(value);
            s.chars().next().unwrap_or('\0')
        }
    };

    // Precision on %c repeats the character that many times (default 1)
    let repeat = spec.precision.unwrap_or(1).max(1);
    let s: String = repeat_n(c, repeat).collect();

    // Zero-pad flag is ignored for char specifiers
    let mut flags = spec.flags.clone();
    flags.zero_pad = false;
    apply_width(output, "", &s, spec.width, &flags, false);
}

/// Escape control characters (0x00-0x1f, 0x7f) as \uXXXX for %#q/%#Q.
fn escape_control_chars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_control() {
            write!(result, "\\u{:04x}", c as u32).unwrap();
        } else if c == '\\' {
            result.push_str("\\\\");
        } else {
            result.push(c);
        }
    }
    result
}

fn format_sql_quote(output: &mut String, value: &Value, spec: &FormatSpec) {
    // %q: double single quotes; NULL → (NULL). Supports width/precision.
    // %#q: also escape control characters as \uXXXX.
    let mut flags = spec.flags.clone();
    flags.zero_pad = false;
    match value {
        Value::Null => {
            let truncated = truncate_to_precision("(NULL)", spec.precision, &spec.flags);
            apply_width(output, "", truncated, spec.width, &flags, false);
        }
        _ => {
            let s = coerce_to_string(value);
            let truncated = truncate_to_precision(&s, spec.precision, &spec.flags);
            let mut escaped = truncated.replace('\'', "''");
            if spec.flags.alternate {
                escaped = escape_control_chars(&escaped);
            }
            apply_width(output, "", &escaped, spec.width, &flags, false);
        }
    }
}

fn format_sql_quote_wrap(output: &mut String, value: &Value, spec: &FormatSpec) {
    // %Q: like %q but wrapped in quotes; NULL → unquoted NULL. Supports width/precision.
    // %#Q: also escape control characters as \uXXXX.
    let mut flags = spec.flags.clone();
    flags.zero_pad = false;
    match value {
        Value::Null => {
            let truncated = truncate_to_precision("NULL", spec.precision, &spec.flags);
            apply_width(output, "", truncated, spec.width, &flags, false);
        }
        _ => {
            let s = coerce_to_string(value);
            let truncated = truncate_to_precision(&s, spec.precision, &spec.flags);
            let mut escaped = truncated.replace('\'', "''");
            // %#Q: escape control chars and wrap with unistr('...') if any are present.
            let use_unistr = if spec.flags.alternate {
                let has_ctrl = escaped.bytes().any(|b| b <= 0x1f);
                if has_ctrl {
                    escaped = escape_control_chars(&escaped);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            let mut quoted = String::with_capacity(escaped.len() + 10);
            if use_unistr {
                quoted.push_str("unistr('");
            } else {
                quoted.push('\'');
            }
            quoted.push_str(&escaped);
            if use_unistr {
                quoted.push_str("')");
            } else {
                quoted.push('\'');
            }
            apply_width(output, "", &quoted, spec.width, &flags, false);
        }
    }
}

fn format_sql_identifier(output: &mut String, value: &Value, spec: &FormatSpec) {
    // %w: double double-quotes (no wrapping quotes in SQL context); NULL → (NULL).
    let mut flags = spec.flags.clone();
    flags.zero_pad = false;
    match value {
        Value::Null => {
            let truncated = truncate_to_precision("(NULL)", spec.precision, &spec.flags);
            apply_width(output, "", truncated, spec.width, &flags, false);
        }
        _ => {
            let s = coerce_to_string(value);
            let truncated = truncate_to_precision(&s, spec.precision, &spec.flags);
            let escaped = truncated.replace('"', "\"\"");
            apply_width(output, "", &escaped, spec.width, &flags, false);
        }
    }
}

fn format_ordinal(output: &mut String, value: &Value, spec: &FormatSpec) {
    // %r: format integer as ordinal (1st, 2nd, 3rd, 4th, ...)
    let i = coerce_to_i64(value);
    let negative = i < 0;
    let abs = i.unsigned_abs();
    let suffix = match (abs % 100, abs % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    let mut digits = abs.to_string();
    // Precision is the total width of digits+suffix, not just digits.
    let digit_prec = spec.precision.map(|p| p.saturating_sub(suffix.len()));
    digits = pad_with_precision(digits, digit_prec);
    let prefix = sign_prefix(negative, &spec.flags);

    // Zero-pad: pad the digit portion to fill width (0 overrides - like integers)
    if spec.flags.zero_pad {
        let w = spec.width.unwrap_or(0);
        let content_chars = prefix.len() + digits.len() + suffix.len();
        if content_chars < w {
            digits = "0".repeat(w - content_chars) + &digits;
        }
        output.push_str(prefix);
        output.push_str(&digits);
        output.push_str(suffix);
    } else {
        let content = format!("{digits}{suffix}");
        apply_width(output, prefix, &content, spec.width, &spec.flags, false);
    }
}

/// Truncate a string to at most `precision` units: bytes by default,
/// characters with the ! flag, as SQLite counts them. Byte truncation
/// floors to a character boundary — Text values are UTF-8 strings, so the
/// partial code point SQLite emits when precision splits one cannot be
/// represented.
fn truncate_to_precision<'a>(s: &'a str, precision: Option<usize>, flags: &FormatFlags) -> &'a str {
    let Some(prec) = precision else {
        return s;
    };
    if flags.alt_form_2 {
        if let Some((byte_idx, _)) = s.char_indices().nth(prec) {
            &s[..byte_idx]
        } else {
            s
        }
    } else if prec >= s.len() {
        s
    } else {
        let mut end = prec;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

// ── Main entry point ────────────────────────────────────────────

pub fn exec_printf(values: &[Register]) -> crate::Result<Value> {
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let args: Vec<&Value> = values[1..].iter().map(|r| r.get_value()).collect();
    exec_printf_values(values[0].get_value(), &args)
}

/// The C type a sqlite3_mprintf-style caller passes for one argument slot,
/// in order. Derived from the same specifier grammar the engine formats
/// with, so the C API's va_list walk cannot drift from the formatter:
/// varargs.c pulls exactly these types and the bridge feeds the values back
/// into exec_printf_values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintfCArg {
    Int32,
    /// u/o/x/X without a length modifier: an unsigned int, zero-extended.
    Uint32,
    Int64,
    Double,
    Text,
    /// %z: text whose pointer the caller yields; freed after use.
    OwnedText,
    /// %c: a character code in the C API, rendered as that character.
    CharCode,
}

/// The sequence of C argument types `format` consumes: for each conversion,
/// an Int32 for a `*` width, an Int32 for a `.*` precision, then the
/// conversion's own argument. Stops at an unknown conversion, where the
/// engine stops formatting too.
pub fn printf_c_arg_plan(format: &str) -> Vec<PrintfCArg> {
    let mut plan = Vec::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            continue;
        }
        parse_flags(&mut chars);
        if chars.peek() == Some(&'*') {
            chars.next();
            plan.push(PrintfCArg::Int32);
        } else {
            parse_number(&mut chars);
        }
        if chars.peek() == Some(&'.') {
            chars.next();
            if chars.peek() == Some(&'*') {
                chars.next();
                plan.push(PrintfCArg::Int32);
            } else {
                parse_number(&mut chars);
            }
        }
        let mut longs = 0;
        while matches!(chars.peek(), Some(&'l') | Some(&'h')) {
            if chars.peek() == Some(&'l') {
                longs += 1;
            }
            chars.next();
        }
        match chars.next() {
            Some('d' | 'i' | 'r') => plan.push(if longs >= 1 {
                PrintfCArg::Int64
            } else {
                PrintfCArg::Int32
            }),
            Some('u' | 'o' | 'x' | 'X') => plan.push(if longs >= 1 {
                PrintfCArg::Int64
            } else {
                PrintfCArg::Uint32
            }),
            Some('f' | 'e' | 'E' | 'g' | 'G') => plan.push(PrintfCArg::Double),
            Some('s' | 'q' | 'Q' | 'w') => plan.push(PrintfCArg::Text),
            Some('z') => plan.push(PrintfCArg::OwnedText),
            Some('c') => plan.push(PrintfCArg::CharCode),
            Some('p') => plan.push(PrintfCArg::Int64),
            Some('n') | None => {}
            _ => break,
        }
    }
    plan
}

/// Value-level entry point, shared by the SQL printf()/format() functions
/// and the C API's sqlite3_mprintf/sqlite3_snprintf bridge so there is a
/// single formatting implementation.
pub fn exec_printf_values(format: &Value, args: &[&Value]) -> crate::Result<Value> {
    // SQLite converts the format argument to text if not already text.
    let fmt_owned: String;
    let format_str = match format {
        Value::Text(t) => t.as_str(),
        Value::Null => return Ok(Value::Null),
        Value::Numeric(Numeric::Integer(i)) => {
            fmt_owned = i.to_string();
            fmt_owned.as_str()
        }
        Value::Numeric(Numeric::Float(f)) => {
            fmt_owned = format_float(f64::from(*f));
            fmt_owned.as_str()
        }
        Value::Blob(b) => {
            fmt_owned = String::from_utf8_lossy(b).to_string();
            fmt_owned.as_str()
        }
    };

    let format_str = until_nul(format_str);

    let mut result = String::new();
    let mut args_index = 0;
    let mut chars = format_str.chars().peekable();
    if format_str.is_empty() {
        return Ok(Value::Null);
    }

    // Track whether any output or specifier processing happened. SQLite's
    // internal StrAccum buffer stays NULL until something triggers allocation
    // (any literal text, %%, trailing %, or any specifier including %n).
    // When an unknown specifier triggers early return before any allocation,
    // the result is NULL. Otherwise it's the accumulated text (possibly "").
    let mut touched = false;

    while let Some(c) = chars.next() {
        if c != '%' {
            touched = true;
            result.push(c);
            continue;
        }

        // Check for %%
        if chars.peek() == Some(&'%') {
            touched = true;
            chars.next();
            result.push('%');
            continue;
        }

        // Trailing '%' at end of format string is preserved
        if chars.peek().is_none() {
            result.push('%');
            break;
        }

        // Parse the full format specifier
        let spec = parse_format_spec(&mut chars, args, &mut args_index);

        // Get the argument value (or use NULL if missing)
        let needs_arg = !matches!(spec.spec_type, 'n' | '\0');
        let null_val = Value::Null;
        let arg = if needs_arg {
            if args_index < args.len() {
                let v = args[args_index];
                args_index += 1;
                v
            } else {
                &null_val
            }
        } else {
            &null_val
        };

        match spec.spec_type {
            'd' | 'i' => format_signed_int(&mut result, arg, &spec),
            'u' => format_unsigned_int(&mut result, arg, &spec),
            'f' | 'e' | 'E' | 'g' | 'G' => format_real(&mut result, arg, &spec),
            'x' => format_hex(&mut result, arg, &spec, false),
            'X' => format_hex(&mut result, arg, &spec, true),
            'o' => format_octal(&mut result, arg, &spec),
            'p' => format_hex(&mut result, arg, &spec, true),
            's' | 'z' => format_string(&mut result, arg, &spec),
            'c' => format_char(&mut result, arg, &spec),
            'q' => format_sql_quote(&mut result, arg, &spec),
            'Q' => format_sql_quote_wrap(&mut result, arg, &spec),
            'w' => format_sql_identifier(&mut result, arg, &spec),
            'r' => format_ordinal(&mut result, arg, &spec),
            'n' => { /* silently ignored, no arg consumed */ }
            _ => {
                // Unknown specifier: return NULL if nothing was processed
                // before this point, otherwise return accumulated text.
                // This matches SQLite where the internal buffer (zText) stays
                // NULL until any append is attempted.
                if !touched {
                    return Ok(Value::Null);
                }
                break;
            }
        }
        touched = true;
    }

    Ok(Value::build_text(result))
}

/// Append `value` as `printf('%.<precision>g', value)` writes it.
pub(crate) fn push_general_format(output: &mut String, value: &Value, precision: usize) {
    let spec = FormatSpec {
        flags: FormatFlags::default(),
        width: None,
        precision: Some(precision),
        spec_type: 'g',
    };
    format_real(output, value, &spec);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_general_format_writes_the_text_of_printf() {
        for f in [
            0.0,
            -0.0,
            2460513.1046875,
            2460513.104687512,
            -123.456,
            1e-300,
            1.5e300,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            let value = Value::from_f64(f);
            let mut text = String::new();
            push_general_format(&mut text, &value, 16);
            let printf = exec_printf_values(&Value::build_text("%.16g"), &[&value]).unwrap();
            assert_eq!(Value::build_text(text), printf);
        }
    }

    fn text(value: &str) -> Register {
        Register::Value(Value::build_text(value.to_string()))
    }

    fn integer(value: i64) -> Register {
        Register::Value(Value::from_i64(value))
    }

    fn float(value: f64) -> Register {
        Register::Value(Value::from_f64(value))
    }

    #[test]
    fn test_printf_no_args() {
        assert_eq!(exec_printf(&[]).unwrap(), Value::Null);
    }

    #[test]
    fn test_printf_basic_string() {
        assert_eq!(
            exec_printf(&[text("Hello World")]).unwrap(),
            *text("Hello World").get_value()
        );
    }

    #[test]
    fn test_printf_string_formatting() {
        let test_cases = vec![
            (
                vec![text("Hello, %s!"), text("World")],
                text("Hello, World!"),
            ),
            (
                vec![text("%s %s!"), text("Hello"), text("World")],
                text("Hello World!"),
            ),
            (
                vec![text("Hello, %s!"), Register::Value(Value::Null)],
                text("Hello, !"),
            ),
            (vec![text("Value: %s"), integer(42)], text("Value: 42")),
            (vec![text("100%% complete")], text("100% complete")),
        ];
        for (input, output) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *output.get_value());
        }
    }

    #[test]
    fn test_printf_integer_formatting() {
        let test_cases = vec![
            (vec![text("Number: %d"), integer(42)], text("Number: 42")),
            (vec![text("Number: %d"), integer(-42)], text("Number: -42")),
            (
                vec![text("%d + %d = %d"), integer(2), integer(3), integer(5)],
                text("2 + 3 = 5"),
            ),
            (
                vec![text("Number: %d"), text("not a number")],
                text("Number: 0"),
            ),
            (
                vec![text("Truncated float: %d"), float(3.9)],
                text("Truncated float: 3"),
            ),
            (vec![text("Number: %i"), integer(42)], text("Number: 42")),
        ];
        for (input, output) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *output.get_value());
        }
    }

    #[test]
    fn test_printf_unsigned_integer_formatting() {
        let test_cases = vec![
            (vec![text("Number: %u"), integer(42)], text("Number: 42")),
            (
                vec![text("Negative: %u"), integer(-1)],
                text("Negative: 18446744073709551615"),
            ),
            (vec![text("NaN: %u"), text("not a number")], text("NaN: 0")),
        ];
        for (input, output) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *output.get_value());
        }
    }

    #[test]
    fn test_printf_float_formatting() {
        let test_cases = vec![
            (
                vec![text("Number: %f"), float(42.5)],
                text("Number: 42.500000"),
            ),
            (
                vec![text("Number: %f"), float(-42.5)],
                text("Number: -42.500000"),
            ),
            (
                vec![text("Number: %f"), integer(42)],
                text("Number: 42.000000"),
            ),
            (
                vec![text("Number: %f"), text("not a number")],
                text("Number: 0.000000"),
            ),
        ];

        // Huge finite float must not overflow rounding to produce "inf"
        let huge = exec_printf(&[text("%f"), float(1e308)]).unwrap();
        let huge_str = match &huge {
            Value::Text(t) => t.as_str().to_string(),
            _ => panic!("expected text"),
        };
        assert_eq!(huge_str, format!("1{}.000000", "0".repeat(308)));
        assert!(!huge_str.contains("inf"));
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    #[test]
    fn test_printf_width_precision() {
        let test_cases = vec![
            (vec![text("%.2f"), float(4.002)], text("4.00")),
            (vec![text("%05d"), integer(42)], text("00042")),
            (vec![text("%.5d"), integer(42)], text("00042")),
            (vec![text("%+d"), integer(42)], text("+42")),
            (vec![text("%.3s"), text("hello")], text("hel")),
            (vec![text("%08x"), integer(255)], text("000000ff")),
            (vec![text("%#x"), integer(255)], text("0xff")),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    #[test]
    fn test_printf_dynamic_width() {
        assert_eq!(
            exec_printf(&[text("%.*f"), integer(2), float(3.14258)]).unwrap(),
            *text("3.14").get_value()
        );
    }

    #[test]
    fn test_printf_character_formatting() {
        let test_cases = vec![
            (vec![text("character: %c"), text("a")], text("character: a")),
            (
                vec![text("character: %c"), text("this is a test")],
                text("character: t"),
            ),
            (
                vec![text("character: %c"), integer(123)],
                text("character: 1"),
            ),
            (
                vec![text("character: %c"), float(42.5)],
                text("character: 4"),
            ),
            // Empty strings and NULL produce a NUL character.
            (vec![text("character: %c"), text("")], text("character: \0")),
            (
                vec![text("character: %c"), Register::Value(Value::Null)],
                text("character: \0"),
            ),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    #[test]
    fn test_printf_exponential_formatting() {
        let test_cases = vec![
            (
                vec![text("Exp: %e"), float(23000000.0)],
                text("Exp: 2.300000e+07"),
            ),
            (
                vec![text("Exp: %e"), float(-23000000.0)],
                text("Exp: -2.300000e+07"),
            ),
            (vec![text("Exp: %e"), float(0.0)], text("Exp: 0.000000e+00")),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    #[test]
    fn test_printf_general_formatting() {
        let test_cases = vec![
            (vec![text("%g"), float(100.0)], text("100")),
            (vec![text("%g"), float(0.00123)], text("0.00123")),
            (vec![text("%g"), float(1.0)], text("1")),
            (vec![text("%g"), float(1.5)], text("1.5")),
            (vec![text("%g"), float(0.0)], text("0")),
            (vec![text("%g"), integer(42)], text("42")),
            // Comma separator applies to %G decimal notation
            (vec![text("%,G"), integer(1000)], text("1,000")),
            (
                vec![text("%,.20G"), float(1234567.89)],
                text("1,234,567.89"),
            ),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    #[test]
    fn test_printf_sql_quoting() {
        assert_eq!(
            exec_printf(&[text("%q"), text("it's")]).unwrap(),
            *text("it''s").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%Q"), text("it's")]).unwrap(),
            *text("'it''s'").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%Q"), Register::Value(Value::Null)]).unwrap(),
            *text("NULL").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%q"), Register::Value(Value::Null)]).unwrap(),
            *text("(NULL)").get_value()
        );
    }

    #[test]
    fn test_printf_comma_separator() {
        assert_eq!(
            exec_printf(&[text("%,d"), integer(1234567)]).unwrap(),
            *text("1,234,567").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%,d"), integer(-1234567)]).unwrap(),
            *text("-1,234,567").get_value()
        );
    }

    #[test]
    fn test_printf_edge_cases() {
        let test_cases = vec![
            (vec![text("%%%%")], text("%%")),
            (vec![text("No substitutions")], text("No substitutions")),
            (
                vec![text("%d%d%d"), integer(1), integer(2), integer(3)],
                text("123"),
            ),
            // Trailing % is preserved
            (vec![text("test%")], text("test%")),
            // Unknown specifier: NULL if nothing processed before, else accumulated text
            (vec![text("%d%j"), integer(42)], text("42")),
            (vec![text("hello%j")], text("hello")),
            // Negative zero should not show minus sign
            (vec![text("%f"), float(-0.0)], text("0.000000")),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
        assert_eq!(exec_printf(&[text("")]).unwrap(), Value::Null);
        assert_eq!(
            exec_printf(&[text("%s"), text("")]).unwrap(),
            *text("").get_value()
        );
    }

    #[test]
    fn test_printf_hexadecimal_formatting() {
        let test_cases = vec![
            (vec![text("hex: %x"), integer(4)], text("hex: 4")),
            (
                vec![text("hex: %X"), integer(15565303546)],
                text("hex: 39FC3AEFA"),
            ),
            (
                vec![text("hex: %x"), integer(-15565303546)],
                text("hex: fffffffc603c5106"),
            ),
            (vec![text("hex: %x"), float(42.5)], text("hex: 2a")),
            (vec![text("hex: %x"), text("42")], text("hex: 2a")),
            (vec![text("hex: %x"), text("")], text("hex: 0")),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    #[test]
    fn test_printf_octal_formatting() {
        let test_cases = vec![
            (vec![text("octal: %o"), integer(4)], text("octal: 4")),
            (vec![text("octal: %o"), float(42.5)], text("octal: 52")),
            (vec![text("octal: %o"), text("42")], text("octal: 52")),
            // # flag always adds "0" prefix when value is non-zero
            (vec![text("%#o"), integer(8)], text("010")),
            (vec![text("%#o"), integer(0)], text("0")),
            // # flag with precision: "0" prefix added even if precision pads with zeros
            (vec![text("%#.5o"), integer(8)], text("000010")),
            (
                vec![text("%#.20o"), integer(1000)],
                text("000000000000000001750"),
            ),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    // ── Bug fix regression tests ────────────────────────────────────

    #[test]
    fn test_rounding_half_away_from_zero() {
        // Bug 1: SQLite uses half-away-from-zero, not half-to-even
        assert_eq!(
            exec_printf(&[text("%.0f"), float(0.5)]).unwrap(),
            *text("1").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.0f"), float(2.5)]).unwrap(),
            *text("3").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.0f"), float(-0.5)]).unwrap(),
            *text("-1").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.0e"), float(2.5)]).unwrap(),
            *text("3e+00").get_value()
        );
    }

    #[test]
    fn test_alt_hex_zero_pad_width() {
        // Bug 2: # flag with 0 flag - prefix not counted in width
        assert_eq!(
            exec_printf(&[text("%#08x"), integer(255)]).unwrap(),
            *text("0x000000ff").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%#04x"), integer(255)]).unwrap(),
            *text("0x00ff").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%#08o"), integer(8)]).unwrap(),
            *text("000000010").get_value()
        );
    }

    #[test]
    fn test_alt_flag_forces_decimal_point() {
        // Bug 3: # flag forces decimal point on %e and %g
        assert_eq!(
            exec_printf(&[text("%#.0e"), float(1.0)]).unwrap(),
            *text("1.e+00").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%#.0g"), float(1.0)]).unwrap(),
            *text("1.").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%#g"), float(100000.0)]).unwrap(),
            *text("100000.").get_value()
        );
    }

    #[test]
    fn test_g_threshold_rounding() {
        // Bug 4: %g pre-rounding changes the exponent threshold
        assert_eq!(
            exec_printf(&[text("%g"), float(999999.5)]).unwrap(),
            *text("1e+06").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.1g"), float(9.5)]).unwrap(),
            *text("1e+01").get_value()
        );
    }

    #[test]
    fn test_zero_pad_ignored_for_strings() {
        // Bug 5: 0 flag should be ignored for %s and %c
        assert_eq!(
            exec_printf(&[text("%05s"), text("hi")]).unwrap(),
            *text("   hi").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%05c"), text("A")]).unwrap(),
            *text("    A").get_value()
        );
    }

    #[test]
    fn test_q_width_precision() {
        // Bug 6: %q/%Q/%w should respect width and precision
        assert_eq!(
            exec_printf(&[text("%.2q"), text("hello")]).unwrap(),
            *text("he").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%10q"), text("hi")]).unwrap(),
            *text("        hi").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%10Q"), text("hi")]).unwrap(),
            *text("      'hi'").get_value()
        );
    }

    #[test]
    fn test_infinity_handling() {
        // SQLite source (sqlite3.c:32502): infinity + flag_zeropad → 9-fill
        // infinity without flag_zeropad → "Inf"
        let inf_f = exec_printf(&[text("%020f"), float(f64::INFINITY)]).unwrap();
        let inf_str = match &inf_f {
            Value::Text(t) => t.as_str().to_string(),
            _ => panic!("expected text"),
        };
        assert!(inf_str.starts_with("9000"));
        assert_eq!(inf_str.len(), 1007); // 9 + 999 zeros + ".000000"

        assert_eq!(
            exec_printf(&[text("%020e"), float(f64::INFINITY)]).unwrap(),
            *text("00000009.000000e+999").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%020g"), float(f64::INFINITY)]).unwrap(),
            *text("000000000000009e+999").get_value()
        );
        // Without zero-pad → "Inf" (not 9-fill)
        assert_eq!(
            exec_printf(&[text("%e"), float(f64::INFINITY)]).unwrap(),
            *text("Inf").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%G"), float(f64::INFINITY)]).unwrap(),
            *text("Inf").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%f"), float(f64::INFINITY)]).unwrap(),
            *text("Inf").get_value()
        );
        // With zero-pad but no width still triggers 9-fill
        assert_eq!(
            exec_printf(&[text("%0G"), float(f64::INFINITY)]).unwrap(),
            *text("9E+999").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%0,G"), float(f64::INFINITY)]).unwrap(),
            *text("9E+999").get_value()
        );
        // Negative infinity
        assert_eq!(
            exec_printf(&[text("%e"), float(f64::NEG_INFINITY)]).unwrap(),
            *text("-Inf").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%020e"), float(f64::NEG_INFINITY)]).unwrap(),
            *text("-0000009.000000e+999").get_value()
        );
        // # flag with %g infinity: RTZ disabled, so trailing zeros remain
        assert_eq!(
            exec_printf(&[text("%#0g"), float(f64::INFINITY)]).unwrap(),
            *text("9.00000e+999").get_value()
        );
        // ! flag with %e infinity: RTZ enabled, strips to .0
        assert_eq!(
            exec_printf(&[text("%!0e"), float(f64::INFINITY)]).unwrap(),
            *text("9.0e+999").get_value()
        );
        // ! flag with %f infinity: strips trailing fractional zeros
        let inf_bang_f = exec_printf(&[text("%!0f"), float(f64::INFINITY)]).unwrap();
        let inf_bang_str = match &inf_bang_f {
            Value::Text(t) => t.as_str().to_string(),
            _ => panic!("expected text"),
        };
        assert!(
            inf_bang_str.ends_with(".0"),
            "Infinity with %!0f should end with .0, got: ...{}",
            &inf_bang_str[inf_bang_str.len().saturating_sub(10)..]
        );
    }

    #[test]
    fn test_significant_digits_limiting() {
        // Default: 16 significant digits (hide IEEE noise)
        assert_eq!(
            exec_printf(&[text("%.20f"), float(1.0 / 3.0)]).unwrap(),
            *text("0.33333333333333330000").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.20e"), float(1.0 / 3.0)]).unwrap(),
            *text("3.33333333333333300000e-01").get_value()
        );
        // The ! flag allows 20 significant digits, and removes trailing
        // zeros. sqlite3FpDecode has 18 digits of the double to round.
        assert_eq!(
            exec_printf(&[text("%!.20f"), float(1.0 / 3.0)]).unwrap(),
            *text("0.333333333333333315").get_value()
        );
    }

    #[test]
    fn test_nan_handling() {
        // Value::from_f64(NaN) returns Value::Null (NonNan rejects NaN),
        // so NaN is treated as NULL which coerces to 0.0 for float formats.
        // The NaN-specific formatting code (NaN/null output) is defense-in-depth
        // that can't be triggered through the Value system.
        assert_eq!(
            exec_printf(&[text("%f"), float(f64::NAN)]).unwrap(),
            *text("0.000000").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%e"), float(f64::NAN)]).unwrap(),
            *text("0.000000e+00").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%g"), float(f64::NAN)]).unwrap(),
            *text("0").get_value()
        );
    }

    #[test]
    fn test_blob_nul_truncation() {
        // Bug 9: %s on blobs truncates at first NUL byte
        let blob_val = Register::Value(
            Value::from_slice(&[0x48, 0x00, 0x4C]).expect(crate::alloc::ALLOC_ERR_MSG),
        ); // H\0L
        let result = exec_printf(&[text("%s"), blob_val]).unwrap();
        assert_eq!(result, *text("H").get_value());

        let blob_hello =
            Register::Value(Value::from_slice(b"Hello").expect(crate::alloc::ALLOC_ERR_MSG));
        assert_eq!(
            exec_printf(&[text("%s"), blob_hello]).unwrap(),
            *text("Hello").get_value()
        );
    }

    #[test]
    fn test_limit_significant_digits_rounding() {
        // Verify the rounding behavior of limit_significant_digits
        assert_eq!(limit_significant_digits("123456789", 5), "123460000");
        assert_eq!(limit_significant_digits("1.23456789", 5), "1.23460000");
        assert_eq!(limit_significant_digits("0.001234", 3), "0.001230");
        assert_eq!(limit_significant_digits("9.9999", 3), "10.0000");
        assert_eq!(limit_significant_digits("0.099999", 4), "0.100000");
    }

    #[test]
    fn test_i32_star_precision_wrapping() {
        // i32::MIN as * precision wraps back to itself after negation → treated as 0
        assert_eq!(
            exec_printf(&[text("%.*d"), integer(-2147483648), integer(42)]).unwrap(),
            *text("42").get_value()
        );
        // 4294967295 as i64 → -1 as i32 → wrapping_neg → 1
        assert_eq!(
            exec_printf(&[text("%.*d"), integer(4294967295), integer(42)]).unwrap(),
            *text("42").get_value()
        );
    }

    #[test]
    fn test_comma_zero_pad_interaction() {
        // When comma + zero_pad: zero-pad digits to width, then insert commas
        // Width 15 = 15 digit positions, commas added on top
        assert_eq!(
            exec_printf(&[text("%0,15d"), integer(42)]).unwrap(),
            *text("000,000,000,000,042").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%0,15u"), integer(42)]).unwrap(),
            *text("000,000,000,000,042").get_value()
        );
        // Left-justify is ignored when comma + zero-pad are both set
        assert_eq!(
            exec_printf(&[text("%-0,15d"), integer(42)]).unwrap(),
            *text("000,000,000,000,042").get_value()
        );
    }

    #[test]
    fn test_ordinal_format() {
        assert_eq!(
            exec_printf(&[text("%r"), integer(1)]).unwrap(),
            *text("1st").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%r"), integer(2)]).unwrap(),
            *text("2nd").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%r"), integer(3)]).unwrap(),
            *text("3rd").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%r"), integer(11)]).unwrap(),
            *text("11th").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%r"), integer(112)]).unwrap(),
            *text("112th").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.5r"), integer(-39)]).unwrap(),
            *text("-039th").get_value()
        );
        assert_eq!(
            exec_printf(&[text("% r"), integer(42)]).unwrap(),
            *text(" 42nd").get_value()
        );
        // Zero-pad pads the digits before the suffix
        assert_eq!(
            exec_printf(&[text("%010r"), integer(0)]).unwrap(),
            *text("00000000th").get_value()
        );
    }

    #[test]
    fn test_q_null_precision_truncation() {
        // Precision truncates the NULL literal representation
        assert_eq!(
            exec_printf(&[text("%.0q"), Register::Value(Value::Null)]).unwrap(),
            *text("").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.3q"), Register::Value(Value::Null)]).unwrap(),
            *text("(NU").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.0Q"), Register::Value(Value::Null)]).unwrap(),
            *text("").get_value()
        );
    }

    #[test]
    fn test_q_null_width_padding() {
        // Width applies to the NULL representation
        assert_eq!(
            exec_printf(&[text("%-10q"), Register::Value(Value::Null)]).unwrap(),
            *text("(NULL)    ").get_value()
        );
    }

    #[test]
    fn test_unknown_specifier_returns_early() {
        // Unknown specifier as first thing → NULL (SQLite's StrAccum never allocated)
        assert_eq!(
            exec_printf(&[text("%b"), integer(42)]).unwrap(),
            Value::Null,
        );
        // Unknown specifier after literal text → accumulated text
        assert_eq!(
            exec_printf(&[text("hello%b"), integer(42)]).unwrap(),
            *text("hello").get_value()
        );
        // Unknown specifier after %n → "" (StrAccum was allocated by %n processing)
        assert_eq!(
            exec_printf(&[text("%n%b"), integer(42)]).unwrap(),
            *text("").get_value(),
        );
    }

    #[test]
    fn test_control_char_escaping_with_hash_q() {
        // %#q escapes control characters as \uXXXX and doubles backslashes
        assert_eq!(
            exec_printf(&[text("%#q"), text("a\nb")]).unwrap(),
            *text("a\\u000ab").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%#q"), text("a\tb")]).unwrap(),
            *text("a\\u0009b").get_value()
        );
        // Backslash is doubled in escape mode
        assert_eq!(
            exec_printf(&[text("%#q"), text("a\\b")]).unwrap(),
            *text("a\\\\b").get_value()
        );
    }

    #[test]
    fn test_hash_q_upper_unistr_wrapping() {
        // %#Q wraps with unistr('...') when control chars are present
        assert_eq!(
            exec_printf(&[text("%#Q"), text("a\nb")]).unwrap(),
            *text("unistr('a\\u000ab')").get_value()
        );
        // %#Q without control chars — no unistr wrapping
        assert_eq!(
            exec_printf(&[text("%#Q"), text("hello")]).unwrap(),
            *text("'hello'").get_value()
        );
        // %Q without # — no unistr wrapping even with control chars
        assert_eq!(
            exec_printf(&[text("%Q"), text("a\nb")]).unwrap(),
            *text("'a\nb'").get_value()
        );
    }

    #[test]
    fn test_nul_ends_format_and_string_args() {
        // SQLite stops the format string and string arguments at the first NUL.
        assert_eq!(
            exec_printf(&[text("a\0b%d"), integer(5)]).unwrap(),
            *text("a").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%s-%d"), text("a\0b"), integer(1)]).unwrap(),
            *text("a-1").get_value()
        );
        assert_eq!(
            exec_printf(&[
                text("%5s|%q|%Q|%w"),
                text("a\0b"),
                text("c\0d"),
                text("e\0f"),
                text("g\0h")
            ])
            .unwrap(),
            *text("    a|c|'e'|g").get_value()
        );
        // A format string that is empty up to the NUL gives NULL, like ''.
        assert_eq!(exec_printf(&[text("\0x")]).unwrap(), Value::Null);
    }

    #[test]
    fn test_very_small_float_no_nan() {
        // 1e-300 with %G should not produce NaN — round_half_away_e must handle
        // subnormal scale values from 10^(-309+) without dividing by ~0.
        let result = exec_printf(&[text("%.*G"), integer(10), float(1e-300)]).unwrap();
        assert_eq!(result, *text("1E-300").get_value());

        // Also test with %e
        let result = exec_printf(&[text("%.10e"), float(1e-300)]).unwrap();
        assert!(
            !result.to_string().contains("NaN"),
            "1e-300 with %e should not produce NaN"
        );

        // And %g
        let result = exec_printf(&[text("%.10g"), float(1e-300)]).unwrap();
        assert!(
            !result.to_string().contains("NaN"),
            "1e-300 with %g should not produce NaN"
        );
    }

    #[test]
    fn test_large_float_f_format() {
        // sqlite3FpDecode rounds 1e308 to 16 significant digits, which are a
        // 1 and zeros. Rust's format! writes the exact decimal value instead,
        // 1000000000000000010979...
        let result = exec_printf(&[text("%.0f"), float(1e308)]).unwrap();
        assert_eq!(result.to_string(), format!("1{}", "0".repeat(308)));

        // With commas too
        let result = exec_printf(&[text("%,f"), float(1e308)]).unwrap();
        assert_eq!(
            result.to_string(),
            format!("100{}.000000", ",000".repeat(102))
        );
    }

    #[test]
    fn test_negative_zero_suppression() {
        // SQLite 3.51+ (sqlite3.c:32520-32532): With # flag (no + or space),
        // %f suppresses minus sign when displayed value rounds to zero.

        // -0.0000001 with %#f displays as 0.000000 — suppress minus
        let result = exec_printf(&[text("%#f"), float(-0.0000001)]).unwrap();
        assert_eq!(result.to_string(), "0.000000");

        // Same without # flag — keep minus
        let result = exec_printf(&[text("%f"), float(-0.0000001)]).unwrap();
        assert_eq!(result.to_string(), "-0.000000");

        // With + flag, # doesn't suppress (flag_prefix is set)
        let result = exec_printf(&[text("%#+f"), float(-0.0000001)]).unwrap();
        assert_eq!(result.to_string(), "-0.000000");

        // -0.5 rounds to -1, not zero — keep minus
        let result = exec_printf(&[text("%#.0f"), float(-0.5)]).unwrap();
        assert_eq!(result.to_string(), "-1.");

        // -0.4 rounds to 0 — suppress
        let result = exec_printf(&[text("%#.0f"), float(-0.4)]).unwrap();
        assert_eq!(result.to_string(), "0.");

        // -0.0000001 with comma separator
        let result = exec_printf(&[text("%#,f"), float(-0.0000001)]).unwrap();
        assert_eq!(result.to_string(), "0.000000");
    }
}
