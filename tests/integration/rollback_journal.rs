use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use turso_core::io::{FileId, FileSyncType};
use turso_core::{
    Buffer, Clock, Completion, ConnectOpts, Connection, Database, DatabaseOpts, File,
    MonotonicInstant, OpenFlags, OpenOptions, PlatformIO, SessionRole, WallClockInstant, IO,
};
use turso_sqlite::SqliteDialect;

const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

const PYTHON_HELPERS: &str = r#"
import os, shutil, sqlite3, sys

def fmt(v):
    if v is None:
        return "n"
    if isinstance(v, int):
        return "i:%d" % v
    if isinstance(v, str):
        return "t:" + v
    if isinstance(v, bytes):
        return "b:" + v.hex()
    return "r:%r" % v

def dump(con):
    out = []
    tables = con.execute(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
    ).fetchall()
    for (name,) in tables:
        out.append("table " + name)
        for row in con.execute('SELECT * FROM "%s" ORDER BY 1' % name):
            out.append("|".join(fmt(v) for v in row))
    return "\n".join(out)

def populate(con):
    con.execute("PRAGMA page_size=4096")
    con.execute("PRAGMA journal_mode=TRUNCATE")
    con.execute("CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE)")
    con.execute("CREATE TABLE visits(id INTEGER PRIMARY KEY, user_id INTEGER, url TEXT, visit_count INTEGER)")
    con.execute("CREATE INDEX visits_url ON visits(url)")
    con.execute("CREATE INDEX visits_user ON visits(user_id, visit_count)")
    con.execute("BEGIN")
    for i in range(300):
        con.execute("INSERT INTO users VALUES(?, ?, ?)", (i, "user%d" % i, "u%d@example.com" % i))
    for i in range(600):
        con.execute(
            "INSERT INTO visits VALUES(?, ?, ?, ?)",
            (i, i % 300, "https://example.com/%d/%s" % (i, "x" * (i % 50)), i * 7),
        )
    con.execute("COMMIT")
"#;

const PY_CREATE_LEGACY: &str = r#"
con = sqlite3.connect(sys.argv[1], isolation_level=None)
populate(con)
con.close()
"#;

const PY_CHECK_AND_DUMP: &str = r#"
con = sqlite3.connect(sys.argv[1], isolation_level=None)
print(con.execute("PRAGMA integrity_check").fetchone()[0])
print(con.execute("PRAGMA journal_mode").fetchone()[0])
print(dump(con))
con.close()
"#;

const PY_CRASH_WITH_HOT_JOURNAL: &str = r#"
db = sys.argv[1]
crash_dir = sys.argv[2]
con = sqlite3.connect(db, isolation_level=None)
populate(con)
con.close()
pid = os.fork()
if pid == 0:
    con = sqlite3.connect(db, isolation_level=None)
    con.execute("PRAGMA cache_size=2")
    con.execute("PRAGMA locking_mode=EXCLUSIVE")
    con.execute("PRAGMA journal_mode=TRUNCATE")
    con.execute("BEGIN")
    con.execute("UPDATE visits SET url = url || '/changed', visit_count = visit_count + 1")
    for i in range(600, 1500):
        con.execute("INSERT INTO visits VALUES(?, ?, ?, ?)", (i, i % 300, "https://crash.example/%d" % i, i))
    con.execute("DELETE FROM users WHERE id % 3 = 0")
    shutil.copy(db, os.path.join(crash_dir, "crash.db"))
    shutil.copy(db + "-journal", os.path.join(crash_dir, "crash.db-journal"))
    os._exit(0)
os.waitpid(pid, 0)
"#;

fn python(script: &str, args: &[&Path]) -> String {
    let output = Command::new("python3")
        .arg("-I")
        .arg("-c")
        .arg(format!("{PYTHON_HELPERS}\n{script}"))
        .args(args)
        .output()
        .expect("python3 must be installed to compare with SQLite");
    assert!(
        output.status.success(),
        "python3 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

struct SqliteView {
    integrity: String,
    journal_mode: String,
    dump: String,
}

fn sqlite_view(db_path: &Path) -> SqliteView {
    let output = python(PY_CHECK_AND_DUMP, &[db_path]);
    let mut lines = output.splitn(3, '\n');
    SqliteView {
        integrity: lines.next().unwrap().to_string(),
        journal_mode: lines.next().unwrap().to_string(),
        dump: lines.next().unwrap_or("").trim_end_matches('\n').to_string(),
    }
}

fn rollback_journal_options() -> OpenOptions {
    OpenOptions::new(Arc::new(SqliteDialect))
        .flags(OpenFlags::Create)
        .db_opts(DatabaseOpts::new().with_rollback_journal(true))
}

fn open_with_io(db_path: &Path, io: Arc<dyn IO>) -> (Arc<Database>, Arc<Connection>) {
    let db = Database::open(io, db_path.to_str().unwrap(), rollback_journal_options()).unwrap();
    let conn = db.connect(ConnectOpts::new(SessionRole::Superuser)).unwrap();
    (db, conn)
}

fn open(db_path: &Path) -> (Arc<Database>, Arc<Connection>) {
    open_with_io(db_path, Arc::new(PlatformIO::new().unwrap()))
}

fn close(db: Arc<Database>, conn: Arc<Connection>) {
    conn.close().unwrap();
    drop(conn);
    drop(db);
}

fn rows(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut rows = Vec::new();
    stmt.run_with_row_callback(|row| {
        let values: Vec<String> = row
            .get_values()
            .map(|value| match value {
                turso_core::Value::Null => "n".to_string(),
                turso_core::Value::Numeric(turso_core::Numeric::Integer(v)) => format!("i:{v}"),
                turso_core::Value::Numeric(turso_core::Numeric::Float(v)) => {
                    format!("r:{}", f64::from(*v))
                }
                turso_core::Value::Text(v) => format!("t:{}", v.as_str()),
                turso_core::Value::Blob(v) => {
                    let hex: String = v.iter().map(|byte| format!("{byte:02x}")).collect();
                    format!("b:{hex}")
                }
            })
            .collect();
        rows.push(values.join("|"));
        Ok(())
    })
    .unwrap();
    rows
}

fn single(conn: &Arc<Connection>, sql: &str) -> String {
    let rows = rows(conn, sql);
    assert_eq!(rows.len(), 1, "{sql} returned {rows:?}");
    rows.into_iter().next().unwrap()
}

fn turso_dump(conn: &Arc<Connection>) -> String {
    let mut out = Vec::new();
    let tables = rows(
        conn,
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    );
    for table in tables {
        let name = table.strip_prefix("t:").unwrap().to_string();
        out.push(format!("table {name}"));
        out.extend(rows(conn, &format!("SELECT * FROM \"{name}\" ORDER BY 1")));
    }
    out.join("\n")
}

fn files_in(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

fn assert_same_files(after: &BTreeMap<String, Vec<u8>>, before: &BTreeMap<String, Vec<u8>>) {
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>(),
        "the set of files changed"
    );
    for (name, bytes) in after {
        assert!(bytes == &before[name], "{name} changed");
    }
}

fn sidecar(db_path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", db_path.display()))
}

fn header_u32(db_path: &Path, offset: usize) -> u32 {
    let bytes = std::fs::read(db_path).unwrap();
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn header_versions(db_path: &Path) -> (u8, u8) {
    let bytes = std::fs::read(db_path).unwrap();
    (bytes[18], bytes[19])
}

#[test]
fn legacy_database_is_read_without_changing_any_file() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("history.db");
    python(PY_CREATE_LEGACY, &[&db_path]);
    let before = files_in(dir.path());
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        ["history.db", "history.db-journal"]
    );
    assert!(before["history.db-journal"].is_empty());
    let expected = sqlite_view(&db_path);

    let (db, conn) = open(&db_path);
    assert_eq!(turso_dump(&conn), expected.dump);
    assert_eq!(single(&conn, "PRAGMA integrity_check"), "t:ok");
    assert_eq!(single(&conn, "PRAGMA journal_mode"), "t:delete");
    close(db, conn);

    assert_same_files(&files_in(dir.path()), &before);
}

#[test]
fn new_database_is_a_rollback_journal_database_that_sqlite_reads() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("cookies.db");
    let (db, conn) = open(&db_path);
    assert_eq!(single(&conn, "PRAGMA journal_mode=TRUNCATE"), "t:truncate");
    conn.execute("CREATE TABLE cookies(id INTEGER PRIMARY KEY, host TEXT NOT NULL, name TEXT, value TEXT, UNIQUE(host, name))")
        .unwrap();
    conn.execute("CREATE INDEX cookies_value ON cookies(value)").unwrap();
    conn.execute("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT)").unwrap();
    conn.execute("BEGIN").unwrap();
    for i in 0..400 {
        conn.execute(format!(
            "INSERT INTO cookies VALUES({i}, 'host{}.example', 'name{i}', '{}')",
            i % 37,
            "v".repeat(i % 90)
        ))
        .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    conn.execute("INSERT INTO meta VALUES('version', '1')").unwrap();

    let before_rollback = std::fs::read(&db_path).unwrap();
    conn.execute("BEGIN").unwrap();
    conn.execute("DELETE FROM cookies WHERE id < 200").unwrap();
    conn.execute("UPDATE meta SET value = 'rolled back'").unwrap();
    conn.execute("ROLLBACK").unwrap();
    assert_eq!(std::fs::read(&db_path).unwrap(), before_rollback);

    conn.execute("BEGIN").unwrap();
    conn.execute("UPDATE cookies SET value = 'updated' WHERE id % 5 = 0")
        .unwrap();
    let duplicate = conn.execute("INSERT INTO cookies VALUES(1000, 'host1.example', 'name1', 'dup')");
    assert!(duplicate.is_err(), "the duplicate (host, name) must fail");
    conn.execute("INSERT INTO cookies VALUES(1001, 'other.example', 'fresh', 'kept')")
        .unwrap();
    conn.execute("SAVEPOINT partial").unwrap();
    conn.execute("DELETE FROM cookies WHERE id > 300").unwrap();
    conn.execute("ROLLBACK TO partial").unwrap();
    conn.execute("RELEASE partial").unwrap();
    conn.execute("COMMIT").unwrap();

    conn.execute("DELETE FROM cookies WHERE id % 7 = 3").unwrap();
    conn.execute("UPDATE meta SET value = '2' WHERE key = 'version'")
        .unwrap();
    let expected = turso_dump(&conn);
    assert!(expected.contains("i:1001|t:other.example|t:fresh|t:kept"));
    assert!(expected.contains("i:5|t:host5.example|t:name5|t:updated"));
    assert!(expected.contains("i:301|"));
    close(db, conn);

    let sqlite = sqlite_view(&db_path);
    assert_eq!(sqlite.integrity, "ok");
    assert_eq!(sqlite.dump, expected);
    assert_eq!(header_versions(&db_path), (1, 1));
    assert_eq!(header_u32(&db_path, 24), header_u32(&db_path, 92));
    assert_eq!(header_u32(&db_path, 96), 3_053_004);
    let journal = sidecar(&db_path, "-journal");
    assert_eq!(std::fs::metadata(&journal).unwrap().len(), 0);
    assert!(!sidecar(&db_path, "-wal").exists());
    assert!(!sidecar(&db_path, "-shm").exists());
}

#[test]
fn hot_journal_left_by_sqlite_is_rolled_back() {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("source.db");
    python(PY_CRASH_WITH_HOT_JOURNAL, &[&source, dir.path()]);
    let pre_transaction = sqlite_view(&source);
    assert_eq!(pre_transaction.integrity, "ok");

    let crash_db = dir.path().join("crash.db");
    let crash_journal = sidecar(&crash_db, "-journal");
    let journal = std::fs::read(&crash_journal).unwrap();
    assert_eq!(journal[..8], JOURNAL_MAGIC);
    let first_segment_records = u32::from_be_bytes(journal[8..12].try_into().unwrap()) as usize;
    assert!(first_segment_records > 0);
    assert!(
        512 + first_segment_records * (4096 + 8) < journal.len(),
        "the spilling transaction must have written more than one journal segment"
    );
    assert_ne!(
        std::fs::read(&crash_db).unwrap(),
        std::fs::read(&source).unwrap(),
        "SQLite must have spilled part of the transaction into the database file"
    );

    let copy = |name: &str| {
        let copy_dir = dir.path().join(name);
        std::fs::create_dir(&copy_dir).unwrap();
        let db = copy_dir.join("crash.db");
        std::fs::copy(&crash_db, &db).unwrap();
        std::fs::copy(&crash_journal, sidecar(&db, "-journal")).unwrap();
        db
    };
    let sqlite_copy = copy("sqlite");
    let turso_copy = copy("turso");

    let sqlite_recovered = sqlite_view(&sqlite_copy);
    assert_eq!(sqlite_recovered.dump, pre_transaction.dump);

    let (db, conn) = open(&turso_copy);
    assert_eq!(turso_dump(&conn), sqlite_recovered.dump);
    close(db, conn);
    assert!(!sidecar(&turso_copy, "-journal").exists());
    let after_turso = sqlite_view(&turso_copy);
    assert_eq!(after_turso.integrity, "ok");
    assert_eq!(after_turso.dump, pre_transaction.dump);
}

struct Snapshot {
    after_database_writes: usize,
    database_writes: usize,
    target: PathBuf,
    taken: bool,
}

struct SnapshotIo {
    inner: Arc<dyn IO>,
    db_path: String,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
}

impl SnapshotIo {
    fn new(db_path: &Path) -> Self {
        Self {
            inner: Arc::new(PlatformIO::new().unwrap()),
            db_path: db_path.to_str().unwrap().to_string(),
            snapshot: Arc::new(Mutex::new(None)),
        }
    }

    fn arm(&self, after_database_writes: usize, target: &Path) {
        *self.snapshot.lock().unwrap() = Some(Snapshot {
            after_database_writes,
            database_writes: 0,
            target: target.to_path_buf(),
            taken: false,
        });
    }

    fn taken(&self) -> bool {
        self.snapshot
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|snapshot| snapshot.taken)
    }
}

impl Clock for SnapshotIo {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        self.inner.current_time_monotonic()
    }

    fn current_time_wall_clock(&self) -> WallClockInstant {
        self.inner.current_time_wall_clock()
    }
}

impl IO for SnapshotIo {
    fn open_file(
        &self,
        path: &str,
        flags: OpenFlags,
        direct: bool,
    ) -> turso_core::Result<Arc<dyn File>> {
        let inner = self.inner.open_file(path, flags, direct)?;
        if path != self.db_path {
            return Ok(inner);
        }
        Ok(Arc::new(SnapshotFile {
            inner,
            db_path: self.db_path.clone(),
            snapshot: self.snapshot.clone(),
        }))
    }

    fn remove_file(&self, path: &str) -> turso_core::Result<()> {
        self.inner.remove_file(path)
    }

    fn step(&self) -> turso_core::Result<()> {
        self.inner.step()
    }

    fn file_id(&self, path: &str) -> turso_core::Result<FileId> {
        self.inner.file_id(path)
    }
}

struct SnapshotFile {
    inner: Arc<dyn File>,
    db_path: String,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
}

impl SnapshotFile {
    fn before_write(&self) {
        let mut snapshot = self.snapshot.lock().unwrap();
        let Some(snapshot) = snapshot.as_mut() else {
            return;
        };
        if !snapshot.taken && snapshot.database_writes == snapshot.after_database_writes {
            let db = snapshot.target.join("crash.db");
            std::fs::copy(&self.db_path, &db).unwrap();
            std::fs::copy(format!("{}-journal", self.db_path), sidecar(&db, "-journal")).unwrap();
            snapshot.taken = true;
        }
        snapshot.database_writes += 1;
    }
}

impl File for SnapshotFile {
    fn lock_file(&self, exclusive: bool) -> turso_core::Result<()> {
        self.inner.lock_file(exclusive)
    }

    fn unlock_file(&self) -> turso_core::Result<()> {
        self.inner.unlock_file()
    }

    fn pread(&self, pos: u64, c: Completion) -> turso_core::Result<Completion> {
        self.inner.pread(pos, c)
    }

    fn pwrite(&self, pos: u64, buffer: Arc<Buffer>, c: Completion) -> turso_core::Result<Completion> {
        self.before_write();
        self.inner.pwrite(pos, buffer, c)
    }

    fn pwritev(
        &self,
        pos: u64,
        buffers: Vec<Arc<Buffer>>,
        c: Completion,
    ) -> turso_core::Result<Completion> {
        self.before_write();
        self.inner.pwritev(pos, buffers, c)
    }

    fn sync(&self, c: Completion, sync_type: FileSyncType) -> turso_core::Result<Completion> {
        self.inner.sync(c, sync_type)
    }

    fn size(&self) -> turso_core::Result<u64> {
        self.inner.size()
    }

    fn truncate(&self, len: u64, c: Completion) -> turso_core::Result<Completion> {
        self.inner.truncate(len, c)
    }
}

#[test]
fn sqlite_rolls_back_a_commit_interrupted_after_the_journal_sync() {
    let cases = [
        ("FULL", 0usize),
        ("FULL", 3),
        ("NORMAL", 2),
        ("OFF", 0),
        ("OFF", 4),
    ];
    for (round, (synchronous, after_database_writes)) in cases.into_iter().enumerate() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("favicons.db");
        python(PY_CREATE_LEGACY, &[&db_path]);
        let pre_transaction = sqlite_view(&db_path);
        let original_bytes = std::fs::read(&db_path).unwrap();

        let io = Arc::new(SnapshotIo::new(&db_path));
        let (db, conn) = open_with_io(&db_path, io.clone());
        rows(&conn, "PRAGMA journal_mode=TRUNCATE");
        conn.execute(format!("PRAGMA synchronous={synchronous}")).unwrap();
        let snapshot_dir = dir.path().join(format!("snapshot-{round}"));
        std::fs::create_dir(&snapshot_dir).unwrap();
        conn.execute("BEGIN").unwrap();
        conn.execute("UPDATE visits SET url = url || '/turso', visit_count = visit_count * 2")
            .unwrap();
        for i in 600..900 {
            conn.execute(format!(
                "INSERT INTO visits VALUES({i}, {}, 'https://turso.example/{i}', {i})",
                i % 300
            ))
            .unwrap();
        }
        conn.execute("DELETE FROM users WHERE id % 4 = 1").unwrap();
        io.arm(after_database_writes, &snapshot_dir);
        conn.execute("COMMIT").unwrap();
        assert!(io.taken(), "{synchronous}: the commit wrote the database file");
        let committed = turso_dump(&conn);
        close(db, conn);

        let crash_db = snapshot_dir.join("crash.db");
        let journal = std::fs::read(sidecar(&crash_db, "-journal")).unwrap();
        assert_eq!(journal[..8], JOURNAL_MAGIC, "{synchronous}: journal header");
        let record_count = u32::from_be_bytes(journal[8..12].try_into().unwrap());
        if synchronous == "OFF" {
            assert_eq!(record_count, u32::MAX);
        } else {
            assert!(record_count > 0, "{synchronous}: records counted before the database write");
        }
        if after_database_writes > 0 {
            assert_ne!(std::fs::read(&crash_db).unwrap(), original_bytes);
        }

        let turso_copy_dir = dir.path().join(format!("turso-{round}"));
        std::fs::create_dir(&turso_copy_dir).unwrap();
        let turso_copy = turso_copy_dir.join("crash.db");
        std::fs::copy(&crash_db, &turso_copy).unwrap();
        std::fs::copy(sidecar(&crash_db, "-journal"), sidecar(&turso_copy, "-journal")).unwrap();

        let recovered = sqlite_view(&crash_db);
        assert_eq!(recovered.integrity, "ok", "{synchronous}");
        assert_eq!(recovered.dump, pre_transaction.dump, "{synchronous}");

        let (db, conn) = open(&turso_copy);
        assert_eq!(turso_dump(&conn), pre_transaction.dump, "{synchronous}");
        close(db, conn);

        let after_commit = sqlite_view(&db_path);
        assert_eq!(after_commit.integrity, "ok", "{synchronous}");
        assert_eq!(after_commit.dump, committed, "{synchronous}");
        assert_eq!(std::fs::metadata(sidecar(&db_path, "-journal")).unwrap().len(), 0);
    }
}

#[test]
fn journal_mode_converts_between_rollback_and_wal() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("web_data.db");
    python(PY_CREATE_LEGACY, &[&db_path]);

    let (db, conn) = open(&db_path);
    assert_eq!(single(&conn, "PRAGMA journal_mode"), "t:delete");
    let insert = conn.prepare("INSERT INTO users VALUES(1000, 'wal user', 'wal@example.com')");
    assert_eq!(single(&conn, "PRAGMA journal_mode=WAL"), "t:wal");
    assert_eq!(header_versions(&db_path), (2, 2));
    assert_eq!(single(&conn, "PRAGMA journal_mode"), "t:wal");
    insert.unwrap().run_ignore_rows().unwrap();
    conn.execute("UPDATE visits SET visit_count = -1 WHERE id < 10").unwrap();
    let in_wal = turso_dump(&conn);
    close(db, conn);

    let sqlite = sqlite_view(&db_path);
    assert_eq!(sqlite.integrity, "ok");
    assert_eq!(sqlite.journal_mode, "wal");
    assert_eq!(sqlite.dump, in_wal);

    let (db, conn) = open(&db_path);
    assert_eq!(single(&conn, "PRAGMA journal_mode"), "t:wal");
    conn.execute("INSERT INTO users VALUES(1001, 'pending', 'pending@example.com')")
        .unwrap();
    assert_eq!(single(&conn, "PRAGMA journal_mode=TRUNCATE"), "t:truncate");
    assert_eq!(header_versions(&db_path), (1, 1));
    assert!(!sidecar(&db_path, "-wal").exists());
    conn.execute("INSERT INTO users VALUES(1002, 'rollback user', 'rollback@example.com')")
        .unwrap();
    let in_rollback = turso_dump(&conn);
    assert!(in_rollback.contains("i:1001|t:pending"));
    close(db, conn);

    assert!(!sidecar(&db_path, "-wal").exists());
    assert_eq!(
        std::fs::metadata(sidecar(&db_path, "-journal")).unwrap().len(),
        0
    );
    let sqlite = sqlite_view(&db_path);
    assert_eq!(sqlite.integrity, "ok");
    assert_eq!(sqlite.journal_mode, "delete");
    assert_eq!(sqlite.dump, in_rollback);
}

#[test]
fn new_database_becomes_wal_when_asked_before_the_first_write() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("ukm.db");
    let (db, conn) = open(&db_path);
    assert_eq!(single(&conn, "PRAGMA journal_mode=WAL"), "t:wal");
    conn.execute("CREATE TABLE sources(id INTEGER PRIMARY KEY, url TEXT)")
        .unwrap();
    conn.execute("INSERT INTO sources VALUES(1, 'https://example.com')")
        .unwrap();
    let expected = turso_dump(&conn);
    close(db, conn);

    assert_eq!(header_versions(&db_path), (2, 2));
    assert!(!sidecar(&db_path, "-journal").exists());
    let sqlite = sqlite_view(&db_path);
    assert_eq!(sqlite.integrity, "ok");
    assert_eq!(sqlite.journal_mode, "wal");
    assert_eq!(sqlite.dump, expected);
}

#[test]
fn chromium_open_pragmas_succeed_and_leave_unwritten_files_unchanged() {
    let dir = TempDir::new().unwrap();
    let rollback_path = dir.path().join("rollback.db");
    python(PY_CREATE_LEGACY, &[&rollback_path]);
    let wal_path = dir.path().join("wal.db");
    python(
        "con = sqlite3.connect(sys.argv[1], isolation_level=None)\npopulate(con)\ncon.execute('PRAGMA journal_mode=WAL')\ncon.close()\n",
        &[&wal_path],
    );
    let before = files_in(dir.path());
    assert!(!before.contains_key("wal.db-wal"));

    for (db_path, journal_mode) in [(&rollback_path, "TRUNCATE"), (&wal_path, "WAL")] {
        let (db, conn) = open(db_path);
        assert_eq!(single(&conn, "PRAGMA locking_mode=NORMAL"), "t:normal");
        rows(&conn, "PRAGMA page_size=4096");
        rows(&conn, "PRAGMA synchronous=OFF");
        rows(&conn, "PRAGMA synchronous=NORMAL");
        assert_eq!(
            single(&conn, &format!("PRAGMA journal_mode={journal_mode}")),
            format!("t:{}", journal_mode.to_lowercase())
        );
        rows(&conn, "PRAGMA cache_size=128");
        rows(&conn, "PRAGMA mmap_size=268435456");
        rows(&conn, "PRAGMA mmap_size");
        assert_eq!(single(&conn, "SELECT count(*) FROM visits"), "i:600");
        close(db, conn);
    }

    assert_same_files(&files_in(dir.path()), &before);
}

#[test]
fn second_connection_sees_commits_and_waits_for_the_writer() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("shortcuts.db");
    python(PY_CREATE_LEGACY, &[&db_path]);
    let (db, writer) = open(&db_path);
    let reader = db.connect(ConnectOpts::new(SessionRole::Superuser)).unwrap();
    assert_eq!(single(&reader, "SELECT count(*) FROM users"), "i:300");
    writer.execute("BEGIN").unwrap();
    writer.execute("DELETE FROM users WHERE id >= 100").unwrap();
    assert!(
        reader.execute("SELECT count(*) FROM users").is_err(),
        "a reader must not read the database while a writer may change it"
    );
    writer.execute("COMMIT").unwrap();
    assert_eq!(single(&reader, "SELECT count(*) FROM users"), "i:100");
    reader.close().unwrap();
    drop(reader);
    close(db, writer);
    assert_eq!(sqlite_view(&db_path).integrity, "ok");
}
