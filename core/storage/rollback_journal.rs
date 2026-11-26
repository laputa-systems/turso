use crate::io::{CompletionGroup, File, FileSyncType};
use crate::storage::database::{DatabaseStorage, IOContext};
use crate::storage::journal_mode::JournalMode;
use crate::storage::pager::{PageRef, Pager};
use crate::storage::sqlite3_ondisk::begin_write_btree_page;
use crate::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::sync::{Arc, Mutex};
use crate::types::{IOCompletions, IOResultOr};
use crate::{io_yield_one, Buffer, Completion, CompletionError, IOResult, LimboError, OpenFlags};
use crate::{Result, SyncMode, IO};

pub(crate) const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

// SQLite uses 512-byte journal headers when the file system reports
// power-safe overwrite, which is the default of its unix VFS.
const JOURNAL_SECTOR_SIZE: u32 = 512;
const MAX_JOURNAL_SECTOR_SIZE: u32 = 0x10000;
const MAX_PAGE_SIZE: u32 = 65536;
const JOURNAL_HEADER_FIELDS_LEN: usize = 28;
const RECORD_COUNT_FIELDS_LEN: usize = 12;
const RECORD_COUNT_UNKNOWN: u32 = 0xffff_ffff;
const SUPER_JOURNAL_TRAILER_LEN: u64 = 16;

// The SQLite library of the browser that these database files come from.
pub(crate) const HEADER_SQLITE_VERSION_NUMBER: u32 = 3_053_004;

pub(crate) fn journal_path(db_path: &str) -> String {
    format!("{db_path}-journal")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RollbackJournalMode {
    Delete,
    Truncate,
    Persist,
}

impl RollbackJournalMode {
    pub(crate) fn from_journal_mode(mode: JournalMode) -> Option<Self> {
        match mode {
            JournalMode::Delete => Some(Self::Delete),
            JournalMode::Truncate => Some(Self::Truncate),
            JournalMode::Persist => Some(Self::Persist),
            JournalMode::Memory | JournalMode::Wal | JournalMode::Mvcc | JournalMode::Off => None,
        }
    }

    pub(crate) fn as_journal_mode(self) -> JournalMode {
        match self {
            Self::Delete => JournalMode::Delete,
            Self::Truncate => JournalMode::Truncate,
            Self::Persist => JournalMode::Persist,
        }
    }
}

pub(crate) struct RollbackJournalShared {
    path: String,
    mode: Mutex<RollbackJournalMode>,
    locks: Mutex<JournalLocks>,
    commit_generation: AtomicU64,
    needs_playback: AtomicBool,
    retired: AtomicBool,
}

#[derive(Default)]
struct JournalLocks {
    readers: usize,
    writer: bool,
}

impl RollbackJournalShared {
    pub(crate) fn new(db_path: &str) -> Self {
        Self {
            path: journal_path(db_path),
            mode: Mutex::new(RollbackJournalMode::Delete),
            locks: Mutex::new(JournalLocks::default()),
            commit_generation: AtomicU64::new(0),
            needs_playback: AtomicBool::new(false),
            retired: AtomicBool::new(false),
        }
    }

    pub(crate) fn mode(&self) -> RollbackJournalMode {
        *self.mode.lock()
    }

    pub(crate) fn set_mode(&self, mode: RollbackJournalMode) {
        *self.mode.lock() = mode;
    }

    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }

    fn play_back_failed_commit(&self, io: &dyn IO, db_file: &dyn DatabaseStorage) -> Result<()> {
        roll_back_journal(io, db_file, &self.path, self.mode())?;
        self.needs_playback.store(false, Ordering::Release);
        self.commit_generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

impl std::fmt::Debug for RollbackJournalShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RollbackJournalShared")
            .field("path", &self.path)
            .field("mode", &self.mode())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeldLock {
    None,
    Read,
    Write,
}

pub(crate) struct RollbackJournal {
    shared: Arc<RollbackJournalShared>,
    held: Mutex<HeldLock>,
    seen_generation: AtomicU64,
    commit: Mutex<Option<Box<JournalCommit>>>,
}

impl RollbackJournal {
    pub(crate) fn new(shared: Arc<RollbackJournalShared>) -> Self {
        let seen_generation = AtomicU64::new(shared.commit_generation.load(Ordering::Acquire));
        Self {
            shared,
            held: Mutex::new(HeldLock::None),
            seen_generation,
            commit: Mutex::new(None),
        }
    }

    pub(crate) fn shared(&self) -> &Arc<RollbackJournalShared> {
        &self.shared
    }

    pub(crate) fn begin_read_tx(&self, io: &dyn IO, db_file: &dyn DatabaseStorage) -> Result<bool> {
        if self.shared.retired.load(Ordering::Acquire) {
            return Err(LimboError::SchemaUpdated);
        }
        let mut held = self.held.lock();
        if *held != HeldLock::None {
            return Ok(false);
        }
        let play_back = {
            let mut locks = self.shared.locks.lock();
            if locks.writer {
                return Err(LimboError::Busy);
            }
            let play_back =
                locks.readers == 0 && self.shared.needs_playback.load(Ordering::Acquire);
            if play_back {
                locks.writer = true;
            } else {
                locks.readers += 1;
            }
            play_back
        };
        if play_back {
            let result = self.shared.play_back_failed_commit(io, db_file);
            let mut locks = self.shared.locks.lock();
            locks.writer = false;
            result?;
            locks.readers += 1;
        }
        *held = HeldLock::Read;
        let generation = self.shared.commit_generation.load(Ordering::Acquire);
        Ok(self.seen_generation.swap(generation, Ordering::AcqRel) != generation)
    }

    pub(crate) fn begin_write_tx(&self) -> Result<()> {
        let mut held = self.held.lock();
        match *held {
            HeldLock::Write => return Ok(()),
            HeldLock::None => {
                return Err(LimboError::InternalError(
                    "rollback journal write transaction needs a read transaction".into(),
                ))
            }
            HeldLock::Read => {}
        }
        let mut locks = self.shared.locks.lock();
        if locks.writer || locks.readers > 1 {
            return Err(LimboError::Busy);
        }
        locks.writer = true;
        *held = HeldLock::Write;
        Ok(())
    }

    pub(crate) fn end_write_tx(&self) {
        let mut held = self.held.lock();
        if *held == HeldLock::Write {
            self.shared.locks.lock().writer = false;
            *held = HeldLock::Read;
        }
    }

    pub(crate) fn end_read_tx(&self) {
        let mut held = self.held.lock();
        let mut locks = self.shared.locks.lock();
        if *held == HeldLock::Write {
            locks.writer = false;
        }
        if *held != HeldLock::None {
            locks.readers -= 1;
        }
        *held = HeldLock::None;
    }

    pub(crate) fn holds_read_lock(&self) -> bool {
        *self.held.lock() != HeldLock::None
    }

    pub(crate) fn holds_write_lock(&self) -> bool {
        *self.held.lock() == HeldLock::Write
    }

    pub(crate) fn commit(&self, pager: &Pager, sync_mode: SyncMode) -> IOResultOr<()> {
        let mut slot = self.commit.lock();
        if slot.is_none() {
            let dirty = pager.dirty_page_ids();
            if dirty.is_empty() {
                return Ok(IOResult::Done(()));
            }
            let header = crate::return_if_io!(pager.with_header_mut(|header| {
                let change_counter = header.change_counter.get().wrapping_add(1);
                header.change_counter = change_counter.into();
                header.version_valid_for = change_counter.into();
                header.version_number = HEADER_SQLITE_VERSION_NUMBER.into();
                header.database_size.get()
            }));
            *slot = Some(Box::new(JournalCommit::plan(
                pager,
                &self.shared,
                sync_mode,
                header,
            )?));
        }
        let commit = slot.as_mut().expect("commit planned above");
        crate::return_if_io!(commit.run(pager, &self.shared));
        *slot = None;
        drop(slot);
        pager.clear_dirty_page_set();
        let generation = self.shared.commit_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.seen_generation.store(generation, Ordering::Release);
        Ok(IOResult::Done(()))
    }

    pub(crate) fn abort_commit(&self, io: &dyn IO, db_file: &dyn DatabaseStorage) -> Result<()> {
        let Some(commit) = self.commit.lock().take() else {
            return Ok(());
        };
        if self.shared.needs_playback.load(Ordering::Acquire) {
            return self.shared.play_back_failed_commit(io, db_file);
        }
        if let Some(journal) = commit.journal {
            drop(journal);
            finalize_journal_blocking(io, &self.shared.path, self.shared.mode())?;
        }
        Ok(())
    }
}

impl Drop for RollbackJournal {
    fn drop(&mut self) {
        self.end_read_tx();
    }
}

impl std::fmt::Debug for RollbackJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RollbackJournal")
            .field("shared", &self.shared)
            .field("held", &*self.held.lock())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CommitStep {
    ReadOriginals,
    OpenJournal,
    WriteJournal,
    SyncJournalRecords,
    WriteRecordCount,
    SyncJournalHeader,
    WriteDatabase,
    SyncDatabase,
    FinalizeJournal,
    SyncFinalizedJournal,
    Done,
}

struct JournalCommit {
    step: CommitStep,
    pending: Option<Completion>,
    mode: RollbackJournalMode,
    sync_mode: SyncMode,
    sync_type: FileSyncType,
    page_size: usize,
    original_pages: u32,
    nonce: u32,
    journaled: Vec<(u32, Arc<Buffer>)>,
    dirty: Vec<PageRef>,
    journal: Option<Arc<dyn File>>,
}

impl JournalCommit {
    fn plan(
        pager: &Pager,
        shared: &RollbackJournalShared,
        sync_mode: SyncMode,
        new_database_pages: u32,
    ) -> Result<Self> {
        let page_size = pager.get_page_size_unchecked().get() as usize;
        let file_size = pager.db_file.size()?;
        let original_pages = u32::try_from(file_size.div_ceil(page_size as u64))
            .map_err(|_| LimboError::Corrupt("database file is too large".into()))?;
        let mut dirty = Vec::new();
        let mut journaled = Vec::new();
        for page_id in pager.dirty_page_ids() {
            let page = pager.cache_get(page_id as usize)?.ok_or_else(|| {
                LimboError::InternalError(format!(
                    "dirty page {page_id} is not in the page cache at commit"
                ))
            })?;
            if !page.is_loaded() {
                return Err(LimboError::InternalError(format!(
                    "dirty page {page_id} is not loaded at commit"
                )));
            }
            if page_id > new_database_pages {
                return Err(LimboError::InternalError(format!(
                    "dirty page {page_id} is past the end of the database ({new_database_pages} pages)"
                )));
            }
            if page_id <= original_pages {
                journaled.push((page_id, Arc::new(Buffer::new_temporary(page_size))));
            }
            dirty.push(page);
        }
        Ok(Self {
            step: CommitStep::ReadOriginals,
            pending: None,
            mode: shared.mode(),
            sync_mode,
            sync_type: pager.get_sync_type(),
            page_size,
            original_pages,
            nonce: pager.io.generate_random_number() as u32,
            journaled,
            dirty,
            journal: None,
        })
    }

    fn run(&mut self, pager: &Pager, shared: &RollbackJournalShared) -> IOResultOr<()> {
        loop {
            if let Some(completion) = self.pending.take() {
                if !completion.finished() {
                    self.pending = Some(completion.clone());
                    io_yield_one!(completion);
                }
                if let Some(err) = completion.get_error() {
                    return Err(LimboError::CompletionError(err).into());
                }
            }
            let step = self.step;
            self.step = next_step(step);
            self.pending = match step {
                CommitStep::ReadOriginals => self.read_originals(pager)?,
                CommitStep::OpenJournal => self.open_journal(pager, shared)?,
                CommitStep::WriteJournal => Some(self.write_journal()?),
                CommitStep::SyncJournalRecords if self.sync_mode == SyncMode::Full => {
                    Some(self.sync_journal()?)
                }
                CommitStep::WriteRecordCount if self.sync_mode != SyncMode::Off => {
                    Some(self.write_record_count()?)
                }
                CommitStep::SyncJournalHeader if self.sync_mode != SyncMode::Off => {
                    Some(self.sync_journal()?)
                }
                CommitStep::WriteDatabase => {
                    shared.needs_playback.store(true, Ordering::Release);
                    Some(self.write_database(pager)?)
                }
                CommitStep::SyncDatabase if self.sync_mode != SyncMode::Off => Some(
                    pager
                        .db_file
                        .sync(Completion::new_sync(|_| {}), self.sync_type)?,
                ),
                CommitStep::FinalizeJournal => self.finalize_journal(pager, shared)?,
                CommitStep::SyncFinalizedJournal => {
                    shared.needs_playback.store(false, Ordering::Release);
                    self.sync_finalized_journal()?
                }
                CommitStep::Done => {
                    self.step = CommitStep::Done;
                    return Ok(IOResult::Done(()));
                }
                CommitStep::SyncJournalRecords
                | CommitStep::WriteRecordCount
                | CommitStep::SyncJournalHeader
                | CommitStep::SyncDatabase => None,
            };
        }
    }

    fn read_originals(&self, pager: &Pager) -> Result<Option<Completion>> {
        if self.journaled.is_empty() {
            return Ok(None);
        }
        let mut group = CompletionGroup::new(|_| {});
        let io_ctx = IOContext::untransformed();
        for (page_id, buffer) in &self.journaled {
            let page_id = *page_id;
            let completion = Completion::new_read(buffer.clone(), move |result| {
                let (buffer, bytes_read) = match result {
                    Ok(read) => read,
                    Err(err) => return Some(err),
                };
                // SQLite reads the missing tail of a short last page as zeros.
                let bytes_read = bytes_read.max(0) as usize;
                buffer.as_mut_slice()[bytes_read.min(buffer.len())..].fill(0);
                None
            });
            group.add(&completion);
            let _ = pager
                .db_file
                .read_page(page_id as usize, &io_ctx, completion)?;
        }
        Ok(Some(group.build()))
    }

    fn open_journal(
        &mut self,
        pager: &Pager,
        shared: &RollbackJournalShared,
    ) -> Result<Option<Completion>> {
        let journal = match open_existing(pager.io.as_ref(), &shared.path, OpenFlags::NoLock)? {
            Some(journal) => journal,
            None => {
                let journal = pager.io.open_file(
                    &shared.path,
                    OpenFlags::Create | OpenFlags::NoLock,
                    false,
                )?;
                if self.sync_mode != SyncMode::Off && !pager.io.is_memory() {
                    sync_parent_directory(&shared.path);
                }
                journal
            }
        };
        let stale = journal.size()? > 0;
        self.journal = Some(journal.clone());
        if !stale {
            return Ok(None);
        }
        // A persisted journal can hold old records and headers past the end
        // of the new ones, which a later playback would apply.
        Ok(Some(journal.truncate(0, Completion::new_trunc(|_| {}))?))
    }

    fn journal(&self) -> &Arc<dyn File> {
        self.journal.as_ref().expect("journal opened")
    }

    fn write_journal(&self) -> Result<Completion> {
        let mut header = vec![0u8; JOURNAL_SECTOR_SIZE as usize];
        if self.sync_mode == SyncMode::Off {
            header[..8].copy_from_slice(&JOURNAL_MAGIC);
            header[8..12].copy_from_slice(&RECORD_COUNT_UNKNOWN.to_be_bytes());
        }
        header[12..16].copy_from_slice(&self.nonce.to_be_bytes());
        header[16..20].copy_from_slice(&self.original_pages.to_be_bytes());
        header[20..24].copy_from_slice(&JOURNAL_SECTOR_SIZE.to_be_bytes());
        header[24..28].copy_from_slice(&(self.page_size as u32).to_be_bytes());
        let mut buffers = Vec::with_capacity(self.journaled.len() + 1);
        buffers.push(Arc::new(Buffer::new(header)));
        for (page_id, original) in &self.journaled {
            let page = original.as_slice();
            let mut record = Vec::with_capacity(self.page_size + 8);
            record.extend_from_slice(&page_id.to_be_bytes());
            record.extend_from_slice(page);
            record.extend_from_slice(&record_checksum(self.nonce, page).to_be_bytes());
            buffers.push(Arc::new(Buffer::new(record)));
        }
        self.journal()
            .pwritev(0, buffers, Completion::new_write(|_| {}))
    }

    fn sync_journal(&self) -> Result<Completion> {
        self.journal()
            .sync(Completion::new_sync(|_| {}), self.sync_type)
    }

    fn write_record_count(&self) -> Result<Completion> {
        let mut fields = vec![0u8; RECORD_COUNT_FIELDS_LEN];
        fields[..8].copy_from_slice(&JOURNAL_MAGIC);
        fields[8..12].copy_from_slice(&(self.journaled.len() as u32).to_be_bytes());
        self.journal().pwrite(
            0,
            Arc::new(Buffer::new(fields)),
            Completion::new_write(|_| {}),
        )
    }

    fn write_database(&self, pager: &Pager) -> Result<Completion> {
        let mut group = CompletionGroup::new(|_| {});
        for page in &self.dirty {
            let _ = begin_write_btree_page(pager, page, Some(&mut group))?;
        }
        Ok(group.build())
    }

    fn finalize_journal(
        &mut self,
        pager: &Pager,
        shared: &RollbackJournalShared,
    ) -> Result<Option<Completion>> {
        match self.mode {
            RollbackJournalMode::Truncate => Ok(Some(
                self.journal()
                    .truncate(0, Completion::new_trunc(|_| {}))?,
            )),
            RollbackJournalMode::Persist => Ok(Some(self.journal().pwrite(
                0,
                Arc::new(Buffer::new(vec![0u8; JOURNAL_HEADER_FIELDS_LEN])),
                Completion::new_write(|_| {}),
            )?)),
            RollbackJournalMode::Delete => {
                self.journal = None;
                pager.io.remove_file(&shared.path)?;
                Ok(None)
            }
        }
    }

    fn sync_finalized_journal(&mut self) -> Result<Option<Completion>> {
        let sync = match self.mode {
            RollbackJournalMode::Truncate => self.sync_mode == SyncMode::Full,
            RollbackJournalMode::Persist => self.sync_mode != SyncMode::Off,
            RollbackJournalMode::Delete => false,
        };
        let completion = if sync { Some(self.sync_journal()?) } else { None };
        Ok(completion)
    }
}

// SQLite syncs the directory of a journal it created, so that the journal
// still exists after a power loss. A file system that cannot sync a directory
// is not an error for SQLite.
fn sync_parent_directory(path: &str) {
    let Some(directory) = std::path::Path::new(path).parent() else {
        return;
    };
    let directory = if directory.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        directory
    };
    if let Err(err) = std::fs::File::open(directory).and_then(|dir| dir.sync_all()) {
        tracing::debug!("syncing the directory of journal '{path}' failed: {err}");
    }
}

fn next_step(step: CommitStep) -> CommitStep {
    match step {
        CommitStep::ReadOriginals => CommitStep::OpenJournal,
        CommitStep::OpenJournal => CommitStep::WriteJournal,
        CommitStep::WriteJournal => CommitStep::SyncJournalRecords,
        CommitStep::SyncJournalRecords => CommitStep::WriteRecordCount,
        CommitStep::WriteRecordCount => CommitStep::SyncJournalHeader,
        CommitStep::SyncJournalHeader => CommitStep::WriteDatabase,
        CommitStep::WriteDatabase => CommitStep::SyncDatabase,
        CommitStep::SyncDatabase => CommitStep::FinalizeJournal,
        CommitStep::FinalizeJournal => CommitStep::SyncFinalizedJournal,
        CommitStep::SyncFinalizedJournal | CommitStep::Done => CommitStep::Done,
    }
}

// SQLite's `pager_cksum`: the nonce plus every 200th byte of the page,
// counted back from 200 bytes before its end.
fn record_checksum(nonce: u32, page: &[u8]) -> u32 {
    let mut checksum = nonce;
    let mut offset = page.len() as isize - 200;
    while offset > 0 {
        checksum = checksum.wrapping_add(page[offset as usize] as u32);
        offset -= 200;
    }
    checksum
}

pub(crate) fn recover_hot_journal(
    io: &dyn IO,
    db_file: &dyn DatabaseStorage,
    journal_path: &str,
    open_flags: OpenFlags,
) -> Result<()> {
    let read_only = open_flags.contains(OpenFlags::ReadOnly);
    let Some(journal) = open_existing(io, journal_path, OpenFlags::ReadOnly)? else {
        return Ok(());
    };
    if journal.size()? == 0 {
        return Ok(());
    }
    if db_file.size()? == 0 {
        // SQLite deletes a journal next to an empty database: the journal
        // belongs to a deleted database, or to a first transaction that never
        // wrote the database file.
        drop(journal);
        if !read_only {
            io.remove_file(journal_path)?;
        }
        return Ok(());
    }
    let first = read_exact(io, journal.as_ref(), 0, 1)?;
    if first.is_none_or(|byte| byte[0] == 0) {
        return Ok(());
    }
    if read_only {
        return Err(LimboError::InvalidArgument(format!(
            "database has a hot journal '{journal_path}' that a read-only connection cannot roll back"
        )));
    }
    drop(journal);
    roll_back_journal(io, db_file, journal_path, RollbackJournalMode::Delete)
}

fn roll_back_journal(
    io: &dyn IO,
    db_file: &dyn DatabaseStorage,
    journal_path: &str,
    mode: RollbackJournalMode,
) -> Result<()> {
    let Some(journal) = open_existing(io, journal_path, OpenFlags::NoLock)? else {
        return Ok(());
    };
    wait(io, journal.sync(Completion::new_sync(|_| {}), FileSyncType::Fsync)?)?;
    if super_journal_committed(io, journal.as_ref())? {
        tracing::debug!("journal '{journal_path}' belongs to a committed multi-database transaction");
    } else {
        play_back(io, db_file, journal.as_ref())?;
    }
    wait(io, db_file.sync(Completion::new_sync(|_| {}), FileSyncType::Fsync)?)?;
    drop(journal);
    finalize_journal_blocking(io, journal_path, mode)
}

fn finalize_journal_blocking(io: &dyn IO, journal_path: &str, mode: RollbackJournalMode) -> Result<()> {
    match mode {
        RollbackJournalMode::Delete => match io.remove_file(journal_path) {
            Ok(()) => Ok(()),
            Err(err) if is_not_found(&err) => Ok(()),
            Err(err) => Err(err),
        },
        RollbackJournalMode::Truncate => {
            let Some(journal) = open_existing(io, journal_path, OpenFlags::NoLock)? else {
                return Ok(());
            };
            wait(io, journal.truncate(0, Completion::new_trunc(|_| {}))?)?;
            wait(io, journal.sync(Completion::new_sync(|_| {}), FileSyncType::Fsync)?)
        }
        RollbackJournalMode::Persist => {
            let Some(journal) = open_existing(io, journal_path, OpenFlags::NoLock)? else {
                return Ok(());
            };
            wait(
                io,
                journal.pwrite(
                    0,
                    Arc::new(Buffer::new(vec![0u8; JOURNAL_HEADER_FIELDS_LEN])),
                    Completion::new_write(|_| {}),
                )?,
            )?;
            wait(io, journal.sync(Completion::new_sync(|_| {}), FileSyncType::Fsync)?)
        }
    }
}

// SQLite's `pager_playback` for a hot journal: every journal segment is read
// in turn, the database is truncated to the size the first segment records,
// and the records up to the first bad checksum are written back.
fn play_back(io: &dyn IO, db_file: &dyn DatabaseStorage, journal: &dyn File) -> Result<()> {
    let journal_size = journal.size()?;
    let mut offset = 0u64;
    let mut sector_size = JOURNAL_SECTOR_SIZE as u64;
    let mut page_size = 0usize;
    let mut database_pages = 0u32;
    let mut first_segment = true;
    'segments: loop {
        let header_offset = if offset == 0 {
            0
        } else {
            (offset - 1) / sector_size * sector_size + sector_size
        };
        if header_offset + sector_size > journal_size {
            break;
        }
        let Some(fields) = read_exact(io, journal, header_offset, JOURNAL_HEADER_FIELDS_LEN)?
        else {
            break;
        };
        if fields[..8] != JOURNAL_MAGIC {
            break;
        }
        let mut record_count = be_u32(&fields[8..12]);
        let nonce = be_u32(&fields[12..16]);
        let original_pages = be_u32(&fields[16..20]);
        if first_segment {
            let journal_sector_size = be_u32(&fields[20..24]);
            let mut journal_page_size = be_u32(&fields[24..28]);
            if journal_page_size == 0 {
                journal_page_size = database_page_size(io, db_file)?;
            }
            if !(512..=MAX_PAGE_SIZE).contains(&journal_page_size)
                || !journal_page_size.is_power_of_two()
                || !(32..=MAX_JOURNAL_SECTOR_SIZE).contains(&journal_sector_size)
                || !journal_sector_size.is_power_of_two()
            {
                break;
            }
            page_size = journal_page_size as usize;
            sector_size = journal_sector_size as u64;
            if header_offset + sector_size > journal_size {
                break;
            }
        }
        offset = header_offset + sector_size;
        let record_size = page_size as u64 + 8;
        if record_count == RECORD_COUNT_UNKNOWN {
            record_count = ((journal_size - sector_size) / record_size) as u32;
        }
        if first_segment {
            set_database_size(io, db_file, page_size, original_pages)?;
            database_pages = original_pages;
            first_segment = false;
        }
        let pending_byte_page = Pager::get_pending_byte() / page_size as u32 + 1;
        for _ in 0..record_count {
            let Some(record) = read_exact(io, journal, offset, page_size + 8)? else {
                break 'segments;
            };
            offset += record_size;
            let page_id = be_u32(&record[..4]);
            if page_id == 0 || page_id == pending_byte_page {
                offset = journal_size;
                break;
            }
            if page_id > database_pages {
                continue;
            }
            let image = &record[4..4 + page_size];
            if record_checksum(nonce, image) != be_u32(&record[4 + page_size..]) {
                offset = journal_size;
                break;
            }
            wait(
                io,
                db_file.write_page(
                    page_id as usize,
                    Arc::new(Buffer::new(image.to_vec())),
                    &IOContext::untransformed(),
                    Completion::new_write(|_| {}),
                )?,
            )?;
        }
    }
    Ok(())
}

// SQLite's `pager_truncate`: shrinks the file, or grows it by writing a
// zeroed last page.
fn set_database_size(
    io: &dyn IO,
    db_file: &dyn DatabaseStorage,
    page_size: usize,
    pages: u32,
) -> Result<()> {
    let current = db_file.size()?;
    let wanted = pages as u64 * page_size as u64;
    if current > wanted {
        wait(
            io,
            db_file.truncate(wanted as usize, Completion::new_trunc(|_| {}))?,
        )?;
    } else if current + (page_size as u64) <= wanted {
        wait(
            io,
            db_file.write_page(
                pages as usize,
                Arc::new(Buffer::new(vec![0u8; page_size])),
                &IOContext::untransformed(),
                Completion::new_write(|_| {}),
            )?,
        )?;
    }
    Ok(())
}

fn database_page_size(io: &dyn IO, db_file: &dyn DatabaseStorage) -> Result<u32> {
    let header = Arc::new(Buffer::new_temporary(512));
    wait(
        io,
        db_file.read_header(Completion::new_read(header.clone(), |_| None))?,
    )?;
    let raw = u16::from_be_bytes([header.as_slice()[16], header.as_slice()[17]]) as u32;
    Ok(if raw == 1 { MAX_PAGE_SIZE } else { raw })
}

// A journal ends with the name of a super-journal when its transaction
// spanned several databases. That transaction committed when the
// super-journal is gone, and then SQLite does not play the journal back.
fn super_journal_committed(io: &dyn IO, journal: &dyn File) -> Result<bool> {
    let size = journal.size()?;
    if size < SUPER_JOURNAL_TRAILER_LEN {
        return Ok(false);
    }
    let Some(trailer) = read_exact(
        io,
        journal,
        size - SUPER_JOURNAL_TRAILER_LEN,
        SUPER_JOURNAL_TRAILER_LEN as usize,
    )?
    else {
        return Ok(false);
    };
    if trailer[8..] != JOURNAL_MAGIC {
        return Ok(false);
    }
    let name_len = be_u32(&trailer[..4]) as u64;
    if name_len == 0 || name_len > size - SUPER_JOURNAL_TRAILER_LEN {
        return Ok(false);
    }
    let Some(name) = read_exact(
        io,
        journal,
        size - SUPER_JOURNAL_TRAILER_LEN - name_len,
        name_len as usize,
    )?
    else {
        return Ok(false);
    };
    let checksum = name
        .iter()
        .fold(be_u32(&trailer[4..8]), |sum, byte| sum.wrapping_sub(*byte as u32));
    if checksum != 0 {
        return Ok(false);
    }
    let name_end = name.iter().position(|byte| *byte == 0).unwrap_or(name.len());
    let Ok(super_path) = std::str::from_utf8(&name[..name_end]) else {
        return Ok(false);
    };
    if super_path.is_empty() {
        return Ok(false);
    }
    Ok(open_existing(io, super_path, OpenFlags::ReadOnly)?.is_none())
}

fn open_existing(io: &dyn IO, path: &str, flags: OpenFlags) -> Result<Option<Arc<dyn File>>> {
    match io.open_file(path, flags, false) {
        Ok(file) => Ok(Some(file)),
        Err(err) if is_not_found(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

fn is_not_found(err: &LimboError) -> bool {
    matches!(
        err,
        LimboError::CompletionError(CompletionError::IOError(std::io::ErrorKind::NotFound, _))
    )
}

fn read_exact(io: &dyn IO, file: &dyn File, pos: u64, len: usize) -> Result<Option<Vec<u8>>> {
    let buffer = Arc::new(Buffer::new_temporary(len));
    let bytes_read = Arc::new(AtomicU64::new(0));
    let completion = {
        let bytes_read = bytes_read.clone();
        Completion::new_read(buffer.clone(), move |result| {
            if let Ok((_, n)) = result {
                bytes_read.store(n.max(0) as u64, Ordering::Release);
            }
            None
        })
    };
    wait(io, file.pread(pos, completion)?)?;
    if bytes_read.load(Ordering::Acquire) < len as u64 {
        return Ok(None);
    }
    Ok(Some(buffer.as_slice().to_vec()))
}

fn wait(io: &dyn IO, completion: Completion) -> Result<()> {
    io.wait_for_completion(completion)
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().expect("four bytes"))
}
