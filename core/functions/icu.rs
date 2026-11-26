//! The SQL functions of SQLite's ICU extension (`ext/icu/icu.c`), which a
//! build with SQLITE_ENABLE_ICU registers over the built-in ones:
//!
//! - `upper(X)`, `lower(X)`, `upper(X, L)` and `lower(X, L)` map case with
//!   ICU's full case mapping, in the root locale or in locale `L`.
//! - `like(P, S)`, `like(P, S, E)` and the LIKE operator compare characters
//!   by their simple case folding, so `'Ä' LIKE 'ä'` is true.
//! - `regexp(P, S)` and the REGEXP operator are true when the pattern matches
//!   the whole string.
//!
//! GLOB keeps its built-in behavior, because the extension does not replace
//! it.

use std::borrow::Cow;
use std::cell::RefCell;
use std::ops::Range;
use std::sync::Arc;

use icu_casemap::CaseMapper;
use icu_casemap::CaseMapperBorrowed;
use icu_locale::langid;
use icu_locale::LanguageIdentifier;
use icu_properties::props::{CaseIgnorable, Cased};
use icu_properties::{CodePointSetData, CodePointSetDataBorrowed};

use crate::ast::{self, Cmd, CreateTableBody, Privilege, QualifiedName, Stmt};
use crate::dialect::{
    Catalog, Dependent, Dialect, RenameQuoting, ScalarFuncContext, SortTermMatching,
};
use crate::function::{Func, ScalarFunc};
use crate::namespace::NamespaceId;
use crate::numeric::{self, Numeric};
use crate::schema::{BTreeTable, Schema, SchemaBuilder};
use crate::security::policy::RowPolicy;
use crate::security::roles::RoleId;
use crate::vdbe::Register;
use crate::{Connection, LikeFunction, LimboError, OpenOptions, Result, Value};

use super::icu_regex;

/// The longest LIKE pattern, in bytes, that the extension accepts. It is
/// the extension's own default of SQLITE_MAX_LIKE_PATTERN_LENGTH.
const MAX_LIKE_PATTERN_LENGTH: usize = 50000;

impl dyn Dialect {
    /// This dialect with the SQL functions of SQLite's ICU extension in place
    /// of the built-in `upper`, `lower`, `like` and `regexp`, as in a SQLite
    /// built with SQLITE_ENABLE_ICU, such as the one in Chromium. Every other
    /// behavior is the dialect's own, and the name of the dialect stays the
    /// same, so a database opened with either one opens with the other.
    ///
    /// ```ignore
    /// let dialect: Arc<dyn Dialect> = Arc::new(SqliteDialect);
    /// let options = OpenOptions::new(dialect.with_icu_functions());
    /// ```
    pub fn with_icu_functions(self: Arc<Self>) -> Arc<dyn Dialect> {
        Arc::new(IcuDialect { inner: self })
    }
}

struct IcuDialect {
    inner: Arc<dyn Dialect>,
}

impl Dialect for IcuDialect {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn format_error(&self, error: LimboError) -> LimboError {
        self.inner.format_error(error)
    }

    fn parse(&self, sql: &str) -> Result<(Option<Cmd>, usize)> {
        self.inner.parse(sql)
    }

    fn parse_internal(&self, sql: &str) -> Result<(Option<Cmd>, usize)> {
        self.inner.parse_internal(sql)
    }

    fn parse_internal_column_definition(&self, sql: &str) -> Result<ast::ColumnDefinition> {
        self.inner.parse_internal_column_definition(sql)
    }

    fn parameter_marker_ranges(&self, sql: &str) -> Result<Vec<Range<usize>>> {
        self.inner.parameter_marker_ranges(sql)
    }

    fn get_internal_dialect(&self) -> Arc<dyn Dialect> {
        self.inner.get_internal_dialect().with_icu_functions()
    }

    fn parse_table_sql(&self, sql: &str, root_page: i64) -> Result<BTreeTable> {
        self.inner.parse_table_sql(sql, root_page)
    }

    fn parse_table_sql_ast(&self, sql: &str) -> Result<Stmt> {
        self.inner.parse_table_sql_ast(sql)
    }

    fn table_sql_for_replay(&self, sql: &str) -> Result<String> {
        self.inner.table_sql_for_replay(sql)
    }

    fn format_table_sql(
        &self,
        input: &str,
        tbl_name: &QualifiedName,
        body: &CreateTableBody,
    ) -> Result<String> {
        self.inner.format_table_sql(input, tbl_name, body)
    }

    fn format_rewritten_table_sql(&self, stmt: &Stmt) -> Result<String> {
        self.inner.format_rewritten_table_sql(stmt)
    }

    fn format_index_sql(&self, input: &str, canonical: String) -> Result<String> {
        self.inner.format_index_sql(input, canonical)
    }

    fn format_trigger_sql(&self, input: &str, canonical: String) -> Result<String> {
        self.inner.format_trigger_sql(input, canonical)
    }

    fn format_view_sql(&self, input: &str, canonical: String) -> Result<String> {
        self.inner.format_view_sql(input, canonical)
    }

    fn added_column_text(&self, input: &str) -> Option<String> {
        self.inner.added_column_text(input)
    }

    fn table_sql_with_added_column(&self, sql: &str, column: &str) -> Option<String> {
        self.inner.table_sql_with_added_column(sql, column)
    }

    fn renamed_schema_sql(
        &self,
        sql: &str,
        rewritten: &Stmt,
        rendered: String,
        quoting: RenameQuoting,
    ) -> Result<String> {
        self.inner.renamed_schema_sql(sql, rewritten, rendered, quoting)
    }

    fn schema_builder(&self, enable_custom_types: bool) -> Result<SchemaBuilder> {
        self.inner.schema_builder(enable_custom_types)
    }

    fn register_native_extensions(&self, options: OpenOptions) -> OpenOptions {
        self.inner.register_native_extensions(options)
    }

    fn resolve_function(&self, name: &str, arg_count: usize) -> Result<Option<Func>> {
        match IcuFunction::resolve(name, arg_count) {
            Some(IcuFunction::Upper | IcuFunction::Lower | IcuFunction::Like) => {
                self.inner.resolve_function(name, arg_count)
            }
            Some(function) => Ok(Some(Func::Dialect(function.name().to_string()))),
            None => self.inner.resolve_function(name, arg_count),
        }
    }

    fn call_scalar(
        &self,
        context: &ScalarFuncContext<'_>,
        func: &ScalarFunc,
        args: &[Register],
    ) -> Result<Value> {
        let values = || args.iter().map(Register::get_value);
        match func {
            ScalarFunc::Upper => Ok(case_map(values(), Case::Upper)),
            ScalarFunc::Lower => Ok(case_map(values(), Case::Lower)),
            ScalarFunc::Like if context.connection.like_function() == LikeFunction::Dialect => {
                like(&values().collect::<Vec<_>>())
            }
            _ => self.inner.call_scalar(context, func, args),
        }
    }

    fn exec_scalar_function(&self, conn: &Connection, name: &str, args: &[Value]) -> Result<Value> {
        match IcuFunction::resolve(name, args.len()) {
            Some(IcuFunction::UpperInLocale) => Ok(case_map(args.iter(), Case::Upper)),
            Some(IcuFunction::LowerInLocale) => Ok(case_map(args.iter(), Case::Lower)),
            Some(IcuFunction::Regexp) => regexp(&args[0], &args[1]),
            Some(IcuFunction::Upper | IcuFunction::Lower | IcuFunction::Like) | None => {
                self.inner.exec_scalar_function(conn, name, args)
            }
        }
    }

    fn requires_custom_types(&self) -> bool {
        self.inner.requires_custom_types()
    }

    fn qualified_column_uses_nearest_table_only(&self) -> bool {
        self.inner.qualified_column_uses_nearest_table_only()
    }

    fn table_function_arguments_read_only_earlier_tables(&self) -> bool {
        self.inner.table_function_arguments_read_only_earlier_tables()
    }

    fn binds_names_at_create(&self) -> bool {
        self.inner.binds_names_at_create()
    }

    fn relation_name_in_messages(
        &self,
        connection: &Connection,
        schema: &Schema,
        namespace: NamespaceId,
        name: &str,
    ) -> String {
        self.inner
            .relation_name_in_messages(connection, schema, namespace, name)
    }

    fn relation_dependents(
        &self,
        schema: &Schema,
        namespace: NamespaceId,
        name: &str,
    ) -> Vec<Dependent> {
        self.inner.relation_dependents(schema, namespace, name)
    }

    fn supports_namespaces(&self) -> bool {
        self.inner.supports_namespaces()
    }

    fn relation_names_unique_across_kinds(&self) -> bool {
        self.inner.relation_names_unique_across_kinds()
    }

    fn catalog(&self) -> &dyn Catalog {
        self.inner.catalog()
    }

    fn role_by_name(&self, schema: &Schema, name: &str) -> Option<RoleId> {
        self.inner.role_by_name(schema, name)
    }

    fn role_is_superuser(&self, schema: &Schema, role: RoleId) -> bool {
        self.inner.role_is_superuser(schema, role)
    }

    fn role_bypass_rls(&self, schema: &Schema, role: RoleId) -> bool {
        self.inner.role_bypass_rls(schema, role)
    }

    fn can_set_role(&self, schema: &Schema, member: RoleId, role: RoleId) -> bool {
        self.inner.can_set_role(schema, member, role)
    }

    fn role_has_privileges_of(&self, schema: &Schema, member: RoleId, role: RoleId) -> bool {
        self.inner.role_has_privileges_of(schema, member, role)
    }

    fn relation_owner(
        &self,
        schema: &Schema,
        namespace: NamespaceId,
        name: &str,
    ) -> Option<RoleId> {
        self.inner.relation_owner(schema, namespace, name)
    }

    fn owned_sequences(
        &self,
        schema: &Schema,
        namespace: NamespaceId,
        table: &str,
        column: Option<&str>,
    ) -> Vec<String> {
        self.inner.owned_sequences(schema, namespace, table, column)
    }

    fn role_has_privilege(
        &self,
        schema: &Schema,
        namespace: NamespaceId,
        name: &str,
        role: RoleId,
        privilege: Privilege,
    ) -> bool {
        self.inner
            .role_has_privilege(schema, namespace, name, role, privilege)
    }

    fn table_rls_enabled(&self, schema: &Schema, namespace: NamespaceId, name: &str) -> bool {
        self.inner.table_rls_enabled(schema, namespace, name)
    }

    fn row_security_policies<'a>(
        &self,
        schema: &'a Schema,
        namespace: NamespaceId,
        name: &str,
    ) -> &'a [RowPolicy] {
        self.inner.row_security_policies(schema, namespace, name)
    }

    fn supports_capture_data_changes(&self) -> bool {
        self.inner.supports_capture_data_changes()
    }

    fn default_namespaces(&self) -> &'static [&'static str] {
        self.inner.default_namespaces()
    }

    fn upsert_rejects_row_written_by_same_statement(&self) -> bool {
        self.inner.upsert_rejects_row_written_by_same_statement()
    }

    fn rejects_scalar_subquery_with_several_rows(&self) -> bool {
        self.inner.rejects_scalar_subquery_with_several_rows()
    }

    fn sort_term_matching(&self) -> SortTermMatching {
        self.inner.sort_term_matching()
    }

    fn errors_on_zero_divisor(&self) -> bool {
        self.inner.errors_on_zero_divisor()
    }

    fn concatenation_of_blobs_is_blob(&self) -> bool {
        self.inner.concatenation_of_blobs_is_blob()
    }

    fn mod_of_integers_is_integer(&self) -> bool {
        self.inner.mod_of_integers_is_integer()
    }

    fn sum_counts_infinite_inputs(&self) -> bool {
        self.inner.sum_counts_infinite_inputs()
    }

    fn add_column_computes_default_for_existing_rows(&self) -> bool {
        self.inner.add_column_computes_default_for_existing_rows()
    }

    fn allows_qualified_columns_in_index_terms(&self) -> bool {
        self.inner.allows_qualified_columns_in_index_terms()
    }

    fn text_input_types_refuse_other_kinds(&self) -> bool {
        self.inner.text_input_types_refuse_other_kinds()
    }

    fn constraint_errors_show_values(&self) -> bool {
        self.inner.constraint_errors_show_values()
    }

    fn generates_integer_primary_keys(&self) -> bool {
        self.inner.generates_integer_primary_keys()
    }

    fn check_ctas_column_type(&self, column: &str, type_name: Option<&str>) -> Result<()> {
        self.inner.check_ctas_column_type(column, type_name)
    }

    fn write_array_blob_element(&self, out: &mut String, blob: &[u8]) {
        self.inner.write_array_blob_element(out, blob)
    }

    fn json_contains_function(&self) -> Option<&'static str> {
        self.inner.json_contains_function()
    }
}

/// The functions that the ICU extension registers, by name and number of
/// arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IcuFunction {
    Upper,
    Lower,
    UpperInLocale,
    LowerInLocale,
    Like,
    Regexp,
}

impl IcuFunction {
    fn resolve(name: &str, arg_count: usize) -> Option<Self> {
        let is = |function: &str| name.eq_ignore_ascii_case(function);
        Some(match arg_count {
            1 if is("upper") => Self::Upper,
            1 if is("lower") => Self::Lower,
            2 if is("upper") => Self::UpperInLocale,
            2 if is("lower") => Self::LowerInLocale,
            2 | 3 if is("like") => Self::Like,
            2 if is("regexp") => Self::Regexp,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Upper | Self::UpperInLocale => "upper",
            Self::Lower | Self::LowerInLocale => "lower",
            Self::Like => "like",
            Self::Regexp => "regexp",
        }
    }
}

const CASE_MAPPER: CaseMapperBorrowed<'static> = CaseMapper::new();

/// The text that SQLite gives a function for a value, as
/// `sqlite3_value_text` reads it, or `None` for NULL.
fn value_text(value: &Value) -> Option<Cow<'_, [u8]>> {
    Some(match value {
        Value::Null => return None,
        Value::Text(text) => Cow::Borrowed(text.as_bytes()),
        Value::Blob(blob) => Cow::Borrowed(blob.as_slice()),
        Value::Numeric(Numeric::Integer(integer)) => Cow::Owned(integer.to_string().into_bytes()),
        Value::Numeric(Numeric::Float(float)) => {
            Cow::Owned(numeric::format_float(f64::from(*float)).into_bytes())
        }
    })
}

const UTF8_TRANS1: [u8; 64] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00, 0x01, 0x02, 0x03, 0x00, 0x01, 0x00, 0x00,
];

/// The text that SQLite hands to ICU as UTF-16 for UTF-8 `bytes`, as
/// `sqlite3_value_text16` converts them. SQLite reads a malformed sequence
/// as U+FFFD, and a noncharacter U+FFFE or U+FFFF too.
fn sqlite_utf16_text(bytes: &[u8]) -> Cow<'_, str> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        if !text.contains(['\u{fffe}', '\u{ffff}']) {
            return Cow::Borrowed(text);
        }
    }
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let mut c = u32::from(bytes[i]);
        i += 1;
        if c >= 0xc0 {
            c = u32::from(UTF8_TRANS1[(c - 0xc0) as usize]);
            while i < bytes.len() && bytes[i] & 0xc0 == 0x80 {
                c = (c << 6).wrapping_add(u32::from(bytes[i] & 0x3f));
                i += 1;
            }
            if c < 0x80 || c & 0xffff_f800 == 0xd800 || c & 0xffff_fffe == 0xfffe {
                c = 0xfffd;
            }
        }
        // SQLite's UTF-16 writer keeps 20 bits of a code point above
        // U+FFFF, so one above U+10FFFF lands in a supplementary plane.
        if c > 0xffff {
            c = 0x10000 + ((c - 0x10000) & 0xfffff);
        }
        out.push(char::from_u32(c).expect("every code point SQLite writes is a scalar value"));
    }
    Cow::Owned(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Case {
    Upper,
    Lower,
}

/// `icuCaseFunc16`: the value with its case mapped, in the locale of the
/// second argument when there is one.
fn case_map<'a>(mut args: impl Iterator<Item = &'a Value>, case: Case) -> Value {
    let input = args.next().expect("upper() and lower() take at least one argument");
    let locale = args.next().and_then(value_text);
    let Some(input) = value_text(input) else {
        return Value::Null;
    };
    let text = sqlite_utf16_text(&input);
    let language = case_language(locale.as_deref());
    let mapped = match case {
        Case::Upper if *language == GREEK => greek_uppercase(&text),
        Case::Upper => CASE_MAPPER.uppercase_to_string(&text, language).into_owned(),
        Case::Lower => CASE_MAPPER.lowercase_to_string(&text, language).into_owned(),
    };
    Value::build_text(mapped)
}

static GREEK: LanguageIdentifier = langid!("el");

/// Greek uppercasing, ICU 78's `GreekUpper::toUpper`: each Greek letter
/// loses its accents and breathings, keeps or gains a dialytika, and maps a
/// iota subscript to a capital iota; other characters map as in the root
/// locale.
fn greek_uppercase(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut upper = String::with_capacity(text.len());
    let mut other_start = 0;
    let mut state = GreekState::default();
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        let mut next = index + 1;
        let mut next_state = GreekState {
            after_cased: if CASE_IGNORABLE.contains(c) {
                state.after_cased
            } else {
                CASED.contains(c)
            },
            after_accented_vowel: None,
        };
        let Some(letter) = greek_letter(c) else {
            index = next;
            state = next_state;
            continue;
        };
        let other: String = chars[other_start..index].iter().collect();
        upper.push_str(&CASE_MAPPER.uppercase_to_string(&other, &ROOT));
        let mut marks = letter.marks;
        let mut base = letter.base;
        if letter.vowel && matches!(base, 'Ι' | 'Υ') {
            match state.after_accented_vowel {
                Some(GreekAccent::Precomposed) => marks.dialytika = true,
                Some(GreekAccent::Combining) => marks.combining_dialytika = true,
                None => {}
            }
        }
        let mut iota_subscripts = usize::from(marks.ypogegrammeni);
        let precomposed_accent = marks.accent;
        while let Some(&mark) = chars.get(next) {
            if !marks.add_combining(mark) {
                break;
            }
            if mark == '\u{345}' {
                iota_subscripts += 1;
            }
            next += 1;
        }
        if letter.vowel && marks.accent && !marks.dialytika {
            next_state.after_accented_vowel = Some(if precomposed_accent {
                GreekAccent::Precomposed
            } else {
                GreekAccent::Combining
            });
        }
        let mut add_tonos = false;
        if base == 'Η'
            && marks.accent
            && iota_subscripts == 0
            && !state.after_cased
            && !followed_by_cased_letter(&chars[next..])
        {
            // A lone accented eta keeps its tonos: it is the disjunctive "or".
            if precomposed_accent {
                base = 'Ή';
            } else {
                add_tonos = true;
            }
        } else if marks.dialytika && matches!(base, 'Ι' | 'Υ') {
            base = if base == 'Ι' { 'Ϊ' } else { 'Ϋ' };
            marks.dialytika = false;
            marks.combining_dialytika = false;
        }
        upper.push(base);
        if marks.dialytika || marks.combining_dialytika {
            upper.push('\u{308}');
        }
        if add_tonos {
            upper.push('\u{301}');
        }
        upper.extend(std::iter::repeat_n('Ι', iota_subscripts));
        other_start = next;
        index = next;
        state = next_state;
    }
    let other: String = chars[other_start..].iter().collect();
    upper.push_str(&CASE_MAPPER.uppercase_to_string(&other, &ROOT));
    upper
}

static ROOT: LanguageIdentifier = LanguageIdentifier::UNKNOWN;
const CASED: CodePointSetDataBorrowed<'static> = CodePointSetData::new::<Cased>();
const CASE_IGNORABLE: CodePointSetDataBorrowed<'static> = CodePointSetData::new::<CaseIgnorable>();

#[derive(Clone, Copy, Default)]
struct GreekState {
    after_cased: bool,
    after_accented_vowel: Option<GreekAccent>,
}

/// Where a vowel has the accent that it loses, which decides how the iota
/// or upsilon after it gains a dialytika.
#[derive(Clone, Copy)]
enum GreekAccent {
    Precomposed,
    Combining,
}

struct GreekLetter {
    base: char,
    vowel: bool,
    marks: GreekMarks,
}

#[derive(Clone, Copy, Default)]
struct GreekMarks {
    accent: bool,
    dialytika: bool,
    combining_dialytika: bool,
    ypogegrammeni: bool,
}

impl GreekMarks {
    /// Adds a Greek combining diacritic, or says that `mark` is not one.
    fn add_combining(&mut self, mark: char) -> bool {
        match mark {
            '\u{300}' | '\u{301}' | '\u{342}' | '\u{302}' | '\u{303}' | '\u{311}' => {
                self.accent = true
            }
            '\u{308}' => self.combining_dialytika = true,
            '\u{344}' => {
                self.combining_dialytika = true;
                self.accent = true;
            }
            '\u{345}' => self.ypogegrammeni = true,
            '\u{304}' | '\u{306}' | '\u{313}' | '\u{314}' | '\u{343}' => {}
            _ => return false,
        }
        true
    }
}

fn followed_by_cased_letter(chars: &[char]) -> bool {
    chars
        .iter()
        .find(|&&c| !CASE_IGNORABLE.contains(c))
        .is_some_and(|&c| CASED.contains(c))
}

/// The Greek letter `c`: its capital base letter and the diacritics it
/// carries, or None for a character that Greek uppercasing leaves to the
/// root locale.
fn greek_letter(c: char) -> Option<GreekLetter> {
    let code = c as usize;
    let packed = match code {
        0x370..=0x3FF => GREEK_LETTERS_0370[code - 0x370],
        0x1F00..=0x1FFC => GREEK_LETTERS_1F00[code - 0x1F00],
        0x2126 => 7,
        _ => 0,
    };
    if packed == 0 {
        return None;
    }
    if packed & 0x80 != 0 {
        let base = if packed & 1 != 0 { 'Ρ' } else { CASE_MAPPER.simple_uppercase(c) };
        return Some(GreekLetter { base, vowel: false, marks: GreekMarks::default() });
    }
    let base = ['Α', 'Ε', 'Η', 'Ι', 'Ο', 'Υ', 'Ω', 'ϒ'][usize::from(packed & 0xF) - 1];
    Some(GreekLetter {
        base,
        // ICU does not count the upsilon with hook as a vowel.
        vowel: base != 'ϒ',
        marks: GreekMarks {
            accent: packed & 0x40 != 0,
            dialytika: packed & 0x20 != 0,
            combining_dialytika: false,
            ypogegrammeni: packed & 0x10 != 0,
        },
    })
}

// The two tables are the Greek letter data of ICU4X (icu_casemap
// greek_to_me/data.rs), Copyright © 2020-2024 Unicode, Inc., under the
// Unicode License V3 in licenses/core/icu4x-unicode-license.md. A byte is 0
// for no Greek letter; with bit 7 a consonant, which bit 0 marks as rho;
// otherwise a vowel, whose low four bits are its capital (1 Α to 8 ϒ) and
// whose bits 6, 5 and 4 mark an accent, a dialytika and a ypogegrammeni.
const GREEK_LETTERS_0370: [u8; 0x90] = [
    128, 128, 128, 128, 0, 0, 128, 128, 0, 0, 128, 128, 128, 128, 0, 128,
    0, 0, 0, 0, 0, 0, 65, 0, 66, 67, 68, 0, 69, 0, 70, 71,
    100, 1, 128, 128, 128, 2, 128, 3, 128, 4, 128, 128, 128, 128, 128, 5,
    128, 129, 0, 128, 128, 6, 128, 128, 128, 7, 36, 38, 65, 66, 67, 68,
    102, 1, 128, 128, 128, 2, 128, 3, 128, 4, 128, 128, 128, 128, 128, 5,
    128, 129, 128, 128, 128, 6, 128, 128, 128, 7, 36, 38, 69, 70, 71, 128,
    128, 128, 8, 72, 40, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128,
    128, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    128, 129, 128, 128, 128, 2, 0, 128, 128, 128, 128, 128, 128, 128, 128, 128,
];
const GREEK_LETTERS_1F00: [u8; 0xFD] = [
    1, 1, 65, 65, 65, 65, 65, 65, 1, 1, 65, 65, 65, 65, 65, 65,
    2, 2, 66, 66, 66, 66, 0, 0, 2, 2, 66, 66, 66, 66, 0, 0,
    3, 3, 67, 67, 67, 67, 67, 67, 3, 3, 67, 67, 67, 67, 67, 67,
    4, 4, 68, 68, 68, 68, 68, 68, 4, 4, 68, 68, 68, 68, 68, 68,
    5, 5, 69, 69, 69, 69, 0, 0, 5, 5, 69, 69, 69, 69, 0, 0,
    6, 6, 70, 70, 70, 70, 70, 70, 0, 6, 0, 70, 0, 70, 0, 70,
    7, 7, 71, 71, 71, 71, 71, 71, 7, 7, 71, 71, 71, 71, 71, 71,
    65, 65, 66, 66, 67, 67, 68, 68, 69, 69, 70, 70, 71, 71, 0, 0,
    17, 17, 81, 81, 81, 81, 81, 81, 17, 17, 81, 81, 81, 81, 81, 81,
    19, 19, 83, 83, 83, 83, 83, 83, 19, 19, 83, 83, 83, 83, 83, 83,
    23, 23, 87, 87, 87, 87, 87, 87, 23, 23, 87, 87, 87, 87, 87, 87,
    1, 1, 81, 17, 81, 0, 65, 81, 1, 1, 65, 65, 17, 0, 4, 0,
    0, 0, 83, 19, 83, 0, 67, 83, 66, 66, 67, 67, 19, 0, 0, 0,
    4, 4, 100, 100, 0, 0, 68, 100, 4, 4, 68, 68, 0, 0, 0, 0,
    6, 6, 102, 102, 129, 129, 70, 102, 6, 6, 70, 70, 129, 0, 0, 0,
    0, 0, 87, 23, 87, 0, 71, 87, 69, 69, 71, 71, 23,
];

/// The language whose case mappings ICU uses for a locale ID, as
/// `ucase_getCaseLocale` picks it: the language subtag, before the first `_`
/// or `-`, in any letter case. A NULL or unknown locale maps case as the root
/// locale does.
fn case_language(locale: Option<&[u8]>) -> &'static LanguageIdentifier {
    static TURKISH: LanguageIdentifier = langid!("tr");
    static LITHUANIAN: LanguageIdentifier = langid!("lt");
    static DUTCH: LanguageIdentifier = langid!("nl");
    static ARMENIAN: LanguageIdentifier = langid!("hy");
    let Some(locale) = locale else {
        return &ROOT;
    };
    let end = locale
        .iter()
        .position(|&byte| matches!(byte, b'_' | b'-' | 0))
        .unwrap_or(locale.len());
    let language = &locale[..end];
    let is = |code: &[u8]| language.eq_ignore_ascii_case(code);
    if is(b"tr") || is(b"tur") || is(b"az") || is(b"aze") {
        &TURKISH
    } else if is(b"lt") || is(b"lit") {
        &LITHUANIAN
    } else if is(b"el") || is(b"ell") {
        &GREEK
    } else if is(b"nl") || is(b"nld") {
        &DUTCH
    } else if is(b"hy") || is(b"hye") {
        &ARMENIAN
    } else {
        &ROOT
    }
}

/// `icuLikeFunc`: like(P, S) and like(P, S, E).
fn like(args: &[&Value]) -> Result<Value> {
    let pattern = value_text(args[0]);
    let string = value_text(args[1]);
    if pattern.as_ref().map_or(0, |pattern| pattern.len()) > MAX_LIKE_PATTERN_LENGTH {
        return Err(LimboError::SqlError(
            "LIKE or GLOB pattern too complex".to_string(),
        ));
    }
    let mut escape = 0;
    if let Some(escape_value) = args.get(2) {
        let Some(escape_text) = value_text(escape_value) else {
            return Ok(Value::Null);
        };
        let (code_point, length) = first_code_point(&escape_text);
        if length != escape_text.len() {
            return Err(LimboError::SqlError(
                "ESCAPE expression must be a single character".to_string(),
            ));
        }
        escape = code_point;
    }
    Ok(match (pattern, string) {
        (Some(pattern), Some(string)) => {
            Value::from_i64(i64::from(like_compare(&pattern, &string, escape)))
        }
        _ => Value::Null,
    })
}

/// The first code point of `text` as ICU's `U8_NEXT` reads it, and the
/// number of bytes it takes. A malformed sequence is `U_SENTINEL` (-1), as
/// a u32, and takes the bytes of its longest well-formed prefix. An empty
/// text reads its NUL terminator.
fn first_code_point(text: &[u8]) -> (u32, usize) {
    let Some(chunk) = text.utf8_chunks().next() else {
        return (0, 1);
    };
    match chunk.valid().chars().next() {
        Some(c) => (u32::from(c), c.len_utf8()),
        None => (u32::MAX, chunk.invalid().len()),
    }
}

/// The byte at `i` of a text that SQLite reads as a C string: 0 at its end.
fn byte_at(text: &[u8], i: usize) -> u8 {
    text.get(i).copied().unwrap_or(0)
}

/// `SQLITE_ICU_READ_UTF8`: the code point at `i` and the position after it.
/// It reads as many continuation bytes as follow, without checking them.
fn read_utf8(text: &[u8], mut i: usize) -> (u32, usize) {
    let mut c = u32::from(byte_at(text, i));
    i += 1;
    if c >= 0xc0 {
        c = u32::from(UTF8_TRANS1[(c - 0xc0) as usize]);
        while byte_at(text, i) & 0xc0 == 0x80 {
            c = (c << 6).wrapping_add(u32::from(byte_at(text, i) & 0x3f));
            i += 1;
        }
    }
    (c, i)
}

/// `SQLITE_ICU_SKIP_UTF8`: the position after the character at `i`.
fn skip_utf8(text: &[u8], mut i: usize) -> usize {
    let lead = byte_at(text, i);
    i += 1;
    if lead >= 0xc0 {
        while byte_at(text, i) & 0xc0 == 0x80 {
            i += 1;
        }
    }
    i
}

/// `u_foldCase(c, U_FOLD_CASE_DEFAULT)`. A value that is not a Unicode
/// scalar value has no folding.
fn fold_case(c: u32) -> u32 {
    char::from_u32(c).map_or(c, |c| u32::from(CASE_MAPPER.simple_fold(c)))
}

/// `icuLikeCompare`: whether `string` matches the LIKE `pattern`. Both end at
/// their first NUL byte. The recursion of the extension at each `%` is a
/// stack of the string positions that each `%` still has to try.
fn like_compare(pattern: &[u8], string: &[u8], escape: u32) -> bool {
    const MATCH_ONE: u32 = b'_' as u32;
    const MATCH_ALL: u32 = b'%' as u32;
    struct MatchAll {
        rest_of_pattern: usize,
        string: usize,
    }
    let mut pending: Vec<MatchAll> = Vec::new();
    let mut p = 0;
    let mut s = 0;
    loop {
        let matched = 'attempt: {
            let mut prev_escape = false;
            loop {
                let (u_pattern, next) = read_utf8(pattern, p);
                p = next;
                if u_pattern == 0 {
                    break 'attempt byte_at(string, s) == 0;
                }
                if u_pattern == MATCH_ALL && !prev_escape && u_pattern != escape {
                    loop {
                        let c = byte_at(pattern, p);
                        if c != b'%' && c != b'_' {
                            break;
                        }
                        if c == b'_' {
                            if byte_at(string, s) == 0 {
                                break 'attempt false;
                            }
                            s = skip_utf8(string, s);
                        }
                        p += 1;
                    }
                    if byte_at(pattern, p) == 0 {
                        break 'attempt true;
                    }
                    if byte_at(string, s) == 0 {
                        break 'attempt false;
                    }
                    pending.push(MatchAll {
                        rest_of_pattern: p,
                        string: s,
                    });
                    prev_escape = false;
                } else if u_pattern == MATCH_ONE && !prev_escape && u_pattern != escape {
                    if byte_at(string, s) == 0 {
                        break 'attempt false;
                    }
                    s = skip_utf8(string, s);
                } else if u_pattern == escape && !prev_escape {
                    prev_escape = true;
                } else {
                    let (u_string, next) = read_utf8(string, s);
                    s = next;
                    if fold_case(u_string) != fold_case(u_pattern) {
                        break 'attempt false;
                    }
                    prev_escape = false;
                }
            }
        };
        if matched {
            return true;
        }
        loop {
            let Some(match_all) = pending.last_mut() else {
                return false;
            };
            match_all.string = skip_utf8(string, match_all.string);
            if byte_at(string, match_all.string) == 0 {
                pending.pop();
                continue;
            }
            p = match_all.rest_of_pattern;
            s = match_all.string;
            break;
        }
    }
}

/// `icuRegexpFunc`: whether the pattern matches the whole string. ICU reads
/// both as NUL-terminated UTF-16. The `regex` crate stands in for ICU's
/// regular expressions, which accept the same syntax for ordinary patterns.
fn regexp(pattern: &Value, string: &Value) -> Result<Value> {
    let Some(string) = value_text(string) else {
        return Ok(Value::Null);
    };
    let Some(pattern) = value_text(pattern) else {
        return Ok(Value::Null);
    };
    let until_nul = |text: &[u8]| text.iter().position(|&b| b == 0).unwrap_or(text.len());
    let pattern = sqlite_utf16_text(&pattern[..until_nul(&pattern)]);
    let string = sqlite_utf16_text(&string[..until_nul(&string)]);
    thread_local! {
        static LAST_PATTERN: RefCell<Option<(String, fancy_regex::Regex)>> = const { RefCell::new(None) };
    }
    LAST_PATTERN.with_borrow_mut(|last| {
        if last.as_ref().is_none_or(|(text, _)| *text != *pattern) {
            let regex = icu_regex::compile(&pattern).ok_or_else(|| {
                LimboError::SqlError("ICU error: uregex_open(): U_REGEX_RULE_SYNTAX".to_string())
            })?;
            *last = Some((pattern.to_string(), regex));
        }
        let (_, regex) = last.as_ref().expect("the pattern was compiled");
        let matched = regex.is_match(&string).map_err(|_| {
            LimboError::SqlError("ICU error: uregex_matches(): U_REGEX_STACK_OVERFLOW".to_string())
        })?;
        Ok(Value::from_i64(i64::from(matched)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Value {
        Value::build_text(value.to_string())
    }

    fn blob(value: &[u8]) -> Value {
        Value::from_slice(value).unwrap()
    }

    #[test]
    fn case_mapping_is_full_and_contextual() {
        let upper = |value: Value| case_map([value].iter(), Case::Upper);
        let lower = |value: Value| case_map([value].iter(), Case::Lower);
        assert_eq!(upper(text("straße")), text("STRASSE"));
        assert_eq!(upper(text("ǆ")), text("Ǆ"));
        assert_eq!(upper(text("ﬁ")), text("FI"));
        assert_eq!(lower(text("ΣΑΣ")), text("σας"));
        assert_eq!(lower(text("İ")), text("i\u{307}"));
        assert_eq!(upper(Value::from_i64(12)), text("12"));
        assert_eq!(upper(Value::from_f64(1.5)), text("1.5"));
        assert_eq!(upper(Value::Null), Value::Null);
    }

    #[test]
    fn case_mapping_reads_malformed_utf8_as_sqlite_converts_it_to_utf16() {
        let upper = |value: Value| case_map([value].iter(), Case::Upper);
        assert_eq!(upper(blob(b"\x80")), text("\u{80}"));
        assert_eq!(upper(blob(b"\xc3")), text("\u{fffd}"));
        assert_eq!(upper(blob(b"\xc0\x80")), text("\u{fffd}"));
        assert_eq!(upper(blob(b"\xed\xa0\x80")), text("\u{fffd}"));
        assert_eq!(upper(blob(b"\xf4\x90\x80\x80")), text("\u{10000}"));
        assert_eq!(upper(text("\u{fffe}")), text("\u{fffd}"));
        assert_eq!(upper(blob(b"a\x00b")), text("A\0B"));
    }

    #[test]
    fn locale_language_selects_case_mappings() {
        let upper_in =
            |value: &str, locale: Value| case_map([text(value), locale].iter(), Case::Upper);
        let lower_in =
            |value: &str, locale: Value| case_map([text(value), locale].iter(), Case::Lower);
        assert_eq!(upper_in("i", text("tr_TR")), text("İ"));
        assert_eq!(upper_in("i", text("TR-tr")), text("İ"));
        assert_eq!(upper_in("i", text("aze")), text("İ"));
        assert_eq!(lower_in("I", text("tr")), text("ı"));
        assert_eq!(upper_in("i", text("turk")), text("I"));
        assert_eq!(upper_in("i", text("zz_invalid!!")), text("I"));
        assert_eq!(upper_in("i", Value::Null), text("I"));
        assert_eq!(upper_in("և", text("hy")), text("ԵՎ"));
        assert_eq!(upper_in("և", text("hyw")), text("ԵՒ"));
        assert_eq!(upper_in("ά", text("el")), text("Α"));
        assert_eq!(case_map([Value::Null, text("tr")].iter(), Case::Upper), Value::Null);
    }

    #[test]
    fn greek_uppercase_follows_icu() {
        let upper_el = |value: &str| case_map([text(value), text("el")].iter(), Case::Upper);
        assert_eq!(upper_el("ἲἳἴἵ"), text("ΙΪΙΪ"));
        assert_eq!(upper_el("ὒὓὔὕ"), text("ΥΫΥΫ"));
        assert_eq!(upper_el("άίίί"), text("ΑΪΙΪ"));
        assert_eq!(upper_el("\u{3ac}\u{301}\u{3b9}\u{3b9}"), text("ΑΪΙ"));
        assert_eq!(upper_el("\u{3ac}\u{3af}\u{3b9}"), text("ΑΪΙ"));
        assert_eq!(upper_el("\u{3b1}\u{301}\u{3b9}\u{3b9}"), text("ΑΙ\u{308}Ι"));
        assert_eq!(upper_el("\u{3b8}\u{308}"), text("Θ\u{308}"));
        assert_eq!(upper_el("\u{3b1} \u{3ae}"), text("Α Ή"));
        assert_eq!(upper_el("\u{1fb3}\u{301}\u{3b9}"), text("ΑΙΙ\u{308}"));
        assert_eq!(upper_el("\u{3d3}\u{3b9}"), text("ϒΙ"));
    }

    #[test]
    fn like_folds_each_code_point_by_simple_case_folding() {
        let like2 = |pattern: &str, string: &str| like(&[&text(pattern), &text(string)]).unwrap();
        assert_eq!(like2("ä", "Ä"), Value::from_i64(1));
        assert_eq!(like2("Σ", "ς"), Value::from_i64(1));
        assert_eq!(like2("ẞ", "ß"), Value::from_i64(1));
        assert_eq!(like2("ß", "ss"), Value::from_i64(0));
        assert_eq!(like2("i", "İ"), Value::from_i64(0));
        assert_eq!(like2("%b_d", "ABCD"), Value::from_i64(1));
        assert_eq!(like2("a%", "b"), Value::from_i64(0));
        assert_eq!(like2("_", ""), Value::from_i64(0));
        assert_eq!(like2("a", "a\0b"), Value::from_i64(1));
        assert_eq!(like(&[&Value::Null, &text("a")]).unwrap(), Value::Null);
    }

    #[test]
    fn like_escape_is_one_character() {
        let like3 = |pattern: &str, string: &str, escape: &Value| {
            like(&[&text(pattern), &text(string), escape])
        };
        assert_eq!(like3("a\\%", "a%", &text("\\")).unwrap(), Value::from_i64(1));
        assert_eq!(like3("a\\%", "ab", &text("\\")).unwrap(), Value::from_i64(0));
        assert_eq!(like3("éé%", "éx", &text("é")).unwrap(), Value::from_i64(1));
        assert_eq!(like3("é%%", "é%", &text("é")).unwrap(), Value::from_i64(0));
        assert_eq!(like3("a", "a", &Value::Null).unwrap(), Value::Null);
        assert_eq!(like3("a", "a", &blob(b"\xc3")).unwrap(), Value::from_i64(1));
        for escape in [text(""), text("ab"), blob(b"\xc3\xc3")] {
            assert!(matches!(
                like3("a", "a", &escape),
                Err(LimboError::SqlError(message))
                    if message == "ESCAPE expression must be a single character"
            ));
        }
    }

    #[test]
    fn like_pattern_has_a_length_limit() {
        let pattern = text(&"a".repeat(MAX_LIKE_PATTERN_LENGTH));
        assert_eq!(like(&[&pattern, &pattern]).unwrap(), Value::from_i64(1));
        let pattern = text(&"a".repeat(MAX_LIKE_PATTERN_LENGTH + 1));
        assert!(like(&[&pattern, &Value::Null]).is_err());
    }

    #[test]
    fn regexp_matches_the_whole_string() {
        let matches = |pattern: &str, string: &str| regexp(&text(pattern), &text(string)).unwrap();
        assert_eq!(matches("b", "abc"), Value::from_i64(0));
        assert_eq!(matches("a.c", "abc"), Value::from_i64(1));
        assert_eq!(matches("a.c", "a\rc"), Value::from_i64(0));
        assert_eq!(matches("a.c", "a\u{2028}c"), Value::from_i64(0));
        assert_eq!(matches("(?s)a.c", "a\nc"), Value::from_i64(1));
        assert_eq!(matches("a|b", "b"), Value::from_i64(1));
        assert_eq!(regexp(&text("a"), &Value::Null).unwrap(), Value::Null);
        assert_eq!(regexp(&text("("), &Value::Null).unwrap(), Value::Null);
        assert!(regexp(&text("("), &text("a")).is_err());
        assert!(regexp(&text("a)|(b"), &text("a")).is_err());
    }
}
