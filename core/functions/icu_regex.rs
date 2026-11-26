//! The regular expressions of ICU's `uregex_open` and `uregex_matches`,
//! which SQLite's ICU extension runs for REGEXP. A pattern is read with
//! ICU's syntax and rules and written as a pattern of the fancy-regex
//! backtracking engine, which matches it the way ICU does.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

use fancy_regex::{Regex, RegexBuilder};
use icu_casemap::{CaseMapper, CaseMapperBorrowed};

/// ICU's pattern for `pattern`, which must match the whole text, or None
/// when ICU refuses the pattern.
pub(super) fn compile(pattern: &str) -> Option<Regex> {
    let mut parser = Parser {
        chars: pattern.chars().collect(),
        pos: 0,
        groups: 0,
        group_names: Vec::new(),
        backrefs: Vec::new(),
    };
    let node = parser.alternation(Flags::default(), 0)?;
    if parser.pos != parser.chars.len() {
        return None;
    }
    if parser.backrefs.iter().any(|&group| group > parser.groups) {
        return None;
    }
    let mut emitted = String::from("\\A(?:");
    emit(&node, &parser.group_names, &mut emitted)?;
    emitted.push_str(")\\z");
    RegexBuilder::new(&emitted).build().ok()
}

/// The pattern modes of ICU's flags `(?idmsuwx)`.
#[derive(Clone, Copy, Default)]
struct Flags {
    case_insensitive: bool,
    multiline: bool,
    dot_all: bool,
    comments: bool,
    unix_lines: bool,
    unicode_words: bool,
}

#[derive(Debug)]
enum Node {
    Empty,
    /// Literal characters, compared without case in a case-insensitive
    /// mode.
    Literal(Vec<char>, bool),
    /// A character class in the syntax of fancy-regex.
    Class(String),
    /// `\R`: a line terminator, or CR LF.
    LineBreak,
    Concat(Vec<Node>),
    Alternation(Vec<Node>),
    Group(GroupKind, Box<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
        kind: RepeatKind,
    },
    Backref(usize, bool),
    NamedBackref(String, bool),
    Assertion(String),
}

#[derive(Debug, Clone)]
enum GroupKind {
    NonCapturing,
    /// A group with a number, and maybe a name, that a backreference names.
    Capturing,
    Atomic,
    LookAhead,
    NegativeLookAhead,
    LookBehind,
    NegativeLookBehind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepeatKind {
    Greedy,
    Lazy,
    Possessive,
}

/// The line terminators of ICU.
const LINE_TERMINATORS: &str = "\\n\\x0B\\x0C\\r\\x{85}\\x{2028}\\x{2029}";

/// The characters of `\w` in ICU.
const WORD_CHARACTERS: &str = "\\p{Alphabetic}\\p{M}\\p{Nd}\\p{Pc}\\x{200C}\\x{200D}";

struct Parser {
    chars: Vec<char>,
    pos: usize,
    /// The capture groups opened so far.
    groups: usize,
    /// The names of the named capture groups, with their numbers.
    group_names: Vec<(String, usize)>,
    backrefs: Vec<usize>,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Skips the white space and comments of the comments mode.
    fn skip_comments(&mut self, flags: Flags) {
        if !flags.comments {
            return;
        }
        while let Some(c) = self.peek() {
            if is_pattern_white_space(c) {
                self.pos += 1;
            } else if c == '#' {
                while let Some(c) = self.peek() {
                    self.pos += 1;
                    if is_line_terminator(c) {
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    /// An alternation, up to a `)` at `depth` greater than 0 or to the end.
    fn alternation(&mut self, mut flags: Flags, depth: usize) -> Option<Node> {
        let mut branches = Vec::new();
        loop {
            let (branch, branch_flags) = self.sequence(flags)?;
            branches.push(branch);
            flags = branch_flags;
            match self.peek() {
                Some('|') => self.pos += 1,
                Some(')') if depth > 0 => break,
                None => break,
                Some(_) => return None,
            }
        }
        Some(if branches.len() == 1 {
            branches.pop().expect("one branch")
        } else {
            Node::Alternation(branches)
        })
    }

    /// A sequence of quantified atoms, and the flags in effect at its end:
    /// a `(?i)` holds until the end of the enclosing group.
    fn sequence(&mut self, mut flags: Flags) -> Option<(Node, Flags)> {
        let mut items: Vec<Node> = Vec::new();
        loop {
            self.skip_comments(flags);
            let Some(c) = self.peek() else { break };
            if c == '|' || c == ')' {
                break;
            }
            let atom = match c {
                '(' => {
                    self.pos += 1;
                    match self.group(&mut flags)? {
                        Some(node) => node,
                        None => continue,
                    }
                }
                '[' => {
                    self.pos += 1;
                    let class = if self.eat(':') {
                        self.posix_class(flags)?
                    } else {
                        self.class(flags)?
                    };
                    Node::Class(case_class(class, flags))
                }
                '.' => {
                    self.pos += 1;
                    Node::Class(if flags.dot_all {
                        "(?s:.)".to_string()
                    } else if flags.unix_lines {
                        "[^\\n]".to_string()
                    } else {
                        format!("[^{LINE_TERMINATORS}]")
                    })
                }
                '^' => {
                    self.pos += 1;
                    Node::Assertion(caret(flags))
                }
                '$' => {
                    self.pos += 1;
                    Node::Assertion(dollar(flags))
                }
                '\\' => {
                    self.pos += 1;
                    match self.escape(flags)? {
                        Escaped::Node(node) => node,
                        Escaped::Quoted(mut text) => {
                            // A quantifier after the quote repeats its last
                            // character.
                            let Some(last) = text.pop() else { continue };
                            for c in text {
                                push_literal(&mut items, c, flags);
                            }
                            Node::Literal(vec![last], flags.case_insensitive)
                        }
                    }
                }
                '*' | '+' | '?' | '{' | '}' => return None,
                c => {
                    self.pos += 1;
                    Node::Literal(vec![c], flags.case_insensitive)
                }
            };
            let atom = self.quantified(atom, flags)?;
            match atom {
                Node::Literal(chars, case_insensitive) if chars.len() == 1 => {
                    push_literal(&mut items, chars[0], Flags { case_insensitive, ..flags })
                }
                atom => items.push(atom),
            }
        }
        let node = match items.len() {
            0 => Node::Empty,
            1 => items.pop().expect("one item"),
            _ => Node::Concat(items),
        };
        Some((node, flags))
    }

    /// The quantifiers after `atom`.
    fn quantified(&mut self, atom: Node, flags: Flags) -> Option<Node> {
        self.skip_comments(flags);
        let (min, max) = match self.peek() {
            Some('*') => (0, None),
            Some('+') => (1, None),
            Some('?') => (0, Some(1)),
            Some('{') => {
                self.pos += 1;
                let (min, max) = self.interval()?;
                self.pos -= 1;
                (min, max)
            }
            _ => return Some(atom),
        };
        self.pos += 1;
        if !quantifiable(&atom) {
            return None;
        }
        let kind = if self.eat('?') {
            RepeatKind::Lazy
        } else if self.eat('+') {
            RepeatKind::Possessive
        } else {
            RepeatKind::Greedy
        };
        self.skip_comments(flags);
        if matches!(self.peek(), Some('*' | '+' | '?' | '{')) {
            return None;
        }
        Some(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            kind,
        })
    }

    /// `{n}`, `{n,}` or `{n,m}` after the `{`, leaving the position after
    /// the `}`.
    fn interval(&mut self) -> Option<(u32, Option<u32>)> {
        let min = self.number()?;
        let max = if self.eat(',') {
            if self.peek() == Some('}') {
                None
            } else {
                Some(self.number()?)
            }
        } else {
            Some(min)
        };
        if !self.eat('}') || max.is_some_and(|max| max < min) {
            return None;
        }
        Some((min, max))
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.pos;
        let mut value: u32 = 0;
        while let Some(digit) = self.peek().and_then(|c| c.to_digit(10)) {
            value = value.checked_mul(10)?.checked_add(digit)?;
            self.pos += 1;
        }
        (self.pos > start).then_some(value)
    }

    /// The group after `(`, or None for a flag setting that is no group.
    fn group(&mut self, flags: &mut Flags) -> Option<Option<Node>> {
        let mut inner_flags = *flags;
        let kind = if self.eat('?') {
            match self.peek()? {
                ':' => {
                    self.pos += 1;
                    GroupKind::NonCapturing
                }
                '>' => {
                    self.pos += 1;
                    GroupKind::Atomic
                }
                '=' => {
                    self.pos += 1;
                    GroupKind::LookAhead
                }
                '!' => {
                    self.pos += 1;
                    GroupKind::NegativeLookAhead
                }
                '#' => {
                    while self.peek()? != ')' {
                        self.pos += 1;
                    }
                    self.pos += 1;
                    return Some(None);
                }
                '<' => {
                    self.pos += 1;
                    if self.eat('=') {
                        GroupKind::LookBehind
                    } else if self.eat('!') {
                        GroupKind::NegativeLookBehind
                    } else {
                        let name = self.group_name()?;
                        if !self.eat('>') || self.group_names.iter().any(|(known, _)| *known == name) {
                            return None;
                        }
                        self.groups += 1;
                        self.group_names.push((name, self.groups));
                        GroupKind::Capturing
                    }
                }
                _ => {
                    let mut set = true;
                    loop {
                        let c = self.peek()?;
                        self.pos += 1;
                        let flag = match c {
                            'i' => &mut inner_flags.case_insensitive,
                            'm' => &mut inner_flags.multiline,
                            's' => &mut inner_flags.dot_all,
                            'x' => &mut inner_flags.comments,
                            'd' => &mut inner_flags.unix_lines,
                            'w' => &mut inner_flags.unicode_words,
                            'u' => continue,
                            '-' if set => {
                                set = false;
                                continue;
                            }
                            ')' => {
                                *flags = inner_flags;
                                return Some(None);
                            }
                            ':' => break,
                            _ => return None,
                        };
                        *flag = set;
                    }
                    GroupKind::NonCapturing
                }
            }
        } else {
            self.groups += 1;
            GroupKind::Capturing
        };
        let inner = self.alternation(inner_flags, 1)?;
        if !self.eat(')') {
            return None;
        }
        Some(Some(Node::Group(kind, Box::new(inner))))
    }

    fn group_name(&mut self) -> Option<String> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric()) {
            self.pos += 1;
        }
        let name: String = self.chars[start..self.pos].iter().collect();
        name.starts_with(|c: char| c.is_ascii_alphabetic()).then_some(name)
    }

    /// The escape after `\` outside a class.
    fn escape(&mut self, flags: Flags) -> Option<Escaped> {
        let c = self.peek()?;
        self.pos += 1;
        let class = |class: &str| Some(Escaped::Node(Node::Class(case_class(class.to_string(), flags))));
        let assertion = |text: String| Some(Escaped::Node(Node::Assertion(text)));
        match c {
            'A' | 'G' => assertion("\\A".to_string()),
            'z' => assertion("\\z".to_string()),
            'Z' => assertion(dollar(Flags { multiline: false, ..flags })),
            // Chromium's ICU has no word break data for the boundaries of
            // the w flag, so they hold nowhere.
            'b' | 'B' if flags.unicode_words => assertion("(?!)".to_string()),
            'b' | 'B' => {
                let word = format!("[{WORD_CHARACTERS}]");
                let boundary = format!(
                    "(?![\\p{{Grapheme_Extend}}\\p{{Cf}}])(?:(?<={word})(?!{word})|(?<!{word})(?={word}))"
                );
                assertion(if c == 'b' {
                    boundary
                } else {
                    format!("(?!{boundary})")
                })
            }
            'd' => class("\\p{Nd}"),
            'D' => class("\\P{Nd}"),
            's' => class("\\p{White_Space}"),
            'S' => class("\\P{White_Space}"),
            'w' => class(&format!("[{WORD_CHARACTERS}]")),
            'W' => class(&format!("[^{WORD_CHARACTERS}]")),
            'h' => class("[\\t\\p{Zs}]"),
            'H' => class("[^\\t\\p{Zs}]"),
            'v' => class(&format!("[{LINE_TERMINATORS}]")),
            'V' => class(&format!("[^{LINE_TERMINATORS}]")),
            'R' => Some(Escaped::Node(Node::LineBreak)),
            'p' | 'P' => {
                let property = self.property(c == 'P')?;
                class(&property)
            }
            // Chromium's ICU has no grapheme break data, so \X matches no
            // character.
            'X' => Some(Escaped::Node(Node::Assertion("(?!)".to_string()))),
            'N' => None,
            'Q' => {
                let start = self.pos;
                while self.pos < self.chars.len()
                    && !(self.chars[self.pos] == '\\' && self.peek_at(1) == Some('E'))
                {
                    self.pos += 1;
                }
                let text = self.chars[start..self.pos].to_vec();
                if self.pos < self.chars.len() {
                    self.pos += 2;
                }
                Some(Escaped::Quoted(text))
            }
            'k' => {
                if !self.eat('<') {
                    return None;
                }
                let name = self.group_name()?;
                if !self.eat('>') {
                    return None;
                }
                Some(Escaped::Node(Node::NamedBackref(name, flags.case_insensitive)))
            }
            '1'..='9' => {
                let mut group = c.to_digit(10)? as usize;
                while group < self.groups {
                    let Some(digit) = self.peek().and_then(|c| c.to_digit(10)) else { break };
                    self.pos += 1;
                    group = group * 10 + digit as usize;
                }
                self.backrefs.push(group);
                Some(Escaped::Node(Node::Backref(group, flags.case_insensitive)))
            }
            _ => {
                self.pos -= 1;
                let c = self.character_escape()?;
                Some(Escaped::Node(Node::Literal(vec![c], flags.case_insensitive)))
            }
        }
    }

    /// An escape that stands for one character, after the `\`, in a class
    /// or outside one.
    fn character_escape(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        Some(match c {
            'a' => '\u{7}',
            'e' => '\u{1B}',
            'f' => '\u{C}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'c' => match self.peek() {
                Some(control) => {
                    self.pos += 1;
                    char::from_u32(u32::from(control) & 0x1F)?
                }
                None => 'c',
            },
            'u' => self.hex_digits(4, 4)?,
            'U' => self.hex_digits(8, 8)?,
            'x' => {
                if self.eat('{') {
                    let start = self.pos;
                    while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                        self.pos += 1;
                    }
                    if self.pos == start || self.pos - start > 8 || !self.eat('}') {
                        return None;
                    }
                    let digits: String = self.chars[start..self.pos - 1].iter().collect();
                    char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?
                } else {
                    self.hex_digits(0, 2)?
                }
            }
            '0' => {
                let mut value = 0u32;
                let mut digits = 0;
                while digits < 3 {
                    let Some(digit) = self.peek().and_then(|c| c.to_digit(8)) else { break };
                    if value * 8 + digit > 0o377 {
                        break;
                    }
                    value = value * 8 + digit;
                    digits += 1;
                    self.pos += 1;
                }
                if digits == 0 {
                    return None;
                }
                char::from_u32(value)?
            }
            c if c.is_ascii_digit() => return None,
            c => c,
        })
    }

    /// `count` hexadecimal digits, from `min` to `max` of them.
    fn hex_digits(&mut self, min: usize, max: usize) -> Option<char> {
        let start = self.pos;
        while self.pos - start < max && self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
            self.pos += 1;
        }
        if self.pos - start < min {
            return None;
        }
        let digits: String = self.chars[start..self.pos].iter().collect();
        if digits.is_empty() {
            return Some('\0');
        }
        char::from_u32(u32::from_str_radix(&digits, 16).ok()?)
    }

    /// The class of `\p{...}` or `\P{...}` after the letter.
    fn property(&mut self, negated: bool) -> Option<String> {
        if !self.eat('{') {
            return None;
        }
        let start = self.pos;
        while self.peek()? != '}' {
            self.pos += 1;
        }
        let name: String = self.chars[start..self.pos].iter().collect();
        self.pos += 1;
        property_class(&name, negated)
    }

    /// A class after its `[`, up to and with its `]`, in fancy-regex syntax.
    fn class(&mut self, flags: Flags) -> Option<String> {
        let negated = self.eat('^');
        let mut items = String::new();
        let mut first = true;
        // The operands of `&&` and `--`, each a class in fancy-regex syntax.
        let mut operands: Vec<(String, Option<&'static str>)> = Vec::new();
        let mut pending_operator: Option<&'static str> = None;
        loop {
            if flags.comments {
                while self.peek().is_some_and(is_pattern_white_space) {
                    self.pos += 1;
                }
            }
            let c = self.peek()?;
            match c {
                ']' if !first => {
                    self.pos += 1;
                    break;
                }
                '[' => {
                    self.pos += 1;
                    if self.eat(':') {
                        items.push_str(&self.posix_class(flags)?);
                    } else {
                        let nested = self.class(flags)?;
                        items.push_str(&nested);
                    }
                }
                '&' if self.peek_at(1) == Some('&') || self.peek_at(1) == Some('[') && !items.is_empty() => {
                    self.pos += if self.peek_at(1) == Some('&') { 2 } else { 1 };
                    operands.push((std::mem::take(&mut items), pending_operator.take()));
                    pending_operator = Some("&&");
                }
                '-' if self.peek_at(1) == Some('-')
                    || self.peek_at(1) == Some('[') && !items.is_empty() && !first =>
                {
                    self.pos += if self.peek_at(1) == Some('-') { 2 } else { 1 };
                    operands.push((std::mem::take(&mut items), pending_operator.take()));
                    pending_operator = Some("--");
                }
                _ => {
                    let start = self.class_character(&mut items)?;
                    if let Some(start) = start {
                        if self.peek() == Some('-')
                            && self.peek_at(1).is_some_and(|next| next != ']')
                            && self.peek_at(1) != Some('-')
                            && self.peek_at(1) != Some('[')
                        {
                            self.pos += 1;
                            let mut end_items = String::new();
                            let end = self.class_character(&mut end_items)??;
                            if end < start {
                                return None;
                            }
                            let _ = write!(items, "\\x{{{:X}}}-\\x{{{:X}}}", start as u32, end as u32);
                        } else {
                            let _ = write!(items, "\\x{{{:X}}}", start as u32);
                        }
                    }
                }
            }
            first = false;
        }
        operands.push((items, pending_operator.take()));
        let mut class = String::new();
        for (items, operator) in operands {
            if let Some(operator) = operator {
                class.push_str(operator);
            }
            if items.is_empty() {
                class.push_str("[^\\x{0}-\\x{10FFFF}]");
            } else {
                let _ = write!(class, "[{items}]");
            }
        }
        Some(if negated {
            format!("[^{class}]")
        } else {
            format!("[{class}]")
        })
    }

    /// One character of a class, or the class of an escape such as `\d`,
    /// which goes to `items`.
    fn class_character(&mut self, items: &mut String) -> Option<Option<char>> {
        let c = self.peek()?;
        self.pos += 1;
        if c != '\\' {
            return Some(Some(c));
        }
        let escaped = self.peek()?;
        let class = match escaped {
            'd' => "\\p{Nd}".to_string(),
            'D' => "\\P{Nd}".to_string(),
            's' => "\\p{White_Space}".to_string(),
            'S' => "\\P{White_Space}".to_string(),
            'w' => format!("[{WORD_CHARACTERS}]"),
            'W' => format!("[^{WORD_CHARACTERS}]"),
            'h' => "[\\t\\p{Zs}]".to_string(),
            'H' => "[^\\t\\p{Zs}]".to_string(),
            'v' => format!("[{LINE_TERMINATORS}]"),
            'V' => format!("[^{LINE_TERMINATORS}]"),
            'p' | 'P' => {
                self.pos += 1;
                let property = self.property(escaped == 'P')?;
                items.push_str(&property);
                return Some(None);
            }
            'Q' => {
                self.pos += 1;
                while self.pos < self.chars.len()
                    && !(self.chars[self.pos] == '\\' && self.peek_at(1) == Some('E'))
                {
                    let _ = write!(items, "\\x{{{:X}}}", self.chars[self.pos] as u32);
                    self.pos += 1;
                }
                if self.pos < self.chars.len() {
                    self.pos += 2;
                }
                return Some(None);
            }
            'N' | 'X' | 'R' => return None,
            'b' => {
                self.pos += 1;
                return Some(Some('\u{8}'));
            }
            _ => return Some(Some(self.character_escape()?)),
        };
        self.pos += 1;
        items.push_str(&class);
        Some(None)
    }

    /// A POSIX class after its `[:`, up to and with its `:]`.
    fn posix_class(&mut self, flags: Flags) -> Option<String> {
        let skip = |c: &char| flags.comments && is_pattern_white_space(*c);
        let start = self.pos;
        while self.peek()? != ':' {
            self.pos += 1;
        }
        let written: String = self.chars[start..self.pos].iter().filter(|c| !skip(c)).collect();
        let (negated, name) = match written.strip_prefix('^') {
            Some(name) => (true, name.to_string()),
            None => (false, written),
        };
        self.pos += 1;
        if !self.eat(']') {
            return None;
        }
        let class = posix_class(&name)?;
        Some(if negated { format!("[^{class}]") } else { format!("[{class}]") })
    }
}

enum Escaped {
    Node(Node),
    /// The characters of `\Q...\E`.
    Quoted(Vec<char>),
}

/// Adds a literal character to a sequence, joining it to a literal before
/// it, since ICU compares a run of literal characters as one string.
fn push_literal(items: &mut Vec<Node>, c: char, flags: Flags) {
    if let Some(Node::Literal(chars, case_insensitive)) = items.last_mut() {
        if *case_insensitive == flags.case_insensitive {
            chars.push(c);
            return;
        }
    }
    items.push(Node::Literal(vec![c], flags.case_insensitive));
}

fn quantifiable(node: &Node) -> bool {
    match node {
        Node::Assertion(text) => text == "\\A",
        Node::Group(
            GroupKind::LookAhead
            | GroupKind::NegativeLookAhead
            | GroupKind::LookBehind
            | GroupKind::NegativeLookBehind,
            _,
        ) => false,
        _ => true,
    }
}

fn caret(flags: Flags) -> String {
    if !flags.multiline {
        "\\A".to_string()
    } else if flags.unix_lines {
        "(?:\\A|(?<=\\n)(?!\\z))".to_string()
    } else {
        format!("(?:\\A|(?<=[{LINE_TERMINATORS}])(?!\\z))")
    }
}

fn dollar(flags: Flags) -> String {
    match (flags.multiline, flags.unix_lines) {
        (false, true) => "(?:\\z|(?=\\n\\z))".to_string(),
        (true, true) => "(?:\\z|(?=\\n))".to_string(),
        (false, false) => {
            "(?:\\z|(?=\\r\\n\\z)|(?<!\\r)(?=\\n\\z)|(?=[\\x0B\\x0C\\r\\x{85}\\x{2028}\\x{2029}]\\z))"
                .to_string()
        }
        (true, false) => "(?:\\z|(?=[\\x0B\\x0C\\r\\x{85}\\x{2028}\\x{2029}])|(?<!\\r)(?=\\n))".to_string(),
    }
}

fn case_class(class: String, flags: Flags) -> String {
    if flags.case_insensitive {
        format!("(?i:{class})")
    } else {
        class
    }
}

fn is_pattern_white_space(c: char) -> bool {
    matches!(c, '\t'..='\r' | ' ' | '\u{85}' | '\u{200E}' | '\u{200F}' | '\u{2028}' | '\u{2029}')
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n'..='\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// The class of a POSIX class name, as ICU's sets define them.
fn posix_class(name: &str) -> Option<String> {
    let graph = "[^\\p{White_Space}\\p{Cc}\\p{Cn}]";
    Some(match name.to_ascii_lowercase().as_str() {
        "alpha" => "\\p{Alphabetic}".to_string(),
        "digit" => "\\p{Nd}".to_string(),
        "alnum" => "\\p{Alphabetic}\\p{Nd}".to_string(),
        "upper" => "\\p{Uppercase}".to_string(),
        "lower" => "\\p{Lowercase}".to_string(),
        "space" => "\\p{White_Space}".to_string(),
        "blank" => "\\t\\p{Zs}".to_string(),
        "cntrl" => "\\p{Cc}".to_string(),
        "punct" => "\\p{P}".to_string(),
        "xdigit" => "\\p{Nd}\\p{Hex_Digit}".to_string(),
        "graph" => graph.to_string(),
        "print" => format!("[{graph}\\t\\p{{Zs}}]--\\p{{Cc}}"),
        "word" => WORD_CHARACTERS.to_string(),
        _ => return None,
    })
}

/// The class of the property `\p{name}`, or of `\P{name}`.
fn property_class(name: &str, negated: bool) -> Option<String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || " _-=.".contains(c)) {
        return None;
    }
    let class = format!("\\{}{{{name}}}", if negated { 'P' } else { 'p' });
    regex_syntax::Parser::new().parse(&class).ok()?;
    Some(class)
}

/// Writes `node` in fancy-regex syntax. Named groups are written as
/// numbered groups, since fancy-regex takes no numbered backreference in a
/// pattern with named groups, and ICU takes both.
fn emit(node: &Node, names: &[(String, usize)], out: &mut String) -> Option<()> {
    match node {
        Node::Empty => {}
        Node::Literal(chars, false) => {
            for &c in chars {
                let _ = write!(out, "\\x{{{:X}}}", c as u32);
            }
        }
        Node::Literal(chars, true) => emit_case_insensitive(chars, out)?,
        Node::Class(class) | Node::Assertion(class) => out.push_str(class),
        Node::LineBreak => {
            let _ = write!(out, "(?>\\r\\n|[{LINE_TERMINATORS}])");
        }
        Node::Concat(nodes) => {
            for node in nodes {
                emit(node, names, out)?;
            }
        }
        Node::Alternation(nodes) => {
            out.push_str("(?:");
            for (index, node) in nodes.iter().enumerate() {
                if index > 0 {
                    out.push('|');
                }
                emit(node, names, out)?;
            }
            out.push(')');
        }
        Node::Group(kind, inner) => {
            out.push_str(group_open(kind));
            if matches!(kind, GroupKind::LookBehind | GroupKind::NegativeLookBehind) {
                out.push_str(&fixed_length_alternatives(inner, names)?.join("|"));
            } else {
                emit(inner, names, out)?;
            }
            out.push(')');
        }
        Node::Repeat { node, min, .. } if matches!(**node, Node::Assertion(_)) => {
            // A repeated assertion holds when it holds once.
            if *min > 0 {
                emit(node, names, out)?;
            }
        }
        Node::Repeat { node, .. } if matches_only_empty(node) => {}
        Node::Repeat { node, min, max, kind } => {
            if *kind == RepeatKind::Possessive {
                out.push_str("(?>");
            }
            out.push_str("(?:");
            emit(node, names, out)?;
            out.push(')');
            match (min, max) {
                (0, None) => out.push('*'),
                (1, None) => out.push('+'),
                (0, Some(1)) => out.push('?'),
                (min, None) => {
                    let _ = write!(out, "{{{min},}}");
                }
                (min, Some(max)) => {
                    let _ = write!(out, "{{{min},{max}}}");
                }
            }
            match kind {
                RepeatKind::Lazy => out.push('?'),
                RepeatKind::Possessive => out.push(')'),
                RepeatKind::Greedy => {}
            }
        }
        Node::Backref(group, case_insensitive) => emit_backref(*group, *case_insensitive, out),
        Node::NamedBackref(name, case_insensitive) => {
            let (_, group) = names.iter().find(|(known, _)| known == name)?;
            emit_backref(*group, *case_insensitive, out);
        }
    }
    Some(())
}

fn matches_only_empty(node: &Node) -> bool {
    match node {
        Node::Empty => true,
        Node::Group(GroupKind::NonCapturing, inner) => matches_only_empty(inner),
        _ => false,
    }
}

fn emit_backref(group: usize, case_insensitive: bool, out: &mut String) {
    let flags = if case_insensitive { "(?i:" } else { "(?:" };
    let _ = write!(out, "{flags}\\{group})");
}

fn group_open(kind: &GroupKind) -> &'static str {
    match kind {
        GroupKind::NonCapturing => "(?:",
        GroupKind::Capturing => "(",
        GroupKind::Atomic => "(?>",
        GroupKind::LookAhead => "(?=",
        GroupKind::NegativeLookAhead => "(?!",
        GroupKind::LookBehind => "(?<=",
        GroupKind::NegativeLookBehind => "(?<!",
    }
}

/// The alternatives of a look-behind, each of a fixed length, which
/// fancy-regex can look behind for. ICU looks behind for any pattern of
/// bounded length.
fn fixed_length_alternatives(node: &Node, names: &[(String, usize)]) -> Option<Vec<String>> {
    const LIMIT: usize = 256;
    let joined = |parts: Vec<Vec<String>>| -> Option<Vec<String>> {
        let mut alternatives = vec![String::new()];
        for next in parts {
            let mut joined = Vec::new();
            for prefix in &alternatives {
                for suffix in &next {
                    joined.push(format!("{prefix}{suffix}"));
                }
            }
            if joined.len() > LIMIT {
                return None;
            }
            alternatives = joined;
        }
        Some(alternatives)
    };
    let alternatives = match node {
        Node::Empty => vec![String::new()],
        Node::Backref(..) | Node::NamedBackref(..) => return None,
        Node::Literal(chars, true) => {
            let literal: String = chars.iter().collect();
            let folded: Vec<char> = CASE_MAPPER.fold_string(&literal).chars().collect();
            folded_alternatives(&folded, LIMIT)?
        }
        Node::Literal(..) | Node::Class(_) | Node::Assertion(_) => {
            let mut out = String::new();
            emit(node, names, &mut out)?;
            vec![out]
        }
        Node::LineBreak => vec!["\\r\\n".to_string(), format!("[{LINE_TERMINATORS}]")],
        Node::Concat(nodes) => joined(
            nodes
                .iter()
                .map(|node| fixed_length_alternatives(node, names))
                .collect::<Option<_>>()?,
        )?,
        Node::Alternation(nodes) => {
            let mut alternatives = Vec::new();
            for node in nodes {
                alternatives.extend(fixed_length_alternatives(node, names)?);
            }
            alternatives
        }
        Node::Group(GroupKind::NonCapturing, inner) => fixed_length_alternatives(inner, names)?,
        Node::Group(kind @ GroupKind::Capturing, inner) => vec![format!(
            "{}{})",
            group_open(kind),
            fixed_length_alternatives(inner, names)?.join("|")
        )],
        Node::Group(kind, inner) => fixed_length_alternatives(inner, names)?
            .into_iter()
            .map(|inner| format!("{}{inner})", group_open(kind)))
            .collect(),
        // ICU does not look behind for a lazy repetition.
        Node::Repeat { kind: RepeatKind::Lazy, .. } => return None,
        Node::Repeat { node, min, max, .. } => {
            let max = (*max)?;
            let once = fixed_length_alternatives(node, names)?;
            let mut alternatives = Vec::new();
            for count in *min..=max {
                alternatives.extend(joined(vec![once.clone(); count as usize])?);
            }
            alternatives
        }
    };
    (alternatives.len() <= LIMIT).then_some(alternatives)
}

/// Each fixed-length text whose full case folding is `folded`, as in
/// [`emit_folded`].
fn folded_alternatives(folded: &[char], limit: usize) -> Option<Vec<String>> {
    let Some(&first) = folded.first() else { return Some(vec![String::new()]) };
    let mut alternatives = Vec::new();
    let single = format!("(?i:\\x{{{:X}}})", first as u32);
    for rest in folded_alternatives(&folded[1..], limit)? {
        alternatives.push(format!("{single}{rest}"));
    }
    for (fold, chars) in multiple_character_folds() {
        let fold: Vec<char> = fold.chars().collect();
        if !folded.starts_with(&fold) {
            continue;
        }
        let mut class = String::from("[");
        for &c in chars {
            let _ = write!(class, "\\x{{{:X}}}", c as u32);
        }
        class.push(']');
        for rest in folded_alternatives(&folded[fold.len()..], limit)? {
            alternatives.push(format!("{class}{rest}"));
        }
    }
    (alternatives.len() <= limit).then_some(alternatives)
}

const CASE_MAPPER: CaseMapperBorrowed<'static> = CaseMapper::new();

/// Literal characters compared as ICU compares them without case: the text
/// matches when its full case folding is the full case folding of the
/// characters.
fn emit_case_insensitive(chars: &[char], out: &mut String) -> Option<()> {
    let literal: String = chars.iter().collect();
    let folded: Vec<char> = CASE_MAPPER.fold_string(&literal).chars().collect();
    out.push_str("(?:");
    emit_folded(&folded, out)?;
    out.push(')');
    Some(())
}

/// The text whose full case folding is `folded`: each character of the
/// text folds either to one character of `folded`, or to several.
fn emit_folded(folded: &[char], out: &mut String) -> Option<()> {
    // Each character that folds to several makes another branch, so a long
    // run of them makes a pattern too large to compile.
    const MAX_PATTERN_LENGTH: usize = 1 << 20;
    if out.len() > MAX_PATTERN_LENGTH {
        return None;
    }
    let Some(&first) = folded.first() else { return Some(()) };
    let multiple: Vec<(&str, &[char])> = multiple_character_folds()
        .iter()
        .filter(|(fold, _)| {
            let fold: Vec<char> = fold.chars().collect();
            folded.starts_with(&fold)
        })
        .map(|(fold, chars)| (fold.as_str(), chars.as_slice()))
        .collect();
    let mut single = String::new();
    let _ = write!(single, "(?i:\\x{{{:X}}})", first as u32);
    if multiple.is_empty() {
        out.push_str(&single);
        return emit_folded(&folded[1..], out);
    }
    out.push_str("(?:");
    out.push_str(&single);
    emit_folded(&folded[1..], out)?;
    for (fold, chars) in multiple {
        out.push('|');
        out.push('[');
        for &c in chars {
            let _ = write!(out, "\\x{{{:X}}}", c as u32);
        }
        out.push(']');
        emit_folded(&folded[fold.chars().count()..], out)?;
    }
    out.push(')');
    Some(())
}

/// The characters whose full case folding has more than one character, by
/// their folding.
fn multiple_character_folds() -> &'static HashMap<String, Vec<char>> {
    static FOLDS: OnceLock<HashMap<String, Vec<char>>> = OnceLock::new();
    FOLDS.get_or_init(|| {
        let mut folds: HashMap<String, Vec<char>> = HashMap::new();
        for c in (0..=0xFFFF).filter_map(char::from_u32) {
            let mut buffer = [0; 4];
            let folded = CASE_MAPPER.fold_string(c.encode_utf8(&mut buffer));
            if folded.chars().count() > 1 {
                folds.entry(folded.into_owned()).or_default().push(c);
            }
        }
        folds
    })
}

#[cfg(test)]
mod tests {
    use super::compile;

    fn matches(pattern: &str, text: &str) -> Option<bool> {
        Some(compile(pattern)?.is_match(text).expect("no backtrack limit"))
    }

    #[test]
    fn syntax_that_the_regex_crate_lacks() {
        assert_eq!(matches(r"(a)\1", "aa"), Some(true));
        assert_eq!(matches(r"\1(a)", "a"), Some(false));
        assert_eq!(matches(r"(a)\10", "aa0"), Some(true));
        assert_eq!(matches(r"(?<n>a)(b)\k<n>\2", "abab"), Some(true));
        assert_eq!(matches(r".*(?<=ab|c)d", "abd"), Some(true));
        assert_eq!(matches(r"..(?<=a{1,2})b", "aab"), Some(true));
        assert_eq!(matches(r"(?<=a+)b", "ab"), None);
        assert_eq!(matches(r"a++a", "aaa"), Some(false));
        assert_eq!(matches(r"(?>a|ab)c", "abc"), Some(false));
        assert_eq!(matches(r"\Qa.b\E", "a.b"), Some(true));
        assert_eq!(matches(r"\Qa.b\E", "axb"), Some(false));
        assert_eq!(matches(r"a(?#c)b", "ab"), Some(true));
    }

    #[test]
    fn character_sets_are_icu_sets() {
        assert_eq!(matches(r"a\hb", "a\u{a0}b"), Some(true));
        assert_eq!(matches(r"a\hb", "a3b"), Some(false));
        assert_eq!(matches(r"a\vb", "a\u{c}b"), Some(true));
        assert_eq!(matches(r"a\Rb", "a\r\nb"), Some(true));
        assert_eq!(matches(r"a\R\nb", "a\r\nb"), Some(false));
        assert_eq!(matches("a.b", "a\u{b}b"), Some(false));
        assert_eq!(matches("[[:alpha:]]", "é"), Some(true));
        assert_eq!(matches("[[:xdigit:]]", "\u{ff26}"), Some(true));
        assert_eq!(matches("[[:punct:]]", "$"), Some(false));
        assert_eq!(matches("[:alpha:]", ":"), Some(false));
        assert_eq!(matches("[[a-c]&[b-d]]", "&"), Some(false));
        assert_eq!(matches("[[:foo:]]", "f"), None);
        assert_eq!(matches(r"\pL", "a"), None);
    }

    #[test]
    fn case_insensitive_literals_compare_full_case_foldings() {
        assert_eq!(matches("(?i)ß", "SS"), Some(true));
        assert_eq!(matches("(?i)ss", "\u{1e9e}"), Some(true));
        assert_eq!(matches("(?i)\u{fb01}", "FI"), Some(true));
        assert_eq!(matches("(?i)[ß]", "ss"), Some(false));
    }

    #[test]
    fn anchors_follow_icu_line_terminators() {
        assert_eq!(matches("a$\n", "a\n"), Some(true));
        assert_eq!(matches("a$\u{b}", "a\u{b}"), Some(true));
        assert_eq!(matches("a\r$\n", "a\r\n"), Some(false));
        assert_eq!(matches(r"a\Z\r\n", "a\r\n"), Some(true));
        assert_eq!(matches("(?m)a\n^", "a\n"), Some(false));
        assert_eq!(matches("(?m)a\r^\nb", "a\r\nb"), Some(true));
        assert_eq!(matches("(?d).", "\r"), Some(true));
    }

    #[test]
    fn escapes_and_quantifiers_follow_icu() {
        assert_eq!(matches(r"\j", "j"), Some(true));
        assert_eq!(matches(r"\x4", "\u{4}"), Some(true));
        assert_eq!(matches(r"\0400", " 0"), Some(true));
        assert_eq!(matches(r"\cz", "\u{1a}"), Some(true));
        assert_eq!(matches("^*", ""), Some(true));
        assert_eq!(matches("}", "}"), None);
        assert_eq!(matches("a**", "a"), None);
        assert_eq!(matches(r"\b*", ""), None);
        assert_eq!(matches(r"(?<=K??)a", "a"), None);
    }
}
