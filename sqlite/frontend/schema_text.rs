//! The text SQLite stores in `sqlite_schema` for the statement that
//! creates an object: a fixed `CREATE <kind>` prefix followed by the
//! statement as written, from the object's unqualified name onward.

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
    for token in Lexer::new(sql.as_bytes()) {
        let token = token.ok()?;
        if token.token_type == TokenType::TK_NONE {
            continue;
        }
        let Some(start) = (token.value.as_ptr() as usize).checked_sub(base) else { continue };
        if start + token.value.len() > sql.len() {
            continue;
        }
        located.push(Located {
            kind: token.token_type,
            start,
            end: start + token.value.len(),
        });
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

    #[test]
    fn trigger_text_ends_at_end() {
        assert_eq!(
            trigger_text("create temp trigger if not exists main.tr after insert on t begin select 1; end;").as_deref(),
            Some("CREATE TRIGGER tr after insert on t begin select 1; end")
        );
    }

    #[test]
    fn index_text_keeps_the_statement_from_the_name() {
        assert_eq!(index_text("create unique index if not exists i on t (a, b);").as_deref(), Some("CREATE UNIQUE INDEX i on t (a, b)"));
        assert_eq!(index_text("CREATE INDEX main.i ON t(a) WHERE a > 0").as_deref(), Some("CREATE INDEX i ON t(a) WHERE a > 0"));
    }
}
