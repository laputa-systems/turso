//! The SQLite frontend.
//!
//! [`SqliteDialect`] is the [`Dialect`] for SQLite: statements are SQLite
//! SQL and `sqlite_schema` rows are canonical SQLite text, parsed with the
//! SQLite parser.

mod schema_text;

use std::{ops::Range, sync::Arc};
use turso_core::{ast, Dialect, Result};
use turso_parser::{lexer::Lexer, parser::Parser, token::TokenType};

pub use turso_core::dialect::{SQLITE_VERSION, SQLITE_VERSION_NUMBER};

/// The SQLite dialect.
pub struct SqliteDialect;

impl Dialect for SqliteDialect {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn parse(&self, sql: &str) -> Result<(Option<ast::Cmd>, usize)> {
        self.parse_internal(sql)
    }

    fn parse_internal(&self, sql: &str) -> Result<(Option<ast::Cmd>, usize)> {
        let mut parser = Parser::new(sql.as_bytes());
        let cmd = parser.next_cmd()?;
        Ok((cmd, parser.offset()))
    }

    fn parse_internal_column_definition(&self, sql: &str) -> Result<ast::ColumnDefinition> {
        Ok(Parser::new(sql.as_bytes()).parse_column_definition(true)?)
    }

    fn parameter_marker_ranges(&self, sql: &str) -> Result<Vec<Range<usize>>> {
        let mut markers = Vec::new();
        for token in Lexer::new(sql.as_bytes()) {
            let token = token?;
            if token.token_type == TokenType::TK_VARIABLE {
                let start = token.value.as_ptr() as usize - sql.as_ptr() as usize;
                markers.push(start..start + token.value.len());
            }
        }
        Ok(markers)
    }

    fn get_internal_dialect(&self) -> Arc<dyn Dialect> {
        Arc::new(SqliteDialect)
    }

    fn format_table_sql(
        &self,
        input: &str,
        tbl_name: &ast::QualifiedName,
        body: &ast::CreateTableBody,
    ) -> Result<String> {
        match schema_text::table_text(input) {
            Some(text) => Ok(text),
            None => turso_core::dialect::format_table_sql(tbl_name, body),
        }
    }

    fn format_index_sql(&self, input: &str, canonical: String) -> Result<String> {
        Ok(schema_text::index_text(input).unwrap_or(canonical))
    }

    fn format_trigger_sql(&self, input: &str, canonical: String) -> Result<String> {
        Ok(schema_text::trigger_text(input).unwrap_or(canonical))
    }
}
