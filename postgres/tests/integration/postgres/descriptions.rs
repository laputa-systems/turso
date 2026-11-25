//! `pg_description` as a seeded table: a database has the built-in comments
//! before anything is stored, and the first statement that writes a comment
//! stores the table.

use crate::common::TempDatabase;
use turso_core::{NamespaceId, StepResult};
use turso_pg::Connection;

/// The comment that every database has.
const BUILTIN_COMMENT: &str = "standard public schema";

/// The first column of each row of `sql`, as text.
fn texts(conn: &Connection, sql: &str) -> Vec<String> {
    let mut rows = conn.query(sql).unwrap().unwrap();
    let mut values = Vec::new();
    while let StepResult::Row = rows.step().unwrap() {
        values.push(rows.row().unwrap().get_value(0).to_string());
    }
    values
}

/// The comments of the database, in the order of their text.
fn comments(conn: &Connection) -> Vec<String> {
    texts(
        conn,
        "SELECT description FROM pg_description ORDER BY description",
    )
}

/// Whether `pg_description` is a stored table in the schema that `conn`
/// has. It reads the schema and runs no statement, so that it does not
/// change what `conn` sees.
fn is_stored(conn: &Connection) -> bool {
    conn.inner()
        .current_schema()
        .get_btree_table(NamespaceId::ROOT, "pg_description")
        .is_some()
}

#[turso_macros::test(mvcc)]
fn new_database_has_the_builtin_comment_and_stores_nothing(db: TempDatabase) {
    let conn = db.connect_postgres();
    assert!(!is_stored(&conn));
    assert_eq!(comments(&conn), [BUILTIN_COMMENT]);
    assert_eq!(
        texts(&conn, "SELECT obj_description(2200, 'pg_namespace')"),
        [BUILTIN_COMMENT]
    );
    // A statement that reads, or that changes other objects, stores nothing.
    conn.execute("CREATE TABLE items (id int)").unwrap();
    conn.execute("CREATE SCHEMA app").unwrap();
    conn.execute("DROP SCHEMA app").unwrap();
    conn.execute("DROP TABLE items").unwrap();
    assert!(!is_stored(&conn));
    assert_eq!(comments(&conn), [BUILTIN_COMMENT]);
}

#[turso_macros::test(mvcc)]
fn first_comment_stores_the_table_with_the_builtin_comment(db: TempDatabase) {
    let conn = db.connect_postgres();
    let other = db.connect_postgres();
    conn.execute("CREATE TABLE items (id int)").unwrap();
    assert_eq!(comments(&other), [BUILTIN_COMMENT]);

    conn.execute("COMMENT ON TABLE items IS 'the items'")
        .unwrap();
    assert!(is_stored(&conn));
    assert_eq!(comments(&conn), [BUILTIN_COMMENT, "the items"]);
    // Another connection reads its schema again and reads the stored table.
    assert_eq!(comments(&other), [BUILTIN_COMMENT, "the items"]);

    // The next comment writes to the stored table.
    other
        .execute("COMMENT ON COLUMN items.id IS 'the key'")
        .unwrap();
    assert_eq!(comments(&conn), [BUILTIN_COMMENT, "the items", "the key"]);

    drop(other);
    drop(conn);
    let db = db.reopen();
    let conn = db.connect_postgres();
    assert!(is_stored(&conn));
    assert_eq!(comments(&conn), [BUILTIN_COMMENT, "the items", "the key"]);
}

#[turso_macros::test(mvcc)]
fn rollback_of_the_first_comment_stores_nothing(db: TempDatabase) {
    let conn = db.connect_postgres();
    conn.execute("CREATE TABLE items (id int)").unwrap();

    conn.execute("BEGIN").unwrap();
    conn.execute("COMMENT ON TABLE items IS 'the items'")
        .unwrap();
    assert_eq!(comments(&conn), [BUILTIN_COMMENT, "the items"]);
    conn.execute("ROLLBACK").unwrap();
    assert!(!is_stored(&conn));
    assert_eq!(comments(&conn), [BUILTIN_COMMENT]);

    // A later comment stores the table.
    conn.execute("COMMENT ON TABLE items IS 'again'").unwrap();
    assert!(is_stored(&conn));
    assert_eq!(comments(&conn), ["again", BUILTIN_COMMENT]);
}

#[turso_macros::test(mvcc)]
fn comment_on_the_public_schema_replaces_the_builtin_comment(db: TempDatabase) {
    let conn = db.connect_postgres();
    conn.execute("COMMENT ON SCHEMA public IS 'mine'").unwrap();
    assert!(is_stored(&conn));
    assert_eq!(comments(&conn), ["mine"]);
    conn.execute("COMMENT ON SCHEMA public IS NULL").unwrap();
    assert_eq!(comments(&conn), Vec::<String>::new());
}
