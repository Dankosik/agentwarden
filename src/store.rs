//! Files agentwarden keeps: remembered state and the action log.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::state::State;

const LOG_MAX_BYTES: u64 = 1 << 20;
const LOG_KEEP_LINES: usize = 2000;
const TAIL_CHUNK_BYTES: usize = 8192;
const STACK_STATE_MAX_BYTES: usize = 4096;

/// Identity and modification stamp of a file on disk, used to avoid re-reading
/// unchanged `state.json` and MCP config files across `watch` passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl FileStamp {
    pub(crate) fn from_metadata(meta: &fs::Metadata) -> Self {
        Self {
            modified: meta.modified().ok(),
            len: meta.len(),
            #[cfg(unix)]
            dev: meta.dev(),
            #[cfg(unix)]
            ino: meta.ino(),
        }
    }

    pub(crate) fn of(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        Some(Self::from_metadata(&meta))
    }
}

/// Fixed stack-buffered writer so serializing `state.json` allocates zero heap
/// buffers while still issuing a single `write` syscall for typical states.
struct StackBufWriter<W, const N: usize> {
    inner: W,
    buf: [u8; N],
    len: usize,
}

impl<W: Write, const N: usize> StackBufWriter<W, N> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            buf: [0u8; N],
            len: 0,
        }
    }

    fn flush_buf(&mut self) -> io::Result<()> {
        if self.len > 0 {
            self.inner.write_all(&self.buf[..self.len])?;
            self.len = 0;
        }
        Ok(())
    }
}

impl<W: Write, const N: usize> Write for StackBufWriter<W, N> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > N - self.len {
            self.flush_buf()?;
            if bytes.len() >= N {
                self.inner.write_all(bytes)?;
                return Ok(bytes.len());
            }
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buf()?;
        self.inner.flush()
    }
}

/// In-memory `State` cache across `watch` passes, validated by `stat()` so
/// concurrent updates from `reclaim` are immediately observed while unchanged
/// passes do zero file reads or JSON parsing.
#[derive(Default)]
pub struct StateCache {
    initialized: bool,
    persisted: bool,
    stamp: Option<FileStamp>,
    state: State,
}

impl StateCache {
    pub fn load_mut(&mut self, store: &Store) -> (&mut State, bool) {
        let path = store.state_path();
        let stamp = FileStamp::of(&path);
        if self.initialized && self.stamp == stamp {
            return (&mut self.state, self.persisted);
        }
        if stamp.is_none() {
            self.state = State::default();
            self.persisted = false;
            self.stamp = None;
            self.initialized = true;
            return (&mut self.state, false);
        }
        match File::open(&path) {
            Ok(mut file) => {
                let opened_stamp = file
                    .metadata()
                    .ok()
                    .map(|meta| FileStamp::from_metadata(&meta));
                match Store::read_state_file(&mut file) {
                    Some(state) => {
                        self.state = state;
                        self.persisted = true;
                    }
                    None => {
                        self.state = State::default();
                        self.persisted = false;
                    }
                }
                self.stamp = opened_stamp;
                self.initialized = opened_stamp.is_some();
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.state = State::default();
                self.persisted = false;
                self.stamp = None;
                self.initialized = true;
            }
            Err(_) => {
                // Transient open failure: do not poison the cache so the next pass retries.
                self.state = State::default();
                self.persisted = false;
                self.stamp = None;
                self.initialized = false;
            }
        }
        (&mut self.state, self.persisted)
    }

    pub(crate) fn mark_saved(&mut self, stamp: Option<FileStamp>) {
        self.persisted = true;
        self.stamp = stamp;
        self.initialized = stamp.is_some();
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.persisted = false;
    }
}

pub struct Store {
    dir: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Stopped,
    Skipped,
    Failed,
    Restarted,
    Refused,
}

/// One effect, as the action log records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub at: u64,
    pub rule: CompactString,
    pub outcome: Outcome,
    pub target: CompactString,
    pub pids: SmallVec<[i32; 4]>,
    pub footprint_bytes: u64,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `~/Library/Application Support/agentwarden`.
    pub fn for_home(home: &Path) -> Self {
        Self::new(home.join("Library/Application Support/agentwarden"))
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join("actions.jsonl")
    }

    /// A missing or unreadable state starts fresh: it only holds history.
    #[cfg(test)]
    pub fn load_state(&self) -> State {
        self.load_state_with_status().0
    }

    fn read_state_file(file: &mut File) -> Option<State> {
        let len = file.metadata().ok().map(|meta| meta.len());
        if len.is_some_and(|n| n <= STACK_STATE_MAX_BYTES as u64) {
            let mut stack = [0u8; STACK_STATE_MAX_BYTES];
            let mut total = 0usize;
            loop {
                if total == stack.len() {
                    // File grew concurrently past stack buffer; spill to heap.
                    let mut heap = Vec::with_capacity(stack.len() * 2);
                    heap.extend_from_slice(&stack);
                    file.read_to_end(&mut heap).ok()?;
                    return serde_json::from_slice(&heap).ok();
                }
                match file.read(&mut stack[total..]) {
                    Ok(0) => return serde_json::from_slice(&stack[..total]).ok(),
                    Ok(n) => total += n,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return None,
                }
            }
        }
        serde_json::from_reader(BufReader::with_capacity(4096, file)).ok()
    }

    /// Load state and report whether a valid `state.json` was already on disk.
    pub fn load_state_with_status(&self) -> (State, bool) {
        match File::open(self.state_path())
            .ok()
            .and_then(|mut file| Self::read_state_file(&mut file))
        {
            Some(state) => (state, true),
            None => (State::default(), false),
        }
    }

    #[cfg(test)]
    pub fn save_state(&self, state: &State) -> io::Result<()> {
        self.save_state_stamped(state).map(|_| ())
    }

    pub(crate) fn save_state_stamped(&self, state: &State) -> io::Result<Option<FileStamp>> {
        fs::create_dir_all(&self.dir)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.dir)?;
        {
            let mut writer = StackBufWriter::<_, 1024>::new(temporary.as_file_mut());
            serde_json::to_writer(&mut writer, state).map_err(io::Error::other)?;
            writer.flush()?;
        }
        temporary.as_file().sync_all()?;
        let stamp = temporary
            .as_file()
            .metadata()
            .ok()
            .map(|meta| FileStamp::from_metadata(&meta));
        temporary
            .persist(self.state_path())
            .map_err(|error| error.error)?;
        Ok(stamp)
    }

    pub fn append(&self, records: &[Record]) -> io::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        fs::create_dir_all(&self.dir)?;
        let path = self.log_path();
        let mut lines = Vec::with_capacity(records.len().saturating_mul(160));
        for record in records {
            serde_json::to_writer(&mut lines, record).map_err(io::Error::other)?;
            lines.push(b'\n');
        }
        let size = {
            let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
            file.write_all(&lines)?;
            file.flush()?;
            file.metadata()?.len()
        };
        // The handle is closed first: Windows cannot replace an open file.
        if size > LOG_MAX_BYTES {
            self.trim_log(&path, LOG_KEEP_LINES)?;
        }
        Ok(())
    }

    /// The newest `count` records; unreadable lines are skipped.
    pub fn recent(&self, count: usize) -> Vec<Record> {
        if count == 0 {
            return Vec::new();
        }
        let Ok(mut file) = File::open(self.log_path()) else {
            return Vec::new();
        };
        let Ok(Some((start, _))) = Self::tail_start_offset(&mut file, count) else {
            return Vec::new();
        };
        if file.seek(SeekFrom::Start(start)).is_err() {
            return Vec::new();
        }
        let mut reader = BufReader::new(file);
        let mut buffer = Vec::new();
        let mut records = Vec::with_capacity(count.min(64));
        // A line torn by a crash may not be UTF-8; it must not hide the rest.
        while reader.read_until(b'\n', &mut buffer).unwrap_or(0) > 0 {
            let mut end = buffer.len();
            while end > 0 && matches!(buffer[end - 1], b'\n' | b'\r') {
                end -= 1;
            }
            let line = String::from_utf8_lossy(&buffer[..end]);
            if let Ok(record) = serde_json::from_str(&line) {
                records.push(record);
            }
            buffer.clear();
        }
        records
    }

    /// Byte offset where the last `count` lines begin, plus whether the file
    /// ends with `'\n'`. Scans backwards in fixed chunks so reading the tail of
    /// a large log does not scan or allocate the whole file.
    fn tail_start_offset(file: &mut File, count: usize) -> io::Result<Option<(u64, bool)>> {
        if count == 0 {
            return Ok(None);
        }
        let len = file.metadata()?.len();
        if len == 0 {
            return Ok(None);
        }
        let mut last = [0u8; 1];
        file.seek(SeekFrom::Start(len - 1))?;
        file.read_exact(&mut last)?;
        let ends_with_newline = last[0] == b'\n';
        let mut pos = if ends_with_newline { len - 1 } else { len };
        let mut found = 0usize;
        let mut chunk = [0u8; TAIL_CHUNK_BYTES];
        while pos > 0 {
            let to_read =
                usize::try_from(pos.min(TAIL_CHUNK_BYTES as u64)).unwrap_or(TAIL_CHUNK_BYTES);
            pos -= to_read as u64;
            file.seek(SeekFrom::Start(pos))?;
            file.read_exact(&mut chunk[..to_read])?;
            for rel in memchr::memrchr_iter(b'\n', &chunk[..to_read]) {
                found += 1;
                if found == count {
                    return Ok(Some((pos + rel as u64 + 1, ends_with_newline)));
                }
            }
        }
        Ok(Some((0, ends_with_newline)))
    }

    fn trim_log(&self, path: &Path, keep_lines: usize) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.dir)?;
        // Scope the source handle so it closes before `persist` (Windows).
        {
            let mut file = match File::open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            if let Some((start, ends_with_newline)) =
                Self::tail_start_offset(&mut file, keep_lines)?
            {
                file.seek(SeekFrom::Start(start))?;
                io::copy(&mut file, &mut temporary)?;
                if !ends_with_newline {
                    temporary.write_all(b"\n")?;
                }
            }
        }
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use smallvec::smallvec;

    use super::{LOG_KEEP_LINES, Outcome, Record, Store};
    use crate::state::State;

    fn record(at: u64) -> Record {
        Record {
            at,
            rule: "orphan".into(),
            outcome: Outcome::Stopped,
            target: "node".into(),
            pids: smallvec![400, 401],
            footprint_bytes: 94 << 20,
            reason: "its agent session has exited".into(),
            detail: None,
        }
    }

    #[test]
    fn state_round_trips_and_missing_state_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("nested"));
        assert_eq!(store.load_state_with_status(), (State::default(), false));
        let state = State {
            last_codex_restart_at: Some(42),
            ..State::default()
        };
        store.save_state(&state).unwrap();
        assert_eq!(store.load_state_with_status(), (state, true));
        std::fs::write(dir.path().join("nested/state.json"), b"{broken").unwrap();
        assert_eq!(store.load_state_with_status(), (State::default(), false));
    }

    #[test]
    fn log_appends_reads_newest_and_stays_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        store.append(&[record(1), record(2)]).unwrap();
        store.append(&[record(3)]).unwrap();
        let recent = store.recent(2);
        assert_eq!(recent.iter().map(|r| r.at).collect::<Vec<_>>(), [2, 3]);

        let many: Vec<Record> = (0..10_000).map(record).collect();
        store.append(&many).unwrap();
        let size = std::fs::metadata(store.log_path()).unwrap().len();
        assert!(size <= super::LOG_MAX_BYTES, "log was trimmed: {size}");
        let all = store.recent(usize::MAX - 1);
        assert_eq!(all.len(), LOG_KEEP_LINES);
        assert_eq!(all.last().unwrap().at, 9999);
        store.append(&[record(7000)]).unwrap();
        assert_eq!(store.recent(1)[0].at, 7000);
    }

    #[test]
    fn a_torn_log_line_does_not_hide_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        store.append(&[record(1)]).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(store.log_path())
            .unwrap();
        std::io::Write::write_all(&mut file, b"{\"at\":\xff\xfe\n").unwrap();
        store.append(&[record(2)]).unwrap();
        let at: Vec<u64> = store.recent(10).iter().map(|r| r.at).collect();
        assert_eq!(at, [1, 2]);
    }

    #[test]
    fn state_from_an_older_version_keeps_what_it_has() {
        let state: State = serde_json::from_str(r#"{"last_codex_restart_at": 5}"#).unwrap();
        assert_eq!(state.last_codex_restart_at, Some(5));
    }

    #[test]
    fn recent_handles_empty_zero_crlf_missing_newline_and_long_lines() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        assert!(store.recent(0).is_empty());
        assert!(store.recent(5).is_empty());

        std::fs::write(store.log_path(), b"").unwrap();
        assert!(store.recent(5).is_empty());

        // Single line without trailing newline and CRLF line endings.
        let r1 = serde_json::to_string(&record(10)).unwrap();
        let r2 = serde_json::to_string(&record(20)).unwrap();
        std::fs::write(store.log_path(), format!("{r1}\r\n{r2}")).unwrap();
        assert_eq!(
            store.recent(1).iter().map(|r| r.at).collect::<Vec<_>>(),
            [20]
        );
        assert_eq!(
            store.recent(2).iter().map(|r| r.at).collect::<Vec<_>>(),
            [10, 20]
        );
        assert!(store.recent(0).is_empty());

        // Line larger than TAIL_CHUNK_BYTES (8192) spanning multiple chunks.
        let dir2 = tempfile::tempdir().unwrap();
        let store2 = Store::new(dir2.path().to_owned());
        let mut big = record(30);
        big.reason = "x".repeat(12_000);
        store2.append(&[record(1), big, record(40)]).unwrap();
        let tail = store2.recent(2);
        assert_eq!(tail.iter().map(|r| r.at).collect::<Vec<_>>(), [30, 40]);
        assert_eq!(tail[0].reason.len(), 12_000);
    }

    #[test]
    fn state_cache_reuses_unchanged_state_and_reloads_on_external_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let mut cache = super::StateCache::default();

        // Missing file -> fresh unpersisted state.
        let (state, persisted) = cache.load_mut(&store);
        assert!(!persisted);
        assert_eq!(*state, State::default());

        // Saving returns a FileStamp matching the persisted file on disk.
        state.last_swap_used_bytes = Some(1024);
        let stamp = store.save_state_stamped(state).unwrap();
        assert_eq!(stamp, super::FileStamp::of(&store.state_path()));
        cache.mark_saved(stamp);

        // Mutating in-memory cached state without touching disk proves the next
        // `load_mut` is a genuine cache hit (no disk re-read).
        let (state, persisted) = cache.load_mut(&store);
        assert!(persisted);
        assert_eq!(state.last_swap_used_bytes, Some(1024));
        state.last_codex_restart_at = Some(777);
        let (again, persisted) = cache.load_mut(&store);
        assert!(persisted);
        assert_eq!(again.last_codex_restart_at, Some(777));

        // External write (e.g. `reclaim`) changes stamp -> cache reloads from disk.
        let external = State {
            last_codex_restart_at: Some(999),
            last_swap_used_bytes: Some(2048),
            ..State::default()
        };
        store.save_state(&external).unwrap();
        let (reloaded, persisted) = cache.load_mut(&store);
        assert!(persisted);
        assert_eq!(*reloaded, external);

        // Large state (> STACK_STATE_MAX_BYTES) exercises the reader fallback and
        // multi-flush StackBufWriter.
        let mut large = State::default();
        for pid in 0..120 {
            large.known_helpers.insert(crate::model::Identity {
                pid,
                started: "Tue Oct 6 12:37:26 2026".into(),
            });
        }
        let stamp = store.save_state_stamped(&large).unwrap();
        assert!(std::fs::metadata(store.state_path()).unwrap().len() > 4096);
        assert_eq!(stamp, super::FileStamp::of(&store.state_path()));
        let (reloaded_large, persisted) = cache.load_mut(&store);
        assert!(persisted);
        assert_eq!(*reloaded_large, large);

        // Removing state.json invalidates the cache.
        std::fs::remove_file(store.state_path()).unwrap();
        let (after_delete, persisted) = cache.load_mut(&store);
        assert!(!persisted);
        assert_eq!(*after_delete, State::default());
    }
}
