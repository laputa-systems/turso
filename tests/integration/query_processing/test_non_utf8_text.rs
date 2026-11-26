//! Regression test for issue #5164: TEXT values that another program stored
//! with bytes that are not UTF-8. As in SQLite, they are TEXT that keeps
//! these bytes, and they compare and sort by their bytes.

use crate::common::try_limbo_exec_rows;
use asserting::prelude::*;

#[turso_macros::test(init_sql = "CREATE TABLE t(val TEXT);")]
fn test_non_utf8_text_keeps_its_bytes(tmp_db: crate::common::TempDatabase) -> anyhow::Result<()> {
    {
        let sqlite_conn = rusqlite::Connection::open(&tmp_db.path)?;
        sqlite_conn.execute("INSERT INTO t VALUES(CAST(X'FF' AS TEXT)), ('valid')", [])?;
    }

    let limbo_conn = tmp_db.connect_limbo();

    for (query, expected) in [
        (
            "SELECT hex(val), typeof(val), length(val) FROM t ORDER BY rowid",
            vec![row!["FF", "text", 1], row!["76616C6964", "text", 5]],
        ),
        (
            "SELECT hex(val) FROM t ORDER BY val COLLATE NOCASE",
            vec![row!["76616C6964"], row!["FF"]],
        ),
    ] {
        let rows = try_limbo_exec_rows(&tmp_db, &limbo_conn, query)?;
        assert_that!(rows).described_as(query).is_equal_to(expected);
    }

    let rows = try_limbo_exec_rows(&tmp_db, &limbo_conn, "SELECT CAST(X'FF' AS BLOB)")?;
    assert_that!(rows)
        .described_as("the same bytes remain valid when explicitly typed as a blob")
        .is_equal_to(vec![row![vec![0xFF_u8]]]);

    Ok(())
}
