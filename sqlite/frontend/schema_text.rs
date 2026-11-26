//! The text SQLite stores in `sqlite_schema` for the statement that
//! creates an object: a fixed `CREATE <kind>` prefix followed by the
//! statement as written, from the object's unqualified name onward.

use turso_core::ast::{self, NameToken};
use turso_core::dialect::RenameQuoting;
use turso_parser::lexer::Lexer;
use turso_parser::token::TokenType;

struct Located {
    kind: TokenType,
    start: usize,
    end: usize,
}

fn tokens(sql: &str) -> Option<Vec<Located>> {
    let base = sql.as_ptr() as usize;
    let mut located = Vec::new();
    let mut offset = 0;
    'lex: while offset < sql.len() {
        for token in Lexer::new(&sql.as_bytes()[offset..]) {
            let token = token.ok()?;
            if token.token_type == TokenType::TK_NONE {
                continue;
            }
            let Some(start) = (token.value.as_ptr() as usize).checked_sub(base) else { continue };
            if start + token.value.len() > sql.len() {
                continue;
            }
            // A bracket-quoted identifier runs to the next `]`, as the
            // parser reads it; empty brackets are an array type suffix.
            if token.token_type == TokenType::TK_LBRACKET && sql.as_bytes().get(start + 1) != Some(&b']') {
                let end = start + 1 + sql[start + 1..].find(']')? + 1;
                located.push(Located { kind: TokenType::TK_ID, start, end });
                offset = end;
                continue 'lex;
            }
            located.push(Located {
                kind: token.token_type,
                start,
                end: start + token.value.len(),
            });
        }
        break;
    }
    Some(located)
}

fn is_name(kind: TokenType) -> bool {
    !matches!(
        kind,
        TokenType::TK_LP
            | TokenType::TK_RP
            | TokenType::TK_SEMI
            | TokenType::TK_DOT
            | TokenType::TK_COMMA
            | TokenType::TK_EOF
    )
}

/// The index of the object's unqualified name token after `CREATE [TEMP]
/// [UNIQUE] <keyword> [IF NOT EXISTS] [schema.]`.
fn name_index(tokens: &[Located], keyword: TokenType) -> Option<usize> {
    let mut index = 0;
    if tokens.get(index)?.kind != TokenType::TK_CREATE {
        return None;
    }
    index += 1;
    while matches!(tokens.get(index)?.kind, TokenType::TK_TEMP | TokenType::TK_UNIQUE) {
        index += 1;
    }
    if tokens.get(index)?.kind != keyword {
        return None;
    }
    index += 1;
    if tokens.get(index)?.kind == TokenType::TK_IF
        && tokens.get(index + 1)?.kind == TokenType::TK_NOT
        && tokens.get(index + 2)?.kind == TokenType::TK_EXISTS
    {
        index += 3;
    }
    if !is_name(tokens.get(index)?.kind) {
        return None;
    }
    if tokens.get(index + 1).is_some_and(|token| token.kind == TokenType::TK_DOT) {
        index += 2;
        if !is_name(tokens.get(index)?.kind) {
            return None;
        }
    }
    Some(index)
}

/// Where SQLite ends the stored text of a statement whose last parsed token
/// is `last`: before a terminating semicolon, or at the end of the input.
fn lookahead_end(sql: &str, tokens: &[Located]) -> usize {
    match tokens.last() {
        Some(token) if token.kind == TokenType::TK_SEMI => token.start,
        _ => sql.len(),
    }
}

pub fn table_text(sql: &str) -> Option<String> {
    let tokens = tokens(sql)?;
    let name = name_index(&tokens, TokenType::TK_TABLE)?;
    let body = &tokens[name..];
    let semi = body.last().is_some_and(|token| token.kind == TokenType::TK_SEMI);
    let last = if semi { body.len().checked_sub(2)? } else { body.len().checked_sub(1)? };
    let end = if body[last].kind == TokenType::TK_RP {
        body[last].end
    } else {
        lookahead_end(sql, &tokens)
    };
    Some(format!("CREATE TABLE {}", &sql[body[0].start..end]))
}

pub fn index_text(sql: &str) -> Option<String> {
    let tokens = tokens(sql)?;
    let name = name_index(&tokens, TokenType::TK_INDEX)?;
    let unique = tokens[..name].iter().any(|token| token.kind == TokenType::TK_UNIQUE);
    let end = lookahead_end(sql, &tokens);
    let prefix = if unique { "CREATE UNIQUE INDEX" } else { "CREATE INDEX" };
    Some(format!("{prefix} {}", &sql[tokens[name].start..end]))
}

pub fn trigger_text(sql: &str) -> Option<String> {
    let tokens = tokens(sql)?;
    let name = name_index(&tokens, TokenType::TK_TRIGGER)?;
    let end = tokens.iter().rev().find(|token| token.kind == TokenType::TK_END)?;
    Some(format!("CREATE TRIGGER {}", &sql[tokens[name].start..end.end]))
}

pub fn view_text(sql: &str) -> Option<String> {
    let tokens = tokens(sql)?;
    let name = name_index(&tokens, TokenType::TK_VIEW)?;
    let end = lookahead_end(sql, &tokens);
    let text = sql[tokens[name].start..end].trim_end_matches(is_sqlite_space);
    Some(format!("CREATE VIEW {text}"))
}

/// SQLite's `sqlite3Isspace`.
fn is_sqlite_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

pub fn added_column_text(sql: &str) -> Option<String> {
    let tokens = tokens(sql)?;
    let mut index = 0;
    if tokens.get(index)?.kind != TokenType::TK_ALTER || tokens.get(index + 1)?.kind != TokenType::TK_TABLE {
        return None;
    }
    index += 2;
    if !is_name(tokens.get(index)?.kind) {
        return None;
    }
    if tokens.get(index + 1).is_some_and(|token| token.kind == TokenType::TK_DOT) {
        index += 2;
    }
    index += 1;
    if tokens.get(index)?.kind != TokenType::TK_ADD {
        return None;
    }
    index += 1;
    if tokens.get(index)?.kind == TokenType::TK_COLUMNKW {
        index += 1;
    }
    let start = tokens.get(index)?.start;
    Some(sql[start..].trim_end_matches(|c: char| c == ';' || c.is_ascii_whitespace() || c == '\x0b' || c == '\x0c').to_string())
}

pub fn table_sql_with_added_column(sql: &str, column: &str) -> Option<String> {
    let tokens = tokens(sql)?;
    let name = name_index(&tokens, TokenType::TK_TABLE)?;
    let open = tokens[name..].iter().position(|token| token.kind == TokenType::TK_LP)? + name;
    let mut depth = 0usize;
    let mut offset = None;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            TokenType::TK_LP => depth += 1,
            TokenType::TK_RP => {
                depth -= 1;
                if depth == 0 {
                    offset = Some(token.start);
                    break;
                }
            }
            TokenType::TK_COMMA
                if depth == 1
                    && tokens.get(index + 1).is_some_and(|next| {
                        matches!(
                            next.kind,
                            TokenType::TK_CONSTRAINT
                                | TokenType::TK_PRIMARY
                                | TokenType::TK_UNIQUE
                                | TokenType::TK_CHECK
                                | TokenType::TK_FOREIGN
                        )
                    }) =>
            {
                offset = Some(token.start);
                break;
            }
            _ => {}
        }
    }
    let offset = offset?;
    Some(format!("{}, {}{}", &sql[..offset], column, &sql[offset..]))
}

/// SQLite's `renameEditSql`: `sql` with the text of each token that a
/// renamed name of `rewritten` replaces written as that name.
pub fn renamed_text(sql: &str, rewritten: &ast::Stmt, quoting: RenameQuoting) -> Option<String> {
    let mut replacements = Vec::new();
    ast::fmt::for_each_name(rewritten, &mut |name| {
        if let NameToken::Replaces(span) = name.token() {
            replacements.push((span, name.as_str().to_string()));
        }
    })
    .ok()?;
    replacements.sort_by_key(|(span, _)| span.start);
    replacements.dedup_by_key(|(span, _)| span.start);
    let bytes = sql.as_bytes();
    let mut text = String::with_capacity(sql.len());
    let mut copied = 0;
    for (span, name) in replacements {
        if span.start < copied || span.end > sql.len() || !sql.is_char_boundary(span.start) || !sql.is_char_boundary(span.end) {
            return None;
        }
        text.push_str(&sql[copied..span.start]);
        if quoting == RenameQuoting::WhereQuoted && is_id_char(bytes[span.start]) {
            text.push_str(&name);
        } else {
            text.push('"');
            text.push_str(&name.replace('"', "\"\""));
            text.push('"');
            // SQLite keeps a space between the new name and a `"` that
            // follows the replaced token.
            if bytes.get(span.end) == Some(&b'"') {
                text.push(' ');
            }
        }
        copied = span.end;
    }
    text.push_str(&sql[copied..]);
    Some(text)
}

/// SQLite's `sqlite3IsIdChar`.
fn is_id_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_text_keeps_the_statement_from_the_name() {
        assert_eq!(table_text("create table if not exists main.t(a INTEGER,  b);").as_deref(), Some("CREATE TABLE t(a INTEGER,  b)"));
        assert_eq!(table_text("CREATE TABLE \"x y\" (a)").as_deref(), Some("CREATE TABLE \"x y\" (a)"));
        assert_eq!(table_text("CREATE TABLE s(k PRIMARY KEY) WITHOUT ROWID ;").as_deref(), Some("CREATE TABLE s(k PRIMARY KEY) WITHOUT ROWID "));
        assert_eq!(table_text("CREATE TEMP TABLE t(a) STRICT").as_deref(), Some("CREATE TABLE t(a) STRICT"));
    }

    fn renamed_columns(sql: &str, new: &str, quoting: RenameQuoting) -> Option<String> {
        let Some(ast::Cmd::Stmt(mut stmt)) = turso_parser::parser::Parser::new(sql.as_bytes()).next_cmd().unwrap() else {
            panic!("not a statement");
        };
        let ast::Stmt::CreateTable { body: ast::CreateTableBody::ColumnsAndConstraints { columns, .. }, .. } = &mut stmt else {
            panic!("not CREATE TABLE");
        };
        for column in columns {
            column.col_name = column.col_name.renamed(new);
        }
        renamed_text(sql, &stmt, quoting)
    }

    #[test]
    fn renamed_tokens_are_quoted_as_sqlite_quotes_them() {
        assert_eq!(
            renamed_columns("CREATE TABLE t(a, \"b\" INT, [c], d\"x\")", "n", RenameQuoting::WhereQuoted).as_deref(),
            Some("CREATE TABLE t(n, \"n\" INT, \"n\", n\"x\")")
        );
        assert_eq!(
            renamed_columns("CREATE TABLE t(a\"T\", b)", "x\"y", RenameQuoting::Always).as_deref(),
            Some("CREATE TABLE t(\"x\"\"y\" \"T\", \"x\"\"y\")")
        );
    }

    #[test]
    fn bracket_quoted_names_end_at_the_bracket() {
        assert_eq!(
            table_text("CREATE TABLE \"u\"  (id INT, \"x\"\"y\" INT, CHECK ([x\"y] <> 1));").as_deref(),
            Some("CREATE TABLE \"u\"  (id INT, \"x\"\"y\" INT, CHECK ([x\"y] <> 1))")
        );
    }

    #[test]
    fn added_columns_go_before_table_constraints() {
        assert_eq!(added_column_text("ALTER TABLE t ADD COLUMN e TEXT DEFAULT 'x' ;").as_deref(), Some("e TEXT DEFAULT 'x'"));
        assert_eq!(table_sql_with_added_column("CREATE TABLE t(a, b)", "c").as_deref(), Some("CREATE TABLE t(a, b, c)"));
        assert_eq!(
            table_sql_with_added_column("CREATE TABLE t(a INT, b CHECK(b > (1)), PRIMARY KEY(a))", "c").as_deref(),
            Some("CREATE TABLE t(a INT, b CHECK(b > (1)), c, PRIMARY KEY(a))")
        );
    }

    #[test]
    fn trigger_text_ends_at_end() {
        assert_eq!(
            trigger_text("create temp trigger if not exists main.tr after insert on t begin select 1; end;").as_deref(),
            Some("CREATE TRIGGER tr after insert on t begin select 1; end")
        );
    }

    #[test]
    fn view_text_keeps_comments_and_drops_trailing_space() {
        assert_eq!(
            view_text("create view if not exists main.v(x) as select a  from t -- hi\n ;").as_deref(),
            Some("CREATE VIEW v(x) as select a  from t -- hi")
        );
        assert_eq!(view_text("CREATE TEMP VIEW w AS SELECT 1 /* c */  ").as_deref(), Some("CREATE VIEW w AS SELECT 1 /* c */"));
    }

    #[test]
    fn index_text_keeps_the_statement_from_the_name() {
        assert_eq!(index_text("create unique index if not exists i on t (a, b);").as_deref(), Some("CREATE UNIQUE INDEX i on t (a, b)"));
        assert_eq!(index_text("CREATE INDEX main.i ON t(a) WHERE a > 0").as_deref(), Some("CREATE INDEX i ON t(a) WHERE a > 0"));
    }
}
