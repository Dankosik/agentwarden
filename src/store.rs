//! Files agentwarden keeps: remembered state and the action log.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::state::State;

const LOG_MAX_BYTES: u64 = 1 << 20;
const LOG_KEEP_LINES: usize = 2000;

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
    pub rule: String,
    pub outcome: Outcome,
    pub target: String,
    pub pids: Vec<i32>,
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
    pub fn load_state(&self) -> State {
        fs::read(self.state_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save_state(&self, state: &State) -> io::Result<()> {
        let bytes = serde_json::to_vec(state).map_err(io::Error::other)?;
        self.replace(&self.state_path(), &bytes)
    }

    pub fn append(&self, records: &[Record]) -> io::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        fs::create_dir_all(&self.dir)?;
        let path = self.log_path();
        let mut lines = Vec::new();
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
            let kept = Self::tail(&path, LOG_KEEP_LINES)?;
            self.replace(&path, kept.join("\n").as_bytes())?;
        }
        Ok(())
    }

    /// The newest `count` records; unreadable lines are skipped.
    pub fn recent(&self, count: usize) -> Vec<Record> {
        Self::tail(&self.log_path(), count)
            .unwrap_or_default()
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn tail(path: &Path, count: usize) -> io::Result<Vec<String>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut lines = std::collections::VecDeque::new();
        for line in BufReader::new(file).lines() {
            lines.push_back(line?);
            if lines.len() > count {
                lines.pop_front();
            }
        }
        Ok(lines.into())
    }

    /// Write through a temporary file and rename, so a crash leaves the old file.
    fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.dir)?;
        temporary.write_all(bytes)?;
        if !bytes.ends_with(b"\n") && path.extension().is_some_and(|ext| ext == "jsonl") {
            temporary.write_all(b"\n")?;
        }
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{LOG_KEEP_LINES, Outcome, Record, Store};
    use crate::state::State;

    fn record(at: u64) -> Record {
        Record {
            at,
            rule: "orphan".into(),
            outcome: Outcome::Stopped,
            target: "node".into(),
            pids: vec![400, 401],
            footprint_bytes: 94 << 20,
            reason: "its agent session has exited".into(),
            detail: None,
        }
    }

    #[test]
    fn state_round_trips_and_missing_state_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("nested"));
        assert_eq!(store.load_state(), State::default());
        let state = State {
            last_codex_restart_at: Some(42),
            ..State::default()
        };
        store.save_state(&state).unwrap();
        assert_eq!(store.load_state(), state);
        std::fs::write(dir.path().join("nested/state.json"), b"{broken").unwrap();
        assert_eq!(store.load_state(), State::default());
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
}
