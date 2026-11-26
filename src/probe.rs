//! Parsers for the text that macOS system commands print, and for agent MCP
//! configuration. Pure functions, so they are tested on every platform.

use std::io::{self, Read};

#[cfg(test)]
use std::collections::HashMap;

use compact_str::CompactString;
use smallvec::SmallVec;

use crate::model::{Pressure, Process, ServerSignature, Swap};

/// Fields from `ps -axww -o pid=,ppid=,uid=,tty=,state=,time=,etime=,lstart=`.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub struct PsRow {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    pub has_tty: bool,
    /// Exited but not yet reaped; signals cannot reach it.
    pub zombie: bool,
    pub cpu_centis: u64,
    pub age_secs: u64,
    pub started: String,
}

fn next_word<'a>(rest: &mut &'a str) -> Option<&'a str> {
    let trimmed = rest.trim_start();
    if trimmed.is_empty() {
        *rest = "";
        return None;
    }
    match trimmed.find(char::is_whitespace) {
        Some(end) => {
            let (word, tail) = trimmed.split_at(end);
            *rest = tail;
            Some(word)
        }
        None => {
            *rest = "";
            Some(trimmed)
        }
    }
}

fn is_lstart_hms(word: &str) -> bool {
    let b = word.as_bytes();
    b.len() == 8
        && b[2] == b':'
        && b[5] == b':'
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4].is_ascii_digit()
        && b[6].is_ascii_digit()
        && b[7].is_ascii_digit()
}

fn is_lstart_year(word: &str) -> bool {
    let b = word.as_bytes();
    b.len() == 4 && b.iter().all(u8::is_ascii_digit)
}

pub fn join_five(w0: &str, w1: &str, w2: &str, w3: &str, w4: &str) -> String {
    let mut started =
        String::with_capacity(w0.len() + w1.len() + w2.len() + w3.len() + w4.len() + 4);
    started.push_str(w0);
    started.push(' ');
    started.push_str(w1);
    started.push(' ');
    started.push_str(w2);
    started.push(' ');
    started.push_str(w3);
    started.push(' ');
    started.push_str(w4);
    started
}

/// Format five `en_US` `lstart` words into a fixed stack buffer without
/// allocating. Returns `None` only if the joined string exceeds `buf.len()`.
pub fn format_lstart_stack<'a>(lstart: [&str; 5], buf: &'a mut [u8; 40]) -> Option<&'a str> {
    let total =
        lstart[0].len() + lstart[1].len() + lstart[2].len() + lstart[3].len() + lstart[4].len() + 4;
    if total > buf.len() {
        return None;
    }
    let mut pos = 0;
    for (index, word) in lstart.iter().enumerate() {
        if index > 0 {
            buf[pos] = b' ';
            pos += 1;
        }
        let bytes = word.as_bytes();
        buf[pos..pos + bytes.len()].copy_from_slice(bytes);
        pos += bytes.len();
    }
    std::str::from_utf8(&buf[..pos]).ok()
}

/// Losslessly pack an `en_US` `lstart` timestamp (`Day Mon DD HH:MM:SS YYYY`,
/// years 2000..=2132) into a `u32` so pass-to-pass caches need no heap strings
/// and have zero hash collisions on process start times.
pub fn pack_lstart(lstart: [&str; 5]) -> Option<u32> {
    let [_, mon, day, hms, year] = lstart;
    let month: u32 = match mon {
        "Jan" => 0,
        "Feb" => 1,
        "Mar" => 2,
        "Apr" => 3,
        "May" => 4,
        "Jun" => 5,
        "Jul" => 6,
        "Aug" => 7,
        "Sep" => 8,
        "Oct" => 9,
        "Nov" => 10,
        "Dec" => 11,
        _ => return None,
    };
    let day: u32 = day.parse::<u32>().ok()?.checked_sub(1)?;
    if day >= 31 {
        return None;
    }
    let b = hms.as_bytes();
    if !is_lstart_hms(hms) {
        return None;
    }
    let hour = u32::from(b[0] - b'0') * 10 + u32::from(b[1] - b'0');
    let min = u32::from(b[3] - b'0') * 10 + u32::from(b[4] - b'0');
    let sec = u32::from(b[6] - b'0') * 10 + u32::from(b[7] - b'0');
    if hour >= 24 || min >= 60 || sec >= 60 {
        return None;
    }
    let year: u32 = year.parse::<u32>().ok()?.checked_sub(2000)?;
    if year > 132 {
        return None;
    }
    Some((((((year * 12 + month) * 31 + day) * 24 + hour) * 60 + min) * 60) + sec)
}

/// Encode macOS `ps -o tty=` column into a compact `u16` identifier:
/// - `""` or starting with `'?'` (`??`) -> `0` (no controlling terminal)
/// - `ttysNNN` (`sNNN` on truncated displays) -> `0x8000 | (NNN & 0x7FFF)` (bijective for 0..=32767)
/// - `console` -> `1`
/// - any other TTY name -> deterministic non-zero hash in `2..0x8000`.
pub fn parse_tty_id(tty: &str) -> u16 {
    if tty.is_empty() || tty.starts_with('?') {
        return 0;
    }
    if tty == "console" {
        return 1;
    }
    if let Some(num_str) = tty.strip_prefix("ttys").or_else(|| tty.strip_prefix('s'))
        && !num_str.is_empty()
        && num_str.bytes().all(|b| b.is_ascii_digit())
        && let Ok(num) = num_str.parse::<u16>()
        && num < 0x8000
    {
        return 0x8000 | num;
    }
    let mut h: u32 = 2_166_136_261;
    for &b in tty.as_bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(16_777_619);
    }
    let folded = ((h ^ (h >> 16)) & 0x7FFF) as u16;
    if folded < 2 { folded + 2 } else { folded }
}

/// One parsed row from `ps -axww -o pid=,ppid=,uid=,tty=,state=,time=,etime=,lstart=,comm=`.
pub struct ParsedPsLine<'a> {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    pub has_tty: bool,
    pub tty_id: u16,
    pub zombie: bool,
    pub cpu_centis: u64,
    pub age_secs: u64,
    pub lstart: [&'a str; 5],
    pub exe: &'a str,
}

fn parse_ps_prefix(mut rest: &str) -> Option<(ParsedPsLine<'_>, &str)> {
    let pid = next_word(&mut rest)?.parse().ok()?;
    let ppid = next_word(&mut rest)?.parse().ok()?;
    let uid = next_word(&mut rest)?.parse().ok()?;
    let tty = next_word(&mut rest)?;
    let state = next_word(&mut rest)?;
    let time = next_word(&mut rest)?;
    let etime = next_word(&mut rest)?;
    // Five en_US lstart words: Day Mon DD HH:MM:SS YYYY.
    let w0 = next_word(&mut rest)?;
    let w1 = next_word(&mut rest)?;
    let w2 = next_word(&mut rest)?;
    let w3 = next_word(&mut rest)?;
    let w4 = next_word(&mut rest)?;
    if !is_lstart_hms(w3) || !is_lstart_year(w4) {
        return None;
    }
    let tty_id = parse_tty_id(tty);
    Some((
        ParsedPsLine {
            pid,
            ppid,
            uid,
            has_tty: tty_id != 0,
            tty_id,
            zombie: state.starts_with('Z'),
            cpu_centis: clock_centis(time)?,
            age_secs: clock_centis(etime)? / 100,
            lstart: [w0, w1, w2, w3, w4],
            exe: "",
        },
        rest,
    ))
}

/// Parse one line of `ps -axww -o pid=,ppid=,uid=,tty=,state=,time=,etime=,lstart=,comm=`.
pub fn parse_ps_line(line: &str) -> Option<ParsedPsLine<'_>> {
    let (mut row, tail) = parse_ps_prefix(line)?;
    let exe = tail.trim();
    if !row.zombie && exe.is_empty() {
        return None;
    }
    row.exe = exe;
    Some(row)
}

/// Parse one line of `ps -o uid=,lstart= -p <pid>`.
pub fn parse_identify_line(line: &str) -> Option<(CompactString, u32)> {
    let mut words = line.split_whitespace();
    let uid = words.next()?.parse().ok()?;
    let w0 = words.next()?;
    let w1 = words.next()?;
    let w2 = words.next()?;
    let w3 = words.next()?;
    let w4 = words.next()?;
    if words.next().is_some() {
        return None;
    }
    let mut stack = [0u8; 40];
    let started = match format_lstart_stack([w0, w1, w2, w3, w4], &mut stack) {
        Some(text) => CompactString::from(text),
        None => CompactString::from(join_five(w0, w1, w2, w3, w4)),
    };
    Some((started, uid))
}

/// The process table from
/// `ps -axww -o pid=,ppid=,uid=,tty=,state=,time=,etime=,lstart=,comm=`,
/// sorted by PID with zombies omitted, or `None` when it does not read as the
/// expected `en_US` format.
///
/// When `user` is `Some(uid)`, top-level system processes owned by other users
/// (`ppid <= 1 && uid != user`) are skipped because they can never own, belong
/// to, or descend from any of the user's agent processes. Other-user processes
/// with `ppid > 1` (such as setuid children) are kept.
#[cfg(test)]
pub fn ps_processes(text: &str, user: Option<u32>) -> Option<crate::model::Snapshot> {
    let mut non_empty = 0usize;
    let mut parsed = 0usize;
    let bytes_per_kept_row = if user.is_some() { 220 } else { 140 };
    let mut snapshot = crate::model::Snapshot {
        processes: Vec::with_capacity((text.len() / bytes_per_kept_row).max(16)),
        strings: String::with_capacity(text.len() / 2),
        ..crate::model::Snapshot::default()
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        non_empty += 1;
        let Some(row) = parse_ps_line(line) else {
            continue;
        };
        parsed += 1;
        if row.zombie {
            continue;
        }
        if user.is_some_and(|uid| row.uid != uid && row.ppid <= 1) {
            continue;
        }
        let [w0, w1, w2, w3, w4] = row.lstart;
        let started_owned = join_five(w0, w1, w2, w3, w4);
        snapshot.push_process_with_tty(
            row.pid,
            row.ppid,
            row.uid,
            &started_owned,
            row.age_secs,
            row.tty_id,
            row.cpu_centis,
            None,
            row.exe,
            "",
        );
    }
    if parsed == 0 || parsed * 10 < non_empty * 9 {
        return None;
    }
    if !snapshot.processes.is_sorted_by_key(|process| process.pid) {
        snapshot
            .processes
            .sort_unstable_by_key(|process| process.pid);
    }
    Some(snapshot)
}

/// The process table, or `None` when it does not read as the en_US format:
/// an empty or mostly unparsed table must not pass for a machine with no
/// agents.
#[cfg(test)]
pub fn ps_rows(text: &str) -> Option<Vec<PsRow>> {
    let mut non_empty = 0usize;
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        non_empty += 1;
        if let Some(row) = ps_row(line) {
            rows.push(row);
        }
    }
    (!rows.is_empty() && rows.len() * 10 >= non_empty * 9).then_some(rows)
}

#[cfg(test)]
fn ps_row(line: &str) -> Option<PsRow> {
    let (row, tail) = parse_ps_prefix(line)?;
    if !tail.trim().is_empty() {
        return None;
    }
    let [w0, w1, w2, w3, w4] = row.lstart;
    Some(PsRow {
        pid: row.pid,
        ppid: row.ppid,
        uid: row.uid,
        has_tty: row.has_tty,
        zombie: row.zombie,
        cpu_centis: row.cpu_centis,
        age_secs: row.age_secs,
        started: join_five(w0, w1, w2, w3, w4),
    })
}

/// `[[dd-]hh:]mm:ss[.cc]` as hundredths of a second.
fn clock_centis(text: &str) -> Option<u64> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let mut centis = 0u64;
    for (index, part) in clock.split(':').enumerate() {
        let (whole, fraction) = part.split_once('.').unwrap_or((part, "0"));
        let whole: u64 = whole.parse().ok()?;
        centis = centis.checked_mul(if index == 0 { 1 } else { 60 })?;
        centis = centis.checked_add(whole.checked_mul(100)?)?;
        if !fraction.is_empty() && fraction != "0" {
            let bytes = fraction.as_bytes();
            let d0 = bytes[0].checked_sub(b'0').filter(|&d| d <= 9)?;
            let d1 = match bytes.get(1) {
                Some(&b) => b.checked_sub(b'0').filter(|&d| d <= 9)?,
                None => 0,
            };
            centis = centis.checked_add(u64::from(d0) * 10 + u64::from(d1))?;
        }
    }
    days.checked_mul(86_400 * 100)?.checked_add(centis)
}

pub fn parse_pid_column_line(line: &str) -> Option<(i32, &str)> {
    let line = line.trim_start();
    let (pid, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    Some((pid.parse().ok()?, rest.trim()))
}

/// Apply one `ps -ww -o pid=,args=` line onto a PID-sorted snapshot.
#[cfg(test)]
pub fn apply_pid_args_line(line: &str, snapshot: &mut crate::model::Snapshot) {
    let Some((pid, value)) = parse_pid_column_line(line) else {
        return;
    };
    if let Ok(index) = snapshot
        .processes
        .binary_search_by_key(&pid, |process| process.pid)
    {
        let span = snapshot.intern(value);
        snapshot.processes[index].args = span;
    }
}

/// Apply `ps -ww -o pid=,args=` output directly onto a PID-sorted snapshot.
#[cfg(test)]
pub fn apply_pid_args(text: &str, snapshot: &mut crate::model::Snapshot) {
    for line in text.lines() {
        apply_pid_args_line(line, snapshot);
    }
}

/// `ps -axww -o pid=,<column>=`: PID, then the rest of the line verbatim.
#[cfg(test)]
pub fn pid_column(text: &str) -> HashMap<i32, String> {
    text.lines()
        .filter_map(|line| {
            let (pid, rest) = parse_pid_column_line(line)?;
            Some((pid, rest.to_owned()))
        })
        .collect()
}

pub fn parse_top_line(line: &str) -> Option<(i32, u64)> {
    let mut fields = line.split_whitespace();
    // macOS `top` appends architecture markers (`+` 64-bit, `-` 32-bit, `*` Rosetta) to PIDs.
    let pid = fields
        .next()?
        .trim_end_matches(['+', '-', '*'])
        .parse()
        .ok()?;
    Some((pid, size_bytes(fields.next()?)?))
}

/// Apply one `top -l 1 ... -stats pid,mem` table row onto a PID-sorted slice.
pub fn apply_top_footprint_line(line: &str, processes: &mut [Process]) {
    let Some((pid, bytes)) = parse_top_line(line) else {
        return;
    };
    if let Ok(index) = processes.binary_search_by_key(&pid, |process| process.pid) {
        processes[index].footprint_bytes = Some(bytes);
    }
}

/// Apply `top -l 1 ... -stats pid,mem` output directly onto a PID-sorted process slice.
#[cfg(test)]
pub fn apply_top_footprints(text: &str, processes: &mut [Process]) {
    for line in text
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("PID"))
        .skip(1)
    {
        apply_top_footprint_line(line, processes);
    }
}

/// `top -l 1 -stats pid,mem`: the MEM column is the physical footprint.
#[cfg(test)]
pub fn top_footprints(text: &str) -> HashMap<i32, u64> {
    text.lines()
        .skip_while(|line| !line.trim_start().starts_with("PID"))
        .skip(1)
        .filter_map(parse_top_line)
        .collect()
}

/// `2176K`, `98M+`, `1.5G-`, `6144.00M` as bytes.
fn size_bytes(text: &str) -> Option<u64> {
    let text = text.trim_end_matches(['+', '-']);
    let unit = text.chars().last()?;
    let multiplier: f64 = match unit {
        'B' => 1.0,
        'K' => 1024.0,
        'M' => 1024.0 * 1024.0,
        'G' => 1024.0 * 1024.0 * 1024.0,
        'T' => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return text.parse().ok(),
    };
    let value: f64 = text[..text.len() - unit.len_utf8()].parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some((value * multiplier) as u64)
}

/// `sysctl -n kern.memorystatus_vm_pressure_level`: 1 normal, 2 warn, 4 critical.
pub fn pressure(text: &str) -> Option<Pressure> {
    match text.trim() {
        "1" => Some(Pressure::Normal),
        "2" => Some(Pressure::Warning),
        "4" => Some(Pressure::Critical),
        _ => None,
    }
}

/// `sysctl -n vm.swapusage`: `total = 6144.00M  used = 4539.81M  free = …`.
pub fn swap(text: &str) -> Option<Swap> {
    let value = |key: &str| {
        let after = text.split(key).nth(1)?.trim_start().strip_prefix('=')?;
        size_bytes(after.split_whitespace().next()?)
    };
    Some(Swap {
        used_bytes: value("used")?,
        total_bytes: value("total")?,
    })
}

/// Parse one line of `sysctl kern.memorystatus_vm_pressure_level vm.swapusage`.
pub fn sysctl_line(line: &str, level: &mut Pressure, swap_usage: &mut Swap) {
    if let Some(rest) = line.strip_prefix("kern.memorystatus_vm_pressure_level:") {
        if let Some(parsed) = pressure(rest) {
            *level = parsed;
        }
    } else if let Some(rest) = line.strip_prefix("vm.swapusage:")
        && let Some(parsed) = swap(rest)
    {
        *swap_usage = parsed;
    }
}

/// Combined `sysctl kern.memorystatus_vm_pressure_level vm.swapusage` output.
#[cfg(test)]
pub fn sysctl_pressure_and_swap(text: &str) -> (Pressure, Swap) {
    let mut level = Pressure::default();
    let mut swap_usage = Swap::default();
    for line in text.lines() {
        sysctl_line(line, &mut level, &mut swap_usage);
    }
    (level, swap_usage)
}

/// Parse one line of `ioreg` output for `"HIDIdleTime" = <nanoseconds>`.
pub fn hid_idle_secs_line(line: &str) -> Option<u64> {
    if !line.contains("\"HIDIdleTime\"") {
        return None;
    }
    let nanos: u64 = line.rsplit('=').next()?.trim().parse().ok()?;
    Some(nanos / 1_000_000_000)
}

/// `ioreg -r -c IOHIDSystem -d 1`: `"HIDIdleTime" = <nanoseconds>`.
#[cfg(test)]
pub fn hid_idle_secs(text: &str) -> Option<u64> {
    text.lines().find_map(hid_idle_secs_line)
}

/// Parse one line of `launchctl list`: `PID Status Label`.
pub fn launchd_pid_line(line: &str) -> Option<i32> {
    line.split_whitespace().next()?.parse().ok()
}

/// `launchctl list`: `PID Status Label`, PID `-` when the job is not running.
#[cfg(test)]
pub fn launchd_pids(text: &str) -> Vec<i32> {
    let mut pids: Vec<i32> = text.lines().filter_map(launchd_pid_line).collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

// ---------------------------------------------------------------------------
// Stack-buffered zero-MALLOC_SMALL streaming readers for MCP config files
// ---------------------------------------------------------------------------

const MAX_NESTING_DEPTH: usize = 128;

/// Fixed-size stack-buffered byte reader so streaming `~/.codex/config.toml`
/// and `~/.claude.json` never allocates a `BufReader` buffer in `MALLOC_SMALL`.
struct StackReader<R, const N: usize> {
    inner: R,
    buf: [u8; N],
    pos: usize,
    cap: usize,
}

impl<R: Read, const N: usize> StackReader<R, N> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            buf: [0u8; N],
            pos: 0,
            cap: 0,
        }
    }

    fn fill(&mut self) -> Result<bool, ()> {
        self.pos = 0;
        self.cap = 0;
        loop {
            match self.inner.read(&mut self.buf) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.cap = n;
                    return Ok(true);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(()),
            }
        }
    }

    #[inline]
    fn peek(&mut self) -> Result<Option<u8>, ()> {
        if self.pos < self.cap {
            return Ok(Some(self.buf[self.pos]));
        }
        if self.fill()? {
            Ok(Some(self.buf[self.pos]))
        } else {
            Ok(None)
        }
    }

    #[inline]
    fn next_byte(&mut self) -> Result<Option<u8>, ()> {
        let byte = self.peek()?;
        if byte.is_some() {
            self.pos += 1;
        }
        Ok(byte)
    }

    #[inline]
    fn consume(&mut self) {
        self.pos += 1;
    }

    #[inline]
    fn buffered(&self) -> &[u8] {
        &self.buf[self.pos..self.cap]
    }

    #[inline]
    fn advance(&mut self, count: usize) {
        self.pos += count;
    }

    fn expect(&mut self, expected: u8) -> Result<(), ()> {
        match self.next_byte()? {
            Some(b) if b == expected => Ok(()),
            _ => Err(()),
        }
    }

    fn expect_bytes(&mut self, expected: &[u8]) -> Result<(), ()> {
        for &byte in expected {
            self.expect(byte)?;
        }
        Ok(())
    }
}

/// Sink for decoded string characters: either discard (zero allocation), match
/// against a short fixed-size stack buffer (zero allocation), or collect into a
/// `CompactString` (inline up to 24 bytes).
enum StrSink<'a> {
    Discard,
    Stack {
        buf: [u8; 16],
        len: usize,
        overflow: bool,
    },
    Collect(&'a mut CompactString),
}

impl StrSink<'_> {
    fn stack() -> Self {
        Self::Stack {
            buf: [0u8; 16],
            len: 0,
            overflow: false,
        }
    }

    #[inline]
    fn push_byte(&mut self, byte: u8) {
        match self {
            Self::Discard => {}
            Self::Stack { buf, len, overflow } => {
                if *len < buf.len() {
                    buf[*len] = byte;
                    *len += 1;
                } else {
                    *overflow = true;
                }
            }
            Self::Collect(s) => s.push(byte as char),
        }
    }

    #[inline]
    fn push_ascii_bytes(&mut self, bytes: &[u8]) {
        match self {
            Self::Discard => {}
            Self::Stack { buf, len, overflow } => {
                if !*overflow && *len + bytes.len() <= buf.len() {
                    buf[*len..*len + bytes.len()].copy_from_slice(bytes);
                    *len += bytes.len();
                } else {
                    *overflow = true;
                }
            }
            Self::Collect(s) => {
                if let Ok(text) = std::str::from_utf8(bytes) {
                    s.push_str(text);
                }
            }
        }
    }

    #[inline]
    fn push_str(&mut self, text: &str) {
        match self {
            Self::Discard => {}
            Self::Stack { buf, len, overflow } => {
                let bytes = text.as_bytes();
                if !*overflow && *len + bytes.len() <= buf.len() {
                    buf[*len..*len + bytes.len()].copy_from_slice(bytes);
                    *len += bytes.len();
                } else {
                    *overflow = true;
                }
            }
            Self::Collect(s) => s.push_str(text),
        }
    }

    #[inline]
    fn push_char(&mut self, ch: char) {
        let mut encoded = [0u8; 4];
        self.push_str(ch.encode_utf8(&mut encoded));
    }

    fn as_stack_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Stack {
                buf,
                len,
                overflow: false,
            } => Some(&buf[..*len]),
            _ => None,
        }
    }
}

fn read_utf8_sequence<R: Read, const N: usize>(
    reader: &mut StackReader<R, N>,
    first: u8,
    sink: &mut StrSink<'_>,
) -> Result<(), ()> {
    let width = if first & 0b1110_0000 == 0b1100_0000 {
        2
    } else if first & 0b1111_0000 == 0b1110_0000 {
        3
    } else if first & 0b1111_1000 == 0b1111_0000 {
        4
    } else {
        return Err(());
    };
    let mut seq = [first, 0, 0, 0];
    for slot in &mut seq[1..width] {
        *slot = reader.next_byte()?.ok_or(())?;
    }
    let text = std::str::from_utf8(&seq[..width]).map_err(|_| ())?;
    sink.push_str(text);
    Ok(())
}

fn read_hex_u32<R: Read, const N: usize>(
    reader: &mut StackReader<R, N>,
    digits: usize,
) -> Result<u32, ()> {
    let mut value = 0u32;
    for _ in 0..digits {
        let byte = reader.next_byte()?.ok_or(())?;
        let nibble = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'a'..=b'f' => u32::from(byte - b'a' + 10),
            b'A'..=b'F' => u32::from(byte - b'A' + 10),
            _ => return Err(()),
        };
        value = (value << 4) | nibble;
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// Streaming TOML extractor for `~/.codex/config.toml`
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum TableState {
    /// Not yet defined explicitly; may have been created implicitly as a
    /// super-table of a subtable header such as `[mcp_servers.foo.env]`.
    Implicit,
    /// Opened explicitly by a `[...]` table header.
    Explicit,
    /// Defined via dotted keys (`a.command = ...`); cannot be reopened by `[a]`.
    Dotted,
    /// Defined as an inline table (`a = { ... }`); immutable per TOML spec.
    Inline,
}

struct PartialServer {
    name: CompactString,
    state: TableState,
    command: Option<CompactString>,
    args: Option<SmallVec<[CompactString; 2]>>,
}

#[derive(Clone, Copy)]
enum TomlScope {
    Root,
    McpRoot,
    McpServer(usize),
    Other,
}

enum TomlKeyTarget {
    McpRoot,
    McpServer(CompactString),
    ServerCommand(usize),
    ServerArgs(usize),
    ServerSubtable,
    Ignore,
}

struct TomlParser<R> {
    reader: StackReader<R, 4096>,
    mcp_root_state: TableState,
    servers: Vec<PartialServer>,
    project_paths: Option<Vec<String>>,
}

impl<R: Read> TomlParser<R> {
    fn new(inner: R, collect_projects: bool) -> Self {
        Self {
            reader: StackReader::new(inner),
            mcp_root_state: TableState::Implicit,
            servers: Vec::new(),
            project_paths: collect_projects.then(Vec::new),
        }
    }

    fn server_index(&mut self, name: &str) -> usize {
        if let Some(pos) = self.servers.iter().position(|s| s.name == name) {
            return pos;
        }
        let pos = self.servers.len();
        self.servers.push(PartialServer {
            name: CompactString::from(name),
            state: TableState::Implicit,
            command: None,
            args: None,
        });
        pos
    }

    fn skip_hspace(&mut self) -> Result<(), ()> {
        while let Some(b' ' | b'\t') = self.reader.peek()? {
            self.reader.consume();
        }
        Ok(())
    }

    fn skip_comment_to_eol(&mut self) -> Result<(), ()> {
        loop {
            let chunk = self.reader.buffered();
            let chunk_len = chunk.len();
            if chunk_len > 0 {
                let stop = memchr::memchr2(b'\n', b'\r', chunk).unwrap_or(chunk_len);
                let run = &chunk[..stop];
                let ascii_end = run
                    .iter()
                    .position(|&b| (b < 0x20 && b != b'\t') || b >= 0x7F)
                    .unwrap_or(run.len());
                self.reader.advance(ascii_end);
                if ascii_end == stop {
                    if stop < chunk_len {
                        let delim = self.reader.buffered()[0];
                        if delim == b'\n' {
                            return Ok(());
                        }
                        self.reader.consume();
                        if self.reader.peek()? == Some(b'\n') {
                            return Ok(());
                        }
                        return Err(());
                    }
                    continue;
                }
                let byte = self.reader.buffered()[0];
                self.reader.consume();
                if byte >= 0x80 {
                    read_utf8_sequence(&mut self.reader, byte, &mut StrSink::Discard)?;
                    continue;
                }
                return Err(());
            }
            if !self.reader.fill()? {
                return Ok(());
            }
        }
    }

    /// Skip whitespace, newlines, and comments (used between top-level items
    /// and inside arrays / multiline inline tables).
    fn skip_ws_and_comments(&mut self) -> Result<(), ()> {
        while let Some(byte) = self.reader.peek()? {
            match byte {
                b' ' | b'\t' | b'\n' => self.reader.consume(),
                b'\r' => {
                    self.reader.consume();
                    self.reader.expect(b'\n')?;
                }
                b'#' => {
                    self.reader.consume();
                    self.skip_comment_to_eol()?;
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn expect_line_end(&mut self) -> Result<(), ()> {
        self.skip_hspace()?;
        match self.reader.peek()? {
            None => Ok(()),
            Some(b'\n') => {
                self.reader.consume();
                Ok(())
            }
            Some(b'\r') => {
                self.reader.consume();
                self.reader.expect(b'\n')
            }
            Some(b'#') => {
                self.reader.consume();
                self.skip_comment_to_eol()?;
                if self.reader.peek()? == Some(b'\n') {
                    self.reader.consume();
                }
                Ok(())
            }
            Some(_) => Err(()),
        }
    }

    fn parse_escape(&mut self, sink: &mut StrSink<'_>, allow_line_trim: bool) -> Result<(), ()> {
        let esc = self.reader.next_byte()?.ok_or(())?;
        match esc {
            b'b' => sink.push_byte(0x08),
            b't' => sink.push_byte(b'\t'),
            b'n' => sink.push_byte(b'\n'),
            b'f' => sink.push_byte(0x0C),
            b'r' => sink.push_byte(b'\r'),
            b'e' => sink.push_byte(0x1B),
            b'"' => sink.push_byte(b'"'),
            b'\\' => sink.push_byte(b'\\'),
            b'x' => {
                let code = read_hex_u32(&mut self.reader, 2)?;
                sink.push_char(char::from_u32(code).ok_or(())?);
            }
            b'u' => {
                let code = read_hex_u32(&mut self.reader, 4)?;
                sink.push_char(char::from_u32(code).ok_or(())?);
            }
            b'U' => {
                let code = read_hex_u32(&mut self.reader, 8)?;
                sink.push_char(char::from_u32(code).ok_or(())?);
            }
            b' ' | b'\t' | b'\r' | b'\n' if allow_line_trim => {
                // Line-ending backslash: require a newline after optional spaces/tabs.
                let mut saw_newline = esc == b'\n';
                if esc == b'\r' {
                    self.reader.expect(b'\n')?;
                    saw_newline = true;
                }
                while let Some(byte) = self.reader.peek()? {
                    match byte {
                        b' ' | b'\t' => self.reader.consume(),
                        b'\n' => {
                            self.reader.consume();
                            saw_newline = true;
                        }
                        b'\r' => {
                            self.reader.consume();
                            self.reader.expect(b'\n')?;
                            saw_newline = true;
                        }
                        _ => break,
                    }
                }
                if !saw_newline {
                    return Err(());
                }
            }
            _ => return Err(()),
        }
        Ok(())
    }

    fn parse_basic_string_body(&mut self, sink: &mut StrSink<'_>) -> Result<(), ()> {
        loop {
            let chunk = self.reader.buffered();
            let chunk_len = chunk.len();
            if chunk_len > 0 {
                let stop = memchr::memchr2(b'"', b'\\', chunk).unwrap_or(chunk_len);
                let run = &chunk[..stop];
                let ascii_end = run
                    .iter()
                    .position(|&b| (b < 0x20 && b != b'\t') || b >= 0x7F)
                    .unwrap_or(run.len());
                if ascii_end > 0 {
                    sink.push_ascii_bytes(&run[..ascii_end]);
                    self.reader.advance(ascii_end);
                }
                if ascii_end == stop {
                    if stop < chunk_len {
                        let delim = self.reader.buffered()[0];
                        self.reader.consume();
                        if delim == b'"' {
                            return Ok(());
                        }
                        self.parse_escape(sink, false)?;
                    }
                    continue;
                }
                let byte = self.reader.buffered()[0];
                self.reader.consume();
                if byte >= 0x80 {
                    read_utf8_sequence(&mut self.reader, byte, sink)?;
                    continue;
                }
                return Err(());
            }
            if !self.reader.fill()? {
                return Err(());
            }
        }
    }

    fn parse_ml_basic_string_body(&mut self, sink: &mut StrSink<'_>) -> Result<(), ()> {
        // Trim one newline immediately after opening `"""`.
        match self.reader.peek()? {
            Some(b'\n') => self.reader.consume(),
            Some(b'\r') => {
                self.reader.consume();
                self.reader.expect(b'\n')?;
            }
            _ => {}
        }
        loop {
            let chunk = self.reader.buffered();
            let chunk_len = chunk.len();
            if chunk_len > 0 {
                let stop = memchr::memchr3(b'"', b'\\', b'\r', chunk).unwrap_or(chunk_len);
                let run = &chunk[..stop];
                let ascii_end = run
                    .iter()
                    .position(|&b| (b < 0x20 && !matches!(b, b'\t' | b'\n')) || b >= 0x7F)
                    .unwrap_or(run.len());
                if ascii_end > 0 {
                    sink.push_ascii_bytes(&run[..ascii_end]);
                    self.reader.advance(ascii_end);
                }
                if ascii_end == stop {
                    if stop < chunk_len {
                        let delim = self.reader.buffered()[0];
                        self.reader.consume();
                        match delim {
                            b'"' => {
                                let mut quotes = 1usize;
                                while self.reader.peek()? == Some(b'"') {
                                    self.reader.consume();
                                    quotes += 1;
                                    if quotes > 5 {
                                        return Err(());
                                    }
                                }
                                match quotes {
                                    1 => sink.push_byte(b'"'),
                                    2 => sink.push_str("\"\""),
                                    3 => return Ok(()),
                                    4 => {
                                        sink.push_byte(b'"');
                                        return Ok(());
                                    }
                                    5 => {
                                        sink.push_str("\"\"");
                                        return Ok(());
                                    }
                                    _ => return Err(()),
                                }
                            }
                            b'\\' => self.parse_escape(sink, true)?,
                            b'\r' => {
                                self.reader.expect(b'\n')?;
                                sink.push_str("\r\n");
                            }
                            _ => unreachable!(),
                        }
                    }
                    continue;
                }
                let byte = self.reader.buffered()[0];
                self.reader.consume();
                if byte >= 0x80 {
                    read_utf8_sequence(&mut self.reader, byte, sink)?;
                    continue;
                }
                return Err(());
            }
            if !self.reader.fill()? {
                return Err(());
            }
        }
    }

    fn parse_literal_string_body(&mut self, sink: &mut StrSink<'_>) -> Result<(), ()> {
        loop {
            let chunk = self.reader.buffered();
            let chunk_len = chunk.len();
            if chunk_len > 0 {
                let stop = memchr::memchr(b'\'', chunk).unwrap_or(chunk_len);
                let run = &chunk[..stop];
                let ascii_end = run
                    .iter()
                    .position(|&b| (b < 0x20 && b != b'\t') || b >= 0x7F)
                    .unwrap_or(run.len());
                if ascii_end > 0 {
                    sink.push_ascii_bytes(&run[..ascii_end]);
                    self.reader.advance(ascii_end);
                }
                if ascii_end == stop {
                    if stop < chunk_len {
                        self.reader.consume();
                        return Ok(());
                    }
                    continue;
                }
                let byte = self.reader.buffered()[0];
                self.reader.consume();
                if byte >= 0x80 {
                    read_utf8_sequence(&mut self.reader, byte, sink)?;
                    continue;
                }
                return Err(());
            }
            if !self.reader.fill()? {
                return Err(());
            }
        }
    }

    fn parse_ml_literal_string_body(&mut self, sink: &mut StrSink<'_>) -> Result<(), ()> {
        // Trim one newline immediately after opening `'''`.
        match self.reader.peek()? {
            Some(b'\n') => self.reader.consume(),
            Some(b'\r') => {
                self.reader.consume();
                self.reader.expect(b'\n')?;
            }
            _ => {}
        }
        loop {
            let chunk = self.reader.buffered();
            let chunk_len = chunk.len();
            if chunk_len > 0 {
                let stop = memchr::memchr2(b'\'', b'\r', chunk).unwrap_or(chunk_len);
                let run = &chunk[..stop];
                let ascii_end = run
                    .iter()
                    .position(|&b| (b < 0x20 && !matches!(b, b'\t' | b'\n')) || b >= 0x7F)
                    .unwrap_or(run.len());
                if ascii_end > 0 {
                    sink.push_ascii_bytes(&run[..ascii_end]);
                    self.reader.advance(ascii_end);
                }
                if ascii_end == stop {
                    if stop < chunk_len {
                        let delim = self.reader.buffered()[0];
                        self.reader.consume();
                        if delim == b'\'' {
                            let mut quotes = 1usize;
                            while self.reader.peek()? == Some(b'\'') {
                                self.reader.consume();
                                quotes += 1;
                                if quotes > 5 {
                                    return Err(());
                                }
                            }
                            match quotes {
                                1 => sink.push_byte(b'\''),
                                2 => sink.push_str("''"),
                                3 => return Ok(()),
                                4 => {
                                    sink.push_byte(b'\'');
                                    return Ok(());
                                }
                                5 => {
                                    sink.push_str("''");
                                    return Ok(());
                                }
                                _ => return Err(()),
                            }
                        } else {
                            self.reader.expect(b'\n')?;
                            sink.push_str("\r\n");
                        }
                    }
                    continue;
                }
                let byte = self.reader.buffered()[0];
                self.reader.consume();
                if byte >= 0x80 {
                    read_utf8_sequence(&mut self.reader, byte, sink)?;
                    continue;
                }
                return Err(());
            }
            if !self.reader.fill()? {
                return Err(());
            }
        }
    }

    /// Parse any TOML string value (`"..."`, `"""..."""`, `'...'`, `'''...'''`).
    fn parse_string_value(&mut self, sink: &mut StrSink<'_>) -> Result<(), ()> {
        match self.reader.next_byte()? {
            Some(b'"') => {
                if self.reader.peek()? == Some(b'"') {
                    self.reader.consume();
                    if self.reader.peek()? == Some(b'"') {
                        self.reader.consume();
                        self.parse_ml_basic_string_body(sink)
                    } else {
                        // Empty basic string `""`.
                        Ok(())
                    }
                } else {
                    self.parse_basic_string_body(sink)
                }
            }
            Some(b'\'') => {
                if self.reader.peek()? == Some(b'\'') {
                    self.reader.consume();
                    if self.reader.peek()? == Some(b'\'') {
                        self.reader.consume();
                        self.parse_ml_literal_string_body(sink)
                    } else {
                        // Empty literal string `''`.
                        Ok(())
                    }
                } else {
                    self.parse_literal_string_body(sink)
                }
            }
            _ => Err(()),
        }
    }

    fn parse_key_segment(&mut self, sink: &mut StrSink<'_>) -> Result<(), ()> {
        self.skip_hspace()?;
        match self.reader.peek()? {
            Some(b'"') => {
                self.reader.consume();
                self.parse_basic_string_body(sink)
            }
            Some(b'\'') => {
                self.reader.consume();
                self.parse_literal_string_body(sink)
            }
            Some(b) if is_bare_key_byte(b) => {
                while let Some(byte) = self.reader.peek()? {
                    if is_bare_key_byte(byte) {
                        self.reader.consume();
                        sink.push_byte(byte);
                    } else {
                        break;
                    }
                }
                Ok(())
            }
            _ => Err(()),
        }
    }

    fn skip_rest_of_key_path(&mut self) -> Result<(), ()> {
        loop {
            self.skip_hspace()?;
            if self.reader.peek()? != Some(b'.') {
                return Ok(());
            }
            self.reader.consume();
            self.parse_key_segment(&mut StrSink::Discard)?;
        }
    }

    fn parse_key_path(&mut self, scope: TomlScope, is_header: bool) -> Result<TomlKeyTarget, ()> {
        match scope {
            TomlScope::Other => {
                self.parse_key_segment(&mut StrSink::Discard)?;
                self.skip_rest_of_key_path()?;
                Ok(TomlKeyTarget::Ignore)
            }
            TomlScope::Root => {
                let mut seg0 = StrSink::stack();
                self.parse_key_segment(&mut seg0)?;
                if seg0.as_stack_bytes() == Some(b"projects")
                    && is_header
                    && self.project_paths.is_some()
                {
                    self.skip_hspace()?;
                    if self.reader.peek()? == Some(b'.') {
                        self.reader.consume();
                        let mut project = CompactString::default();
                        self.parse_key_segment(&mut StrSink::Collect(&mut project))?;
                        if let Some(paths) = self.project_paths.as_mut()
                            && paths.len() < MAX_DISCOVERED_PATHS
                            && project.starts_with('/')
                            && !paths.iter().any(|existing| existing == project.as_str())
                        {
                            paths.push(project.to_string());
                        }
                        self.skip_rest_of_key_path()?;
                    }
                    return Ok(TomlKeyTarget::Ignore);
                }
                if seg0.as_stack_bytes() != Some(b"mcp_servers") {
                    self.skip_rest_of_key_path()?;
                    return Ok(TomlKeyTarget::Ignore);
                }
                self.skip_hspace()?;
                if self.reader.peek()? != Some(b'.') {
                    return Ok(TomlKeyTarget::McpRoot);
                }
                self.reader.consume();
                if !is_header {
                    if matches!(
                        self.mcp_root_state,
                        TableState::Inline | TableState::Explicit
                    ) {
                        return Err(());
                    }
                    self.mcp_root_state = TableState::Dotted;
                } else if self.mcp_root_state == TableState::Inline {
                    return Err(());
                }
                self.parse_key_path_under_mcp_root(is_header)
            }
            TomlScope::McpRoot => self.parse_key_path_under_mcp_root(is_header),
            TomlScope::McpServer(idx) => self.parse_key_path_under_server(idx, is_header),
        }
    }

    fn parse_key_path_under_mcp_root(&mut self, is_header: bool) -> Result<TomlKeyTarget, ()> {
        let mut name = CompactString::default();
        self.parse_key_segment(&mut StrSink::Collect(&mut name))?;
        self.skip_hspace()?;
        if self.reader.peek()? != Some(b'.') {
            return Ok(TomlKeyTarget::McpServer(name));
        }
        self.reader.consume();
        let idx = self.server_index(&name);
        if is_header {
            if self.servers[idx].state == TableState::Inline {
                return Err(());
            }
        } else {
            match self.servers[idx].state {
                TableState::Inline | TableState::Explicit => return Err(()),
                TableState::Implicit => self.servers[idx].state = TableState::Dotted,
                TableState::Dotted => {}
            }
        }
        self.parse_key_path_under_server(idx, is_header)
    }

    fn parse_key_path_under_server(
        &mut self,
        idx: usize,
        is_header: bool,
    ) -> Result<TomlKeyTarget, ()> {
        let mut field = StrSink::stack();
        self.parse_key_segment(&mut field)?;
        self.skip_hspace()?;
        if self.reader.peek()? == Some(b'.') {
            if matches!(field.as_stack_bytes(), Some(b"command" | b"args")) {
                return Err(());
            }
            self.skip_rest_of_key_path()?;
            return Ok(if is_header {
                TomlKeyTarget::ServerSubtable
            } else {
                TomlKeyTarget::Ignore
            });
        }
        match field.as_stack_bytes() {
            Some(b"command") => Ok(TomlKeyTarget::ServerCommand(idx)),
            Some(b"args") => Ok(TomlKeyTarget::ServerArgs(idx)),
            _ => Ok(if is_header {
                TomlKeyTarget::ServerSubtable
            } else {
                TomlKeyTarget::Ignore
            }),
        }
    }

    fn parse_string_array(&mut self) -> Result<SmallVec<[CompactString; 2]>, ()> {
        self.reader.expect(b'[')?;
        let mut items = SmallVec::new();
        loop {
            self.skip_ws_and_comments()?;
            if self.reader.peek()? == Some(b']') {
                self.reader.consume();
                return Ok(items);
            }
            let mut value = CompactString::default();
            self.parse_string_value(&mut StrSink::Collect(&mut value))?;
            items.push(value);
            self.skip_ws_and_comments()?;
            match self.reader.peek()? {
                Some(b',') => self.reader.consume(),
                Some(b']') => {
                    self.reader.consume();
                    return Ok(items);
                }
                _ => return Err(()),
            }
        }
    }

    fn parse_inline_table(&mut self, scope: TomlScope, depth: usize) -> Result<(), ()> {
        if depth == 0 {
            return Err(());
        }
        self.reader.expect(b'{')?;
        loop {
            self.skip_ws_and_comments()?;
            if self.reader.peek()? == Some(b'}') {
                self.reader.consume();
                return Ok(());
            }
            let target = self.parse_key_path(scope, false)?;
            self.skip_hspace()?;
            self.reader.expect(b'=')?;
            self.skip_hspace()?;
            self.parse_value_for(target, depth - 1)?;
            self.skip_ws_and_comments()?;
            match self.reader.peek()? {
                Some(b',') => self.reader.consume(),
                Some(b'}') => {
                    self.reader.consume();
                    return Ok(());
                }
                _ => return Err(()),
            }
        }
    }

    fn parse_value_for(&mut self, target: TomlKeyTarget, depth: usize) -> Result<(), ()> {
        match target {
            TomlKeyTarget::McpRoot => {
                if self.mcp_root_state != TableState::Implicit || !self.servers.is_empty() {
                    return Err(());
                }
                self.mcp_root_state = TableState::Inline;
                self.parse_inline_table(TomlScope::McpRoot, depth)
            }
            TomlKeyTarget::McpServer(name) => {
                if self.servers.iter().any(|s| s.name == name) {
                    return Err(());
                }
                let idx = self.servers.len();
                self.servers.push(PartialServer {
                    name,
                    state: TableState::Inline,
                    command: None,
                    args: None,
                });
                self.parse_inline_table(TomlScope::McpServer(idx), depth)
            }
            TomlKeyTarget::ServerCommand(idx) => {
                if self.servers[idx].command.is_some() {
                    return Err(());
                }
                let mut cmd = CompactString::default();
                self.parse_string_value(&mut StrSink::Collect(&mut cmd))?;
                self.servers[idx].command = Some(cmd);
                Ok(())
            }
            TomlKeyTarget::ServerArgs(idx) => {
                if self.servers[idx].args.is_some() {
                    return Err(());
                }
                let args = self.parse_string_array()?;
                self.servers[idx].args = Some(args);
                Ok(())
            }
            TomlKeyTarget::ServerSubtable | TomlKeyTarget::Ignore => self.skip_value(depth),
        }
    }

    fn skip_value(&mut self, depth: usize) -> Result<(), ()> {
        if depth == 0 {
            return Err(());
        }
        match self.reader.peek()? {
            Some(b'"' | b'\'') => self.parse_string_value(&mut StrSink::Discard),
            Some(b'[') => {
                self.reader.consume();
                loop {
                    self.skip_ws_and_comments()?;
                    if self.reader.peek()? == Some(b']') {
                        self.reader.consume();
                        return Ok(());
                    }
                    self.skip_value(depth - 1)?;
                    self.skip_ws_and_comments()?;
                    match self.reader.peek()? {
                        Some(b',') => self.reader.consume(),
                        Some(b']') => {
                            self.reader.consume();
                            return Ok(());
                        }
                        _ => return Err(()),
                    }
                }
            }
            Some(b'{') => self.parse_inline_table(TomlScope::Other, depth - 1),
            Some(b) if is_bare_scalar_start(b) => {
                let mut buf = [0u8; 128];
                let mut len = 1usize;
                buf[0] = b;
                self.reader.consume();
                let mut saw_dash = b == b'-';
                let mut saw_space = false;
                while let Some(byte) = self.reader.peek()? {
                    if is_bare_scalar_cont(byte) {
                        if byte == b'-' {
                            saw_dash = true;
                        }
                        self.reader.consume();
                        if len < buf.len() {
                            buf[len] = byte;
                            len += 1;
                        } else {
                            return Err(());
                        }
                    } else if byte == b' ' && saw_dash && !saw_space {
                        // RFC 3339 local date-time allows a single space between date and time.
                        self.reader.consume();
                        saw_space = true;
                        if self
                            .reader
                            .peek()?
                            .is_some_and(|next| next.is_ascii_digit())
                        {
                            if len < buf.len() {
                                buf[len] = b' ';
                                len += 1;
                            } else {
                                return Err(());
                            }
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                if validate_toml_bare_scalar(&buf[..len]) {
                    Ok(())
                } else {
                    Err(())
                }
            }
            _ => Err(()),
        }
    }

    fn parse_document(mut self) -> Result<(Vec<ServerSignature>, Vec<String>), ()> {
        // Optional UTF-8 BOM at the beginning of the file.
        if self.reader.peek()? == Some(0xEF) {
            self.reader.consume();
            self.reader.expect(0xBB)?;
            self.reader.expect(0xBF)?;
        }

        let mut scope = TomlScope::Root;
        loop {
            self.skip_ws_and_comments()?;
            let Some(byte) = self.reader.peek()? else {
                break;
            };
            if byte == b'[' {
                self.reader.consume();
                let is_array = self.reader.peek()? == Some(b'[');
                if is_array {
                    self.reader.consume();
                }
                let target = self.parse_key_path(TomlScope::Root, true)?;
                self.skip_hspace()?;
                self.reader.expect(b']')?;
                if is_array {
                    self.reader.expect(b']')?;
                }
                self.expect_line_end()?;
                scope = match (is_array, target) {
                    (false, TomlKeyTarget::McpRoot) => {
                        if self.mcp_root_state != TableState::Implicit {
                            return Err(());
                        }
                        self.mcp_root_state = TableState::Explicit;
                        TomlScope::McpRoot
                    }
                    (false, TomlKeyTarget::McpServer(name)) => {
                        let idx = self.server_index(&name);
                        if self.servers[idx].state != TableState::Implicit {
                            return Err(());
                        }
                        self.servers[idx].state = TableState::Explicit;
                        TomlScope::McpServer(idx)
                    }
                    (false | true, TomlKeyTarget::ServerSubtable)
                    | (false | true, TomlKeyTarget::Ignore) => TomlScope::Other,
                    _ => return Err(()),
                };
            } else {
                let target = self.parse_key_path(scope, false)?;
                self.skip_hspace()?;
                self.reader.expect(b'=')?;
                self.skip_hspace()?;
                self.parse_value_for(target, MAX_NESTING_DEPTH)?;
                self.expect_line_end()?;
            }
        }

        let collected = self
            .servers
            .into_iter()
            .filter_map(|server| {
                let command = server.command.filter(|cmd| !cmd.is_empty())?;
                Some(ServerSignature {
                    command,
                    args: server.args.unwrap_or_default(),
                })
            })
            .collect();
        Ok((
            dedup_signatures(collected),
            self.project_paths.unwrap_or_default(),
        ))
    }
}

fn is_bare_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn is_bare_scalar_start(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-')
}

fn is_bare_scalar_cont(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+' | b'.' | b':')
}

fn valid_digit_run(bytes: &[u8], is_digit: fn(u8) -> bool) -> bool {
    if bytes.is_empty() || !is_digit(bytes[0]) || !is_digit(bytes[bytes.len() - 1]) {
        return false;
    }
    let mut prev_underscore = false;
    for &b in bytes {
        if b == b'_' {
            if prev_underscore {
                return false;
            }
            prev_underscore = true;
        } else if is_digit(b) {
            prev_underscore = false;
        } else {
            return false;
        }
    }
    true
}

fn parse_two_digits(bytes: &[u8]) -> Option<u8> {
    if bytes.len() == 2 && bytes[0].is_ascii_digit() && bytes[1].is_ascii_digit() {
        Some((bytes[0] - b'0') * 10 + (bytes[1] - b'0'))
    } else {
        None
    }
}

fn valid_toml_date(bytes: &[u8]) -> bool {
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if !bytes[..4].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let year: u16 = u16::from(bytes[0] - b'0') * 1000
        + u16::from(bytes[1] - b'0') * 100
        + u16::from(bytes[2] - b'0') * 10
        + u16::from(bytes[3] - b'0');
    let Some(month) = parse_two_digits(&bytes[5..7]) else {
        return false;
    };
    let Some(day) = parse_two_digits(&bytes[8..10]) else {
        return false;
    };
    let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return false,
    };
    day >= 1 && day <= max_day
}

fn valid_toml_time(bytes: &[u8]) -> bool {
    if bytes.len() < 5 || bytes[2] != b':' {
        return false;
    }
    let Some(hour) = parse_two_digits(&bytes[0..2]) else {
        return false;
    };
    let Some(minute) = parse_two_digits(&bytes[3..5]) else {
        return false;
    };
    if hour > 23 || minute > 59 {
        return false;
    }
    let rest = &bytes[5..];
    if rest.is_empty() {
        // TOML 1.1 allows optional seconds (`HH:MM`).
        return true;
    }
    if rest.len() < 3 || rest[0] != b':' {
        return false;
    }
    let Some(sec) = parse_two_digits(&rest[1..3]) else {
        return false;
    };
    if sec > 60 {
        return false;
    }
    let frac = &rest[3..];
    if frac.is_empty() {
        return true;
    }
    frac.len() >= 2 && frac[0] == b'.' && frac[1..].iter().all(u8::is_ascii_digit)
}

fn valid_toml_datetime_or_time(bytes: &[u8]) -> bool {
    // Local time (`HH:MM...`).
    if bytes.len() >= 5 && bytes[2] == b':' {
        return valid_toml_time(bytes);
    }
    // Date (`YYYY-MM-DD`) or date-time (`YYYY-MM-DD[Tt ]<time>[offset]`).
    if bytes.len() < 10 || !valid_toml_date(&bytes[..10]) {
        return false;
    }
    if bytes.len() == 10 {
        return true;
    }
    if !matches!(bytes[10], b'T' | b't' | b' ') {
        return false;
    }
    let rest = &bytes[11..];
    // Optional trailing offset (`Z`, `z`, or `[+-]HH:MM`).
    let time = if matches!(rest.last(), Some(b'Z' | b'z')) {
        &rest[..rest.len() - 1]
    } else if rest.len() >= 6 && matches!(rest[rest.len() - 6], b'+' | b'-') {
        let offset = &rest[rest.len() - 5..];
        if offset[2] != b':' {
            return false;
        }
        let Some(oh) = parse_two_digits(&offset[0..2]) else {
            return false;
        };
        let Some(om) = parse_two_digits(&offset[3..5]) else {
            return false;
        };
        if oh > 23 || om > 59 {
            return false;
        }
        &rest[..rest.len() - 6]
    } else {
        rest
    };
    valid_toml_time(time)
}

fn validate_toml_bare_scalar(bytes: &[u8]) -> bool {
    if matches!(
        bytes,
        b"true" | b"false" | b"inf" | b"+inf" | b"-inf" | b"nan" | b"+nan" | b"-nan"
    ) {
        return true;
    }
    if bytes.len() >= 3 && bytes[0] == b'0' {
        match bytes[1] {
            b'x' => return valid_digit_run(&bytes[2..], |b| b.is_ascii_hexdigit()),
            b'o' => return valid_digit_run(&bytes[2..], |b| matches!(b, b'0'..=b'7')),
            b'b' => return valid_digit_run(&bytes[2..], |b| matches!(b, b'0' | b'1')),
            _ => {}
        }
    }
    if valid_toml_datetime_or_time(bytes) {
        return true;
    }
    // Decimal integer or float.
    let unsigned = match bytes.first() {
        Some(b'+' | b'-') => &bytes[1..],
        Some(_) => bytes,
        None => return false,
    };
    if unsigned.is_empty() {
        return false;
    }
    let (mantissa, exp) = match unsigned.iter().position(|&b| matches!(b, b'e' | b'E')) {
        Some(pos) => (&unsigned[..pos], Some(&unsigned[pos + 1..])),
        None => (unsigned, None),
    };
    let (int_part, frac_part) = match mantissa.iter().position(|&b| b == b'.') {
        Some(pos) => (&mantissa[..pos], Some(&mantissa[pos + 1..])),
        None => (mantissa, None),
    };
    let int_ok = if int_part == b"0" {
        true
    } else {
        !int_part.is_empty()
            && matches!(int_part[0], b'1'..=b'9')
            && valid_digit_run(int_part, |b| b.is_ascii_digit())
    };
    if !int_ok {
        return false;
    }
    if let Some(frac) = frac_part
        && !valid_digit_run(frac, |b| b.is_ascii_digit())
    {
        return false;
    }
    if let Some(exp_bytes) = exp {
        let exp_digits = match exp_bytes.first() {
            Some(b'+' | b'-') => &exp_bytes[1..],
            Some(_) => exp_bytes,
            None => return false,
        };
        if !valid_digit_run(exp_digits, |b| b.is_ascii_digit()) {
            return false;
        }
    }
    true
}

/// Stream stdio servers directly from `~/.codex/config.toml` using a fixed stack
/// buffer without allocating `BufReader`, an extracted TOML slice, or an AST.
pub fn codex_servers_from_reader(reader: impl Read) -> Vec<ServerSignature> {
    TomlParser::new(reader, false)
        .parse_document()
        .map_or_default(|(servers, _)| servers)
}

/// Stream stdio servers and `[projects."<path>"]` keys from `~/.codex/config.toml`
/// in a single pass.
pub fn codex_config_from_reader(reader: impl Read) -> (Vec<ServerSignature>, Vec<String>) {
    TomlParser::new(reader, true)
        .parse_document()
        .unwrap_or_default()
}

/// Stdio servers in `~/.codex/config.toml`; servers with a URL are skipped.
#[cfg(test)]
pub fn codex_servers(text: &str) -> Vec<ServerSignature> {
    codex_servers_from_reader(io::Cursor::new(text.as_bytes()))
}

// ---------------------------------------------------------------------------
// Streaming RFC 8259 JSON extractor for `~/.claude.json`
// ---------------------------------------------------------------------------

const MAX_DISCOVERED_PATHS: usize = 256;
const MAX_AUX_CONFIG_BYTES: usize = 2 << 20;

struct JsonParser<R> {
    reader: StackReader<R, 4096>,
    servers: Vec<ServerSignature>,
    project_paths: Option<Vec<String>>,
}

impl<R: Read> JsonParser<R> {
    fn new(inner: R, collect_projects: bool) -> Self {
        Self {
            reader: StackReader::new(inner),
            servers: Vec::new(),
            project_paths: collect_projects.then(Vec::new),
        }
    }

    fn skip_ws(&mut self) -> Result<(), ()> {
        while let Some(b' ' | b'\t' | b'\r' | b'\n') = self.reader.peek()? {
            self.reader.consume();
        }
        Ok(())
    }

    fn parse_json_escape(&mut self, sink: &mut StrSink<'_>) -> Result<(), ()> {
        let esc = self.reader.next_byte()?.ok_or(())?;
        match esc {
            b'"' => sink.push_byte(b'"'),
            b'\\' => sink.push_byte(b'\\'),
            b'/' => sink.push_byte(b'/'),
            b'b' => sink.push_byte(0x08),
            b'f' => sink.push_byte(0x0C),
            b'n' => sink.push_byte(b'\n'),
            b'r' => sink.push_byte(b'\r'),
            b't' => sink.push_byte(b'\t'),
            b'u' => {
                let code = read_hex_u32(&mut self.reader, 4)?;
                if (0xD800..=0xDBFF).contains(&code) {
                    // High surrogate must be paired with `\uDC00..\uDFFF`.
                    self.reader.expect(b'\\')?;
                    self.reader.expect(b'u')?;
                    let low = read_hex_u32(&mut self.reader, 4)?;
                    if !(0xDC00..=0xDFFF).contains(&low) {
                        return Err(());
                    }
                    let combined = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                    sink.push_char(char::from_u32(combined).ok_or(())?);
                } else if (0xDC00..=0xDFFF).contains(&code) {
                    return Err(());
                } else {
                    sink.push_char(char::from_u32(code).ok_or(())?);
                }
            }
            _ => return Err(()),
        }
        Ok(())
    }

    fn parse_string(&mut self, sink: &mut StrSink<'_>) -> Result<(), ()> {
        self.reader.expect(b'"')?;
        loop {
            let chunk = self.reader.buffered();
            let chunk_len = chunk.len();
            if chunk_len > 0 {
                let stop = memchr::memchr2(b'"', b'\\', chunk).unwrap_or(chunk_len);
                let run = &chunk[..stop];
                let ascii_end = run
                    .iter()
                    .position(|&b| !(0x20..=0x7F).contains(&b))
                    .unwrap_or(run.len());
                if ascii_end > 0 {
                    sink.push_ascii_bytes(&run[..ascii_end]);
                    self.reader.advance(ascii_end);
                }
                if ascii_end == stop {
                    if stop < chunk_len {
                        let delim = self.reader.buffered()[0];
                        self.reader.consume();
                        if delim == b'"' {
                            return Ok(());
                        }
                        self.parse_json_escape(sink)?;
                    }
                    continue;
                }
                let byte = self.reader.buffered()[0];
                self.reader.consume();
                if byte < 0x20 {
                    return Err(());
                }
                read_utf8_sequence(&mut self.reader, byte, sink)?;
                continue;
            }
            if !self.reader.fill()? {
                return Err(());
            }
        }
    }

    fn skip_number(&mut self) -> Result<(), ()> {
        if self.reader.peek()? == Some(b'-') {
            self.reader.consume();
        }
        match self.reader.next_byte()? {
            Some(b'0') => {}
            Some(b'1'..=b'9') => {
                while self.reader.peek()?.is_some_and(|b| b.is_ascii_digit()) {
                    self.reader.consume();
                }
            }
            _ => return Err(()),
        }
        if self.reader.peek()? == Some(b'.') {
            self.reader.consume();
            if !self.reader.next_byte()?.is_some_and(|b| b.is_ascii_digit()) {
                return Err(());
            }
            while self.reader.peek()?.is_some_and(|b| b.is_ascii_digit()) {
                self.reader.consume();
            }
        }
        if matches!(self.reader.peek()?, Some(b'e' | b'E')) {
            self.reader.consume();
            if matches!(self.reader.peek()?, Some(b'+' | b'-')) {
                self.reader.consume();
            }
            if !self.reader.next_byte()?.is_some_and(|b| b.is_ascii_digit()) {
                return Err(());
            }
            while self.reader.peek()?.is_some_and(|b| b.is_ascii_digit()) {
                self.reader.consume();
            }
        }
        Ok(())
    }

    fn skip_value(&mut self, depth: usize) -> Result<(), ()> {
        if depth == 0 {
            return Err(());
        }
        match self.reader.peek()? {
            Some(b'"') => self.parse_string(&mut StrSink::Discard),
            Some(b'{') => {
                self.reader.consume();
                self.skip_ws()?;
                if self.reader.peek()? == Some(b'}') {
                    self.reader.consume();
                    return Ok(());
                }
                loop {
                    self.parse_string(&mut StrSink::Discard)?;
                    self.skip_ws()?;
                    self.reader.expect(b':')?;
                    self.skip_ws()?;
                    self.skip_value(depth - 1)?;
                    self.skip_ws()?;
                    match self.reader.next_byte()? {
                        Some(b',') => self.skip_ws()?,
                        Some(b'}') => return Ok(()),
                        _ => return Err(()),
                    }
                }
            }
            Some(b'[') => {
                self.reader.consume();
                self.skip_ws()?;
                if self.reader.peek()? == Some(b']') {
                    self.reader.consume();
                    return Ok(());
                }
                loop {
                    self.skip_value(depth - 1)?;
                    self.skip_ws()?;
                    match self.reader.next_byte()? {
                        Some(b',') => self.skip_ws()?,
                        Some(b']') => return Ok(()),
                        _ => return Err(()),
                    }
                }
            }
            Some(b't') => self.reader.expect_bytes(b"true"),
            Some(b'f') => self.reader.expect_bytes(b"false"),
            Some(b'n') => self.reader.expect_bytes(b"null"),
            Some(b'-' | b'0'..=b'9') => self.skip_number(),
            _ => Err(()),
        }
    }

    fn parse_string_array(&mut self) -> Result<SmallVec<[CompactString; 2]>, ()> {
        self.reader.expect(b'[')?;
        self.skip_ws()?;
        let mut items = SmallVec::new();
        if self.reader.peek()? == Some(b']') {
            self.reader.consume();
            return Ok(items);
        }
        loop {
            let mut item = CompactString::default();
            self.parse_string(&mut StrSink::Collect(&mut item))?;
            items.push(item);
            self.skip_ws()?;
            match self.reader.next_byte()? {
                Some(b',') => self.skip_ws()?,
                Some(b']') => return Ok(items),
                _ => return Err(()),
            }
        }
    }

    fn parse_server_object(&mut self, depth: usize) -> Result<Option<ServerSignature>, ()> {
        if depth == 0 {
            return Err(());
        }
        self.reader.expect(b'{')?;
        self.skip_ws()?;
        let mut command: Option<Option<CompactString>> = None;
        let mut args: Option<SmallVec<[CompactString; 2]>> = None;
        if self.reader.peek()? != Some(b'}') {
            loop {
                let mut key = StrSink::stack();
                self.parse_string(&mut key)?;
                self.skip_ws()?;
                self.reader.expect(b':')?;
                self.skip_ws()?;
                match key.as_stack_bytes() {
                    Some(b"command") => {
                        if command.is_some() {
                            return Err(());
                        }
                        if self.reader.peek()? == Some(b'n') {
                            self.reader.expect_bytes(b"null")?;
                            command = Some(None);
                        } else {
                            let mut cmd = CompactString::default();
                            self.parse_string(&mut StrSink::Collect(&mut cmd))?;
                            command = Some(Some(cmd));
                        }
                    }
                    Some(b"args") => {
                        if args.is_some() {
                            return Err(());
                        }
                        args = Some(self.parse_string_array()?);
                    }
                    _ => self.skip_value(depth - 1)?,
                }
                self.skip_ws()?;
                match self.reader.next_byte()? {
                    Some(b',') => self.skip_ws()?,
                    Some(b'}') => break,
                    _ => return Err(()),
                }
            }
        } else {
            self.reader.consume();
        }
        Ok(match command {
            Some(Some(cmd)) if !cmd.is_empty() => Some(ServerSignature {
                command: cmd,
                args: args.unwrap_or_default(),
            }),
            _ => None,
        })
    }

    fn parse_servers_map(
        &mut self,
        depth: usize,
        out: &mut Vec<ServerSignature>,
    ) -> Result<(), ()> {
        if depth == 0 {
            return Err(());
        }
        self.reader.expect(b'{')?;
        self.skip_ws()?;
        if self.reader.peek()? == Some(b'}') {
            self.reader.consume();
            return Ok(());
        }
        // Track entries by key so duplicate server names follow HashMap's
        // last-write-wins semantics (including overwriting stdio with http).
        let mut entries: Vec<(CompactString, Option<ServerSignature>)> = Vec::new();
        let mut key_buf = CompactString::default();
        loop {
            key_buf.clear();
            self.parse_string(&mut StrSink::Collect(&mut key_buf))?;
            self.skip_ws()?;
            self.reader.expect(b':')?;
            self.skip_ws()?;
            let server = self.parse_server_object(depth - 1)?;
            if let Some(existing) = entries.iter_mut().find(|(k, _)| k == key_buf) {
                existing.1 = server;
            } else {
                entries.push((key_buf.clone(), server));
            }
            self.skip_ws()?;
            match self.reader.next_byte()? {
                Some(b',') => self.skip_ws()?,
                Some(b'}') => break,
                _ => return Err(()),
            }
        }
        for (_, server) in entries {
            if let Some(sig) = server {
                out.push(sig);
            }
        }
        Ok(())
    }

    fn parse_project_object(
        &mut self,
        depth: usize,
        out: &mut Vec<ServerSignature>,
    ) -> Result<(), ()> {
        if depth == 0 {
            return Err(());
        }
        self.reader.expect(b'{')?;
        self.skip_ws()?;
        if self.reader.peek()? == Some(b'}') {
            self.reader.consume();
            return Ok(());
        }
        let mut saw_mcp_servers = false;
        loop {
            let mut key = StrSink::stack();
            self.parse_string(&mut key)?;
            self.skip_ws()?;
            self.reader.expect(b':')?;
            self.skip_ws()?;
            if key.as_stack_bytes() == Some(b"mcpServers") {
                if saw_mcp_servers {
                    return Err(());
                }
                saw_mcp_servers = true;
                self.parse_servers_map(depth - 1, out)?;
            } else {
                self.skip_value(depth - 1)?;
            }
            self.skip_ws()?;
            match self.reader.next_byte()? {
                Some(b',') => self.skip_ws()?,
                Some(b'}') => return Ok(()),
                _ => return Err(()),
            }
        }
    }

    fn parse_projects_map(&mut self, depth: usize) -> Result<(), ()> {
        if depth == 0 {
            return Err(());
        }
        self.reader.expect(b'{')?;
        self.skip_ws()?;
        if self.reader.peek()? == Some(b'}') {
            self.reader.consume();
            return Ok(());
        }
        // Keep only projects that actually declared stdio servers; reuse one
        // small scratch buffer for reading keys so projects without servers
        // retain zero heap strings.
        let mut project_servers: Vec<(CompactString, Vec<ServerSignature>)> = Vec::new();
        let mut key_buf = CompactString::default();
        let mut current = Vec::new();
        loop {
            key_buf.clear();
            self.parse_string(&mut StrSink::Collect(&mut key_buf))?;
            if let Some(paths) = self.project_paths.as_mut()
                && paths.len() < MAX_DISCOVERED_PATHS
                && key_buf.starts_with('/')
                && !paths.iter().any(|existing| existing == key_buf.as_str())
            {
                paths.push(key_buf.to_string());
            }
            self.skip_ws()?;
            self.reader.expect(b':')?;
            self.skip_ws()?;
            current.clear();
            self.parse_project_object(depth - 1, &mut current)?;
            if let Some(pos) = project_servers.iter().position(|(k, _)| k == key_buf) {
                if current.is_empty() {
                    project_servers.swap_remove(pos);
                } else {
                    project_servers[pos].1 = std::mem::take(&mut current);
                }
            } else if !current.is_empty() {
                project_servers.push((key_buf.clone(), std::mem::take(&mut current)));
            }
            self.skip_ws()?;
            match self.reader.next_byte()? {
                Some(b',') => self.skip_ws()?,
                Some(b'}') => break,
                _ => return Err(()),
            }
        }
        for (_, servers) in project_servers {
            self.servers.extend(servers);
        }
        Ok(())
    }

    fn parse_root(mut self) -> Result<(Vec<ServerSignature>, Vec<String>), ()> {
        self.skip_ws()?;
        self.reader.expect(b'{')?;
        self.skip_ws()?;
        let mut saw_mcp_servers = false;
        let mut saw_projects = false;
        if self.reader.peek()? != Some(b'}') {
            loop {
                let mut key = StrSink::stack();
                self.parse_string(&mut key)?;
                self.skip_ws()?;
                self.reader.expect(b':')?;
                self.skip_ws()?;
                match key.as_stack_bytes() {
                    Some(b"mcpServers") => {
                        if saw_mcp_servers {
                            return Err(());
                        }
                        saw_mcp_servers = true;
                        let mut root_servers = Vec::new();
                        self.parse_servers_map(MAX_NESTING_DEPTH - 1, &mut root_servers)?;
                        self.servers.extend(root_servers);
                    }
                    Some(b"projects") => {
                        if saw_projects {
                            return Err(());
                        }
                        saw_projects = true;
                        self.parse_projects_map(MAX_NESTING_DEPTH - 1)?;
                    }
                    _ => self.skip_value(MAX_NESTING_DEPTH - 1)?,
                }
                self.skip_ws()?;
                match self.reader.next_byte()? {
                    Some(b',') => self.skip_ws()?,
                    Some(b'}') => break,
                    _ => return Err(()),
                }
            }
        } else {
            self.reader.consume();
        }
        self.skip_ws()?;
        if self.reader.peek()?.is_some() {
            return Err(());
        }
        Ok((
            dedup_signatures(self.servers),
            self.project_paths.unwrap_or_default(),
        ))
    }
}

/// Stream stdio servers directly from a `~/.claude.json` reader using a fixed
/// stack buffer without allocating `BufReader` or scratch buffers for skipped
/// fields.
#[cfg_attr(not(test), allow(dead_code))]
pub fn claude_servers_from_reader(reader: impl Read) -> Vec<ServerSignature> {
    JsonParser::new(reader, false)
        .parse_root()
        .map_or_default(|(servers, _)| servers)
}

/// Stream stdio servers and project directory paths from `~/.claude.json` in a
/// single pass.
pub fn claude_config_from_reader(reader: impl Read) -> (Vec<ServerSignature>, Vec<String>) {
    JsonParser::new(reader, true)
        .parse_root()
        .unwrap_or_default()
}

/// Stdio servers in `~/.claude.json`, user scope and every project scope.
#[cfg(test)]
pub fn claude_servers(text: &str) -> Vec<ServerSignature> {
    claude_servers_from_reader(io::Cursor::new(text.as_bytes()))
}

fn read_bounded_utf8(reader: impl Read, limit: usize) -> Option<String> {
    let mut bytes = Vec::new();
    let mut limited = reader.take((limit as u64).saturating_add(1));
    limited.read_to_end(&mut bytes).ok()?;
    if bytes.len() > limit {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Strip JSONC `//` line comments, `/* ... */` block comments, and trailing
/// commas before `}` or `]` while preserving string literals verbatim.
fn strip_jsonc(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            out.push(b);
            if b == b'\\' && i + 1 < bytes.len() {
                i += 1;
                out.push(bytes[i]);
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            out.push(b);
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
        }
        out.push(b);
        i += 1;
    }

    // Remove trailing commas immediately preceding `}` or `]`.
    let mut cleaned = Vec::with_capacity(out.len());
    let mut j = 0usize;
    in_string = false;
    while j < out.len() {
        let b = out[j];
        if in_string {
            cleaned.push(b);
            if b == b'\\' && j + 1 < out.len() {
                j += 1;
                cleaned.push(out[j]);
            } else if b == b'"' {
                in_string = false;
            }
            j += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            cleaned.push(b);
            j += 1;
            continue;
        }
        if b == b',' {
            let mut k = j + 1;
            while k < out.len() && out[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < out.len() && matches!(out[k], b'}' | b']') {
                j += 1;
                continue;
            }
        }
        cleaned.push(b);
        j += 1;
    }
    String::from_utf8(cleaned).unwrap_or_default()
}

fn extract_json_server_entry(value: &serde_json::Value) -> Option<ServerSignature> {
    let obj = value.as_object()?;
    let cmd_val = obj.get("command")?;
    match cmd_val {
        serde_json::Value::String(cmd) if !cmd.is_empty() => {
            let mut args = SmallVec::new();
            if let Some(arr) = obj.get("args").and_then(serde_json::Value::as_array) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        args.push(CompactString::from(s));
                    }
                }
            }
            Some(ServerSignature {
                command: CompactString::from(cmd.as_str()),
                args,
            })
        }
        serde_json::Value::Array(items) => {
            let mut iter = items.iter().filter_map(serde_json::Value::as_str);
            let cmd = iter.next().filter(|s| !s.is_empty())?;
            let args = iter.map(CompactString::from).collect();
            Some(ServerSignature {
                command: CompactString::from(cmd),
                args,
            })
        }
        serde_json::Value::Object(cmd_obj) => {
            let path = cmd_obj
                .get("path")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())?;
            let mut args = SmallVec::new();
            if let Some(arr) = cmd_obj.get("args").and_then(serde_json::Value::as_array) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        args.push(CompactString::from(s));
                    }
                }
            }
            Some(ServerSignature {
                command: CompactString::from(path),
                args,
            })
        }
        _ => None,
    }
}

fn collect_server_map(
    map: &serde_json::Map<String, serde_json::Value>,
    out: &mut Vec<ServerSignature>,
    depth: u8,
) {
    for (key, value) in map {
        if let Some(sig) = extract_json_server_entry(value) {
            out.push(sig);
        } else if depth > 0
            && matches!(key.as_str(), "servers" | "mcpServers")
            && let Some(nested) = value.as_object()
        {
            collect_server_map(nested, out, depth - 1);
        }
    }
}

/// Extract stdio MCP servers from standard JSON/JSONC MCP config files:
/// Antigravity (`~/.gemini/antigravity/mcp_config.json`), Claude Desktop
/// (`claude_desktop_config.json`), VS Code (`mcp.json` with `"servers"`),
/// Cursor/Windsurf, project `.mcp.json`, and Claude/Codex/Gemini plugin
/// `.mcp.json` (which may use `{"mcpServers": {...}}`, `{"servers": {...}}`, or
/// flat `{"serverName": {"command": ..., "args": [...]}}`).
pub fn mcp_json_servers_from_reader(reader: impl Read) -> Vec<ServerSignature> {
    let Some(raw) = read_bounded_utf8(reader, MAX_AUX_CONFIG_BYTES) else {
        return Vec::new();
    };
    let text = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
    let parsed = serde_json::from_str::<serde_json::Value>(text)
        .or_else(|_| serde_json::from_str::<serde_json::Value>(&strip_jsonc(text)));
    let Ok(serde_json::Value::Object(root)) = parsed else {
        return Vec::new();
    };
    let mut servers = Vec::new();
    if let Some(map) = root
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
    {
        collect_server_map(map, &mut servers, 1);
    } else if let Some(map) = root.get("mcp").and_then(serde_json::Value::as_object) {
        collect_server_map(map, &mut servers, 1);
    } else {
        // Flat plugin `.mcp.json` or VS Code `{"servers": {...}}`.
        collect_server_map(&root, &mut servers, 1);
    }
    dedup_signatures(servers)
}

/// Extract local stdio MCP servers from scoped JSON/JSONC settings files:
/// OpenCode (`opencode.json`/`opencode.jsonc` `"mcp"`), Gemini CLI / Antigravity
/// (`settings.json` `"mcpServers"`), plugin manifests (`plugin.json`), and Zed
/// (`~/.config/zed/settings.json` `"context_servers"`). Ignores unrelated keys
/// such as `"hooks"`.
pub fn opencode_servers_from_reader(reader: impl Read) -> Vec<ServerSignature> {
    let Some(raw) = read_bounded_utf8(reader, MAX_AUX_CONFIG_BYTES) else {
        return Vec::new();
    };
    let text = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
    let cleaned = strip_jsonc(text);
    let Ok(serde_json::Value::Object(root)) = serde_json::from_str(&cleaned) else {
        return Vec::new();
    };
    let mut servers = Vec::new();
    for key in ["mcp", "mcpServers", "context_servers"] {
        if let Some(map) = root.get(key).and_then(serde_json::Value::as_object) {
            collect_server_map(map, &mut servers, 1);
        }
    }
    dedup_signatures(servers)
}

/// Extract project directory paths from `[projects."<path>"]` in `~/.codex/config.toml`.
#[cfg(test)]
pub fn codex_project_paths_from_reader(reader: impl Read) -> Vec<String> {
    codex_config_from_reader(reader).1
}

/// Extract installed plugin directories from `~/.claude/plugins/installed_plugins.json`.
pub fn claude_installed_plugin_paths_from_reader(reader: impl Read) -> Vec<String> {
    let Some(text) = read_bounded_utf8(reader, MAX_AUX_CONFIG_BYTES) else {
        return Vec::new();
    };
    let Ok(serde_json::Value::Object(root)) = serde_json::from_str(&text) else {
        return Vec::new();
    };
    let Some(plugins) = root.get("plugins").and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entries in plugins.values() {
        let Some(arr) = entries.as_array() else {
            continue;
        };
        for entry in arr {
            if paths.len() >= MAX_DISCOVERED_PATHS {
                return paths;
            }
            if let Some(path) = entry.get("installPath").and_then(serde_json::Value::as_str)
                && path.starts_with('/')
                && !paths.iter().any(|existing| existing == path)
            {
                paths.push(path.to_owned());
            }
        }
    }
    paths
}

pub fn dedup_signatures(mut signatures: Vec<ServerSignature>) -> Vec<ServerSignature> {
    signatures.sort_by(|a, b| (&a.command, &a.args).cmp(&(&b.command, &b.args)));
    signatures.dedup();
    signatures
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use smallvec::smallvec;

    use super::*;

    #[derive(Deserialize)]
    struct CodexConfigOracle {
        #[serde(default)]
        mcp_servers: HashMap<String, StdioServerOracle>,
    }

    #[derive(Deserialize)]
    struct ClaudeConfigOracle {
        #[serde(default, rename = "mcpServers")]
        mcp_servers: HashMap<String, StdioServerOracle>,
        #[serde(default)]
        projects: HashMap<String, ClaudeProjectOracle>,
    }

    #[derive(Deserialize)]
    struct ClaudeProjectOracle {
        #[serde(default, rename = "mcpServers")]
        mcp_servers: HashMap<String, StdioServerOracle>,
    }

    #[derive(Deserialize)]
    struct StdioServerOracle {
        command: Option<String>,
        #[serde(default)]
        args: Vec<String>,
    }

    fn oracle_signatures(servers: impl Iterator<Item = StdioServerOracle>) -> Vec<ServerSignature> {
        dedup_signatures(
            servers
                .filter_map(|server| {
                    Some(ServerSignature {
                        command: CompactString::from(server.command.filter(|cmd| !cmd.is_empty())?),
                        args: server.args.into_iter().map(CompactString::from).collect(),
                    })
                })
                .collect(),
        )
    }

    #[test]
    fn ps_rows_parse_cpu_age_tty_and_start() {
        let text = "\
    1     0     0 ??       Ss   37:14.55    4-03:10:30 Fri Oct  2 18:15:22 2026
 9161  9160   501 ??       S     1:02:03.4     02:34:55 Tue Oct  6 12:37:26 2026
 9200  9161   501 ttys001  S+    0:00.01        00:05 Tue Oct  6 15:12:20 2026
 9300  9161   501 ??       Z     0:00.00        00:05 Tue Oct  6 15:12:20 2026
";
        let rows = ps_rows(text).unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows[3].zombie && !rows[1].zombie);
        assert_eq!(rows[0].cpu_centis, (37 * 60 + 14) * 100 + 55);
        assert_eq!(rows[0].age_secs, 4 * 86_400 + 3 * 3600 + 10 * 60 + 30);
        assert_eq!(rows[0].started, "Fri Oct 2 18:15:22 2026");
        assert!(!rows[1].has_tty);
        assert_eq!(rows[1].cpu_centis, (3600 + 2 * 60 + 3) * 100 + 40);
        assert!(rows[2].has_tty);
        assert_eq!(rows[2].age_secs, 5);
    }

    #[test]
    fn foreign_locale_table_is_refused_not_read_as_empty() {
        // `ps` under LANG=ru_RU prints lstart as seven words.
        let russian =
            " 9161  9160   501 ??  S  1:02.03  02:34:55 вторник,  6 октября 2026 г. 17:14:13\n";
        assert_eq!(ps_rows(russian), None);
        assert_eq!(ps_rows(""), None);
    }

    #[test]
    fn pid_column_keeps_paths_with_spaces() {
        let text = " 9161 /Users/u/Library/Application Support/Claude/claude-code/2.1/claude\n   1 /sbin/launchd\n";
        let map = pid_column(text);
        assert_eq!(
            map[&9161],
            "/Users/u/Library/Application Support/Claude/claude-code/2.1/claude"
        );
        assert_eq!(map[&1], "/sbin/launchd");
    }

    #[test]
    fn top_footprints_parse_units_and_trend_marks() {
        let text = "Processes: 3 total\nLoad Avg: 1\n\nPID    MEM\n99916  2176K\n97126* 98M+\n53027- 1.5G-\n12345+ 512K\n";
        let map = top_footprints(text);
        assert_eq!(map[&99916], 2176 * 1024);
        assert_eq!(map[&97126], 98 * 1024 * 1024);
        assert_eq!(map[&53027], (1.5 * 1024.0 * 1024.0 * 1024.0) as u64);
        assert_eq!(map[&12345], 512 * 1024);
    }

    #[test]
    fn system_values_parse() {
        assert_eq!(pressure("2\n"), Some(Pressure::Warning));
        assert_eq!(pressure("4"), Some(Pressure::Critical));
        assert_eq!(pressure("3"), None);
        let swap = swap("total = 6144.00M  used = 4539.81M  free = 1604.19M  (encrypted)").unwrap();
        assert_eq!(swap.total_bytes, 6144 * 1024 * 1024);
        assert_eq!(swap.used_bytes, (4539.81 * 1024.0 * 1024.0) as u64);
        assert_eq!(
            hid_idle_secs("  | |   \"HIDIdleTime\" = 1835000000000\n"),
            Some(1835)
        );
        assert_eq!(
            launchd_pids("PID\tStatus\tLabel\n-\t0\tcom.a\n80503\t0\tcom.b\n"),
            vec![80503]
        );
    }

    #[test]
    fn mcp_configs_yield_stdio_servers_only() {
        let codex = r#"
model = "x"
[mcp_servers.codegraph]
command = "/Users/u/.local/bin/codegraph"
args = ["serve", "--mcp"]
[mcp_servers.context7]
url = "https://example.invalid/mcp"
[mcp_servers.railway]
command = "railway"
args = ["mcp"]
"#;
        let servers = codex_servers(codex);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[1].command, "railway");

        let claude = r#"{"mcpServers":{"railway":{"command":"railway","args":["mcp"]},
            "docs":{"type":"http","url":"https://example.invalid"}},
            "projects":{"/p":{"mcpServers":{"gopls":{"command":"gopls-forward","args":["mcp"]}}}}}"#;
        let servers = claude_servers(claude);
        assert_eq!(
            servers
                .iter()
                .map(|s| s.command.as_str())
                .collect::<Vec<_>>(),
            ["gopls-forward", "railway"]
        );
        assert!(codex_servers("not toml [").is_empty());
    }

    #[test]
    fn ps_processes_combines_fields_and_comm_and_rejects_foreign_locale() {
        let text = "\
 9161  9160   501 ??       S     1:02:03.4     02:34:55 Tue Oct  6 12:37:26 2026 /Users/u/Library/Application Support/Claude/claude
    1     0     0 ??       Ss   37:14.55    4-03:10:30 Fri Oct  2 18:15:22 2026 /sbin/launchd
  324     1     0 ??       Ss   43:46.58    4-03:10:30 Fri Oct  2 18:15:22 2026 /usr/libexec/logd
 9162  9161     0 ??       S     0:00.10        00:05 Tue Oct  6 15:12:20 2026 /usr/bin/sudo
 9300  9161   501 ??       Z     0:00.00        00:05 Tue Oct  6 15:12:20 2026 <defunct>
";
        let mut snap = ps_processes(text, None).unwrap();
        assert_eq!(snap.processes.len(), 4);
        // Sorted by PID and zombies excluded.
        assert_eq!(snap.processes[0].pid, 1);
        assert_eq!(snap.exe(&snap.processes[0]), "/sbin/launchd");
        assert_eq!(snap.processes[2].pid, 9161);
        assert_eq!(
            snap.exe(&snap.processes[2]),
            "/Users/u/Library/Application Support/Claude/claude"
        );
        assert_eq!(snap.started(&snap.processes[2]), "Tue Oct 6 12:37:26 2026");

        // User-scoped parse omits top-level root daemons (1, 324) but keeps setuid child (9162).
        let scoped = ps_processes(text, Some(501)).unwrap();
        assert_eq!(
            scoped.processes.iter().map(|p| p.pid).collect::<Vec<_>>(),
            [9161, 9162]
        );

        apply_pid_args(" 9161 claude --session abc\n 99999 gone\n", &mut snap);
        assert_eq!(snap.args(&snap.processes[2]), "claude --session abc");
        assert!(snap.args(&snap.processes[0]).is_empty());

        apply_top_footprints(
            "Header\nPID MEM\n9161 98M+\n1 2176K\n99999 5M\n",
            &mut snap.processes,
        );
        assert_eq!(snap.processes[0].footprint_bytes, Some(2176 * 1024));
        assert_eq!(snap.processes[2].footprint_bytes, Some(98 * 1024 * 1024));

        let russian = " 9161  9160   501 ??  S  1:02.03  02:34:55 вторник,  6 октября 2026 г. 17:14:13 /bin/foo\n";
        assert_eq!(ps_processes(russian, Some(501)), None);
        assert_eq!(ps_processes("", Some(501)), None);
    }

    #[test]
    fn pack_lstart_format_stack_and_identify_line_work_on_boundaries() {
        let l1 = ["Tue", "Oct", "6", "12:37:26", "2026"];
        let l2 = ["Tue", "Oct", "6", "12:37:27", "2026"];
        let l3 = ["Wed", "Dec", "31", "23:59:59", "2132"];
        let k1 = pack_lstart(l1).unwrap();
        let k2 = pack_lstart(l2).unwrap();
        let k3 = pack_lstart(l3).unwrap();
        assert_eq!(k2, k1 + 1);
        assert!(k3 > k2);
        assert_eq!(pack_lstart(["Tue", "Foo", "6", "12:37:26", "2026"]), None);
        assert_eq!(pack_lstart(["Tue", "Oct", "6", "12:37:26", "1999"]), None);

        let mut stack = [0u8; 40];
        assert_eq!(
            format_lstart_stack(l1, &mut stack),
            Some("Tue Oct 6 12:37:26 2026")
        );
        assert_eq!(
            parse_identify_line("  501 Tue Oct  6 12:37:26 2026\n"),
            Some(("Tue Oct 6 12:37:26 2026".into(), 501))
        );
        assert_eq!(
            parse_identify_line("  501 extra Tue Oct 6 12:37:26 2026"),
            None
        );
    }

    #[test]
    fn combined_sysctl_parses_both_and_degrades_independently() {
        let both = "kern.memorystatus_vm_pressure_level: 2\nvm.swapusage: total = 6144.00M  used = 4539.81M  free = 1604.19M\n";
        let (level, swap_usage) = sysctl_pressure_and_swap(both);
        assert_eq!(level, Pressure::Warning);
        assert_eq!(swap_usage.total_bytes, 6144 * 1024 * 1024);

        let partial = "kern.memorystatus_vm_pressure_level: 4\n";
        let (level, swap_usage) = sysctl_pressure_and_swap(partial);
        assert_eq!(level, Pressure::Critical);
        assert_eq!(swap_usage, Swap::default());
    }

    #[test]
    fn codex_toml_extractor_handles_multiline_strings_quoted_headers_and_inline_tables() {
        let tricky = "\u{feff}\
# [mcp_servers.commented]
# command = \"fake\"
model = \"o3\"
note = \"\"\"
[mcp_servers.fake_basic]
command = \"evil\"
args = [\"x\"]
\"\"\"
lit = '''
[mcp_servers.fake_literal]
command = \"evil2\"
args = [\"y\"]
'''
escaped = \"quote \\\" still string [\"
[projects.\"/a/[b]/c\"]
trust = \"trusted\"
banner = \"\"\"still \\\"\"\"
[mcp_servers.also_fake]
command = \"evil3\"
\"\"\"\"\"
[mcp_servers.\"quoted.server\"]
command = \"srv-a\"
args = [
  \"serve\",
  \"--mcp\",
]
[mcp_servers.\"quoted.server\".env]
FOO = \"bar\"
['mcp_servers'.'lit-server']
command = 'srv-b'
args = ['run']
";
        let servers = codex_servers(tricky);
        assert_eq!(
            servers,
            vec![
                ServerSignature {
                    command: "srv-a".into(),
                    args: smallvec!["serve".into(), "--mcp".into()],
                },
                ServerSignature {
                    command: "srv-b".into(),
                    args: smallvec!["run".into()],
                },
            ]
        );

        let inline = "mcp_servers = { a = { command = \"tool\", args = [\"--mcp\"] } }\n";
        assert_eq!(
            codex_servers(inline),
            vec![ServerSignature {
                command: "tool".into(),
                args: smallvec!["--mcp".into()],
            }]
        );

        let dotted = "mcp_servers.a.command = \"tool\"\nmcp_servers.a.args = [\"--mcp\"]\n";
        assert_eq!(
            codex_servers(dotted),
            vec![ServerSignature {
                command: "tool".into(),
                args: smallvec!["--mcp".into()],
            }]
        );

        // Adversarial valid TOML: CRLF, 6/7/8-quote multiline strings, escaped backslashes
        // before closing delimiters, unrelated inline tables containing `mcp_servers`,
        // nested `[other."mcp_servers"]` headers, escaped `mcp\u005fservers` key,
        // `[mcp_servers]` with dotted keys + inline table, line-ending backslash, and
        // RFC 3339 date-time scalars.
        let adversarial = "\
other = { mcp_servers = 1 }\r\n\
dt = 1979-05-27 07:32:00Z\r\n\
empty_ml = \"\"\"\"\"\"\r\n\
one_q = \"\"\"\"\"\"\"\r\n\
two_q = \"\"\"\"\"\"\"\"\r\n\
lit_ml = ''''''''\r\n\
esc_bs = \"\"\"trail\\\\\"\"\"\r\n\
trimmed = \"\"\"\r\n\
  hello \\\r\n\
     world\"\"\"\r\n\
arr = [\r\n\
  \"[mcp_servers.in_arr]\",\r\n\
  [1, 2],\r\n\
]\r\n\
[another.\"mcp_servers\"]\r\n\
command = \"ignored\"\r\n\
args = [\"mcp\"]\r\n\
[\"mcp\\u005fservers\".escaped_hdr]\r\n\
command = \"esc-tool\"\r\n\
args = [\"--run\", \"\\u2603\"]\r\n\
[mcp_servers]\r\n\
inline_one = { command = \"inline-cmd\", args = [\"serve\"] }\r\n\
dotted_one.command = \"dotted-cmd\"\r\n\
dotted_one.args = [\"mcp\"]\r\n\
[mcp_servers.real]\r\n\
command = \"railway\"\r\n\
args = [\"mcp\"]\r\n\
";
        let expected_full: CodexConfigOracle = toml::from_str(adversarial).unwrap();
        assert_eq!(
            codex_servers(adversarial),
            oracle_signatures(expected_full.mcp_servers.into_values())
        );

        // Invalid TOML anywhere in the file or mismatched types return empty, matching `toml`.
        for broken in [
            "not toml [",
            "[mcp_servers.a]\ncommand = \"ok\"\n[broken",
            "[[mcp_servers]]\ncommand = \"x\"\n",
            "[mcp_servers.a]\ncommand = 123\n",
            "[mcp_servers.a]\ncommand = \"x\"\ncommand = \"y\"\n",
            "[mcp_servers.a]\ncommand = \"x\"\nargs = [1]\n",
            "bad_num = 123 456\n",
            "unclosed = \"\"\"abc",
        ] {
            assert!(
                toml::from_str::<CodexConfigOracle>(broken).is_err(),
                "expected oracle to reject: {broken:?}"
            );
            assert!(
                codex_servers(broken).is_empty(),
                "expected streaming parser to reject: {broken:?}"
            );
        }
    }

    #[test]
    fn claude_json_stream_skips_unrelated_fields_and_empty_commands() {
        let json = r#"{
            "tipsHistory": {"a": 1, "b": [1, 2, 3]},
            "mcpServers": {
                "empty": {"command": "", "args": ["x"]},
                "ok": {"command": "tool", "args": ["mcp"]}
            },
            "projects": {
                "/Users/u/proj1": {"history": ["huge"], "mcpServers": {}},
                "/Users/u/proj2": {"mcpServers": {"p": {"command": "proj-tool", "args": ["serve"]}}}
            }
        }"#;
        let servers = claude_servers(json);
        assert_eq!(
            servers
                .iter()
                .map(|s| s.command.as_str())
                .collect::<Vec<_>>(),
            ["proj-tool", "tool"]
        );
        assert!(claude_servers("{broken").is_empty());
    }

    #[test]
    fn claude_json_stream_matches_serde_json_on_escapes_large_strings_and_errors() {
        let huge = "a".repeat(20_000);
        let complex = format!(
            r#"{{
                "hugeBlob": "{huge}",
                "nums": [0, -0, 123, -45.67, 1e10, 2.5E-3, true, false, null],
                "mcp\u0053ervers": {{
                    "nullCmd": {{"command": null, "args": ["ignored"]}},
                    "rocket": {{"command": "bin/\uD83D\uDE80", "args": ["a\"b\\c", "\t\n"]}}
                }},
                "projects": {{
                    "/p/1": {{
                        "history": ["{huge}", {{"nested": [1, 2, 3]}}],
                        "mcpServers": {{
                            "s": {{"command": "gopls", "args": ["mcp"]}}
                        }}
                    }}
                }}
            }}"#
        );
        let oracle: ClaudeConfigOracle = serde_json::from_str(&complex).unwrap();
        let expected = oracle_signatures(
            oracle.mcp_servers.into_values().chain(
                oracle
                    .projects
                    .into_values()
                    .flat_map(|p| p.mcp_servers.into_values()),
            ),
        );
        assert_eq!(claude_servers(&complex), expected);

        for invalid in [
            "",
            "[]",
            r#"{"a": 01}"#,
            r#"{"a": 1.}"#,
            r#"{"a": 1e}"#,
            "{\"a\": \"\x01\"}",
            r#"{"a": "\uD83D"}"#,
            r#"{"a": "\uDE80"}"#,
            r#"{"mcpServers": {"a": {"command": "x"}}}, trailing"#,
            r#"{"mcpServers": {"a": {"command": "x", "command": "y"}}}"#,
            r#"{"mcpServers": null}"#,
        ] {
            assert!(
                claude_servers(invalid).is_empty(),
                "expected invalid JSON to return empty: {invalid:?}"
            );
        }
    }

    #[test]
    fn codex_toml_matches_oracle_on_duplicates_bare_scalars_crlf_and_chunk_boundaries() {
        // 1. Multiline CRLF handling vs `toml` crate oracle.
        let crlf_ml = "[mcp_servers.a]\r\ncommand = \"\"\"\r\nline1\r\nline2\"\"\"\r\nargs = ['''\r\na\r\nb''']\r\n";
        let expected_crlf: CodexConfigOracle = toml::from_str(crlf_ml).unwrap();
        assert_eq!(
            codex_servers(crlf_ml),
            oracle_signatures(expected_crlf.mcp_servers.into_values()),
            "multiline CRLF mismatch"
        );

        // 2. Valid bare scalars (hex/oct/bin, underscores, inf/nan, RFC3339 variants, subtable before super-table).
        let valid_scalars = "\
b1 = true
b2 = false
i1 = 0
i2 = +99
i3 = -1_000
h1 = 0xdead_beef
o1 = 0o755
bin1 = 0b1010_0101
f1 = +1.0
f2 = 3.1415
f3 = -0.01
f4 = 5e+22
f5 = 1e06
f6 = -2E-2
f7 = 6.626e-34
f8 = 224_617.445_991_228
inf1 = inf
inf2 = +inf
inf3 = -inf
nan1 = nan
nan2 = +nan
nan3 = -nan
d1 = 1979-05-27T07:32:00Z
d2 = 1979-05-27T00:32:00-07:00
d3 = 1979-05-27T00:32:00.999999-07:00
d4 = 1979-05-27 07:32:00Z
d5 = 1979-05-27t07:32:00z
d6 = 1979-05-27T07:32:00
d7 = 1979-05-27 07:32:00.5
d8 = 1979-05-27
t1 = 07:32:00
t2 = 00:32:00.999999
[mcp_servers.sub_first.env]
FOO = \"bar\"
[mcp_servers.sub_first]
command = \"railway\"
args = [\"mcp\"]
[mcp_servers]
inline_srv = { command = \"codegraph\", args = [\"serve\", \"--mcp\"] }
";
        let expected_valid: CodexConfigOracle = toml::from_str(valid_scalars).unwrap();
        assert_eq!(
            codex_servers(valid_scalars),
            oracle_signatures(expected_valid.mcp_servers.into_values()),
            "valid scalars / sub-first mismatch"
        );

        // 3. Chunk-boundary straddling (4096-byte StackReader buffer) across UTF-8, escapes, keys, headers.
        for pad in 4080..4110 {
            let padding = "x".repeat(pad);
            let doc = format!(
                "pad = \"{padding}\"\n[mcp_servers.\"🚀_srv\"]\ncommand = \"bin/🚀\\u002dtool\"\nargs = [\"\\U0001F680\", \"mcp\"]\n"
            );
            let expected: CodexConfigOracle = toml::from_str(&doc).unwrap();
            assert_eq!(
                codex_servers(&doc),
                oracle_signatures(expected.mcp_servers.into_values()),
                "chunk boundary mismatch at pad={pad}"
            );
        }

        // 4. Invalid TOML that must return empty (matching `toml::from_str` error).
        let invalid_cases = [
            // Duplicate [mcp_servers.a] table header
            "[mcp_servers.a]\ncommand = \"railway\"\n[mcp_servers.a]\nargs = [\"mcp\"]\n",
            // Duplicate [mcp_servers] table header
            "[mcp_servers]\na.command = \"x\"\n[mcp_servers]\nb.command = \"y\"\n",
            // Reopening inline table mcp_servers
            "mcp_servers = { a = { command = \"x\" } }\n[mcp_servers.b]\ncommand = \"y\"\n",
            // Reopening inline server entry via table header
            "[mcp_servers]\na = { command = \"x\" }\n[mcp_servers.a]\nargs = [\"y\"]\n",
            // Reopening inline server entry via dotted key
            "mcp_servers.a = { command = \"x\" }\nmcp_servers.a.args = [\"y\"]\n",
            // Reopening dotted-key server table via header
            "[mcp_servers]\na.command = \"x\"\n[mcp_servers.a]\nargs = [\"y\"]\n",
            // Subtable / dotted key colliding with `command` or `args` (with sibling server to prove rejection)
            "[mcp_servers.b]\ncommand = \"ok\"\n[mcp_servers.a.command.sub]\nx = 1\n",
            "[mcp_servers.b]\ncommand = \"ok\"\n[mcp_servers.a.args.sub]\nx = 1\n",
            "[mcp_servers.b]\ncommand = \"ok\"\n[mcp_servers.a]\ncommand.sub = \"x\"\n",
            "[mcp_servers.b]\ncommand = \"ok\"\n[mcp_servers.a]\nargs.sub = \"x\"\n",
            "[mcp_servers.b]\ncommand = \"ok\"\n[mcp_servers]\na.command.sub = \"x\"\n",
            "mcp_servers.a.command.sub = \"x\"\n[mcp_servers.b]\ncommand = \"ok\"\n",
            // Duplicate inline server inside mcp_servers inline table
            "mcp_servers = { a = { command = \"x\" }, a = { command = \"y\" } }\n",
            // Duplicate mcp_servers inline table assignment
            "mcp_servers = { a = { command = \"x\" } }\nmcp_servers = { b = { command = \"y\" } }\n",
            // Adding subtable to inline server table
            "[mcp_servers]\na = { command = \"x\" }\n[mcp_servers.a.env]\nFOO = \"bar\"\n",
            // Explicit [mcp_servers.a] reopened via dotted key under [mcp_servers]
            "[mcp_servers.a]\ncommand = \"x\"\n[mcp_servers]\na.args = [\"y\"]\n",
            // Malformed bare scalars in unrelated sections
            "bad = 1..2\n[mcp_servers.a]\ncommand = \"x\"\n",
            "bad = true_x\n[mcp_servers.a]\ncommand = \"x\"\n",
            "bad = unquoted\n[mcp_servers.a]\ncommand = \"x\"\n",
            "bad = 01\n[mcp_servers.a]\ncommand = \"x\"\n",
            "bad = 1_\n[mcp_servers.a]\ncommand = \"x\"\n",
            "bad = 1__2\n[mcp_servers.a]\ncommand = \"x\"\n",
            "bad = 1.\n[mcp_servers.a]\ncommand = \"x\"\n",
            "bad = 1e\n[mcp_servers.a]\ncommand = \"x\"\n",
            "bad = 0x\n[mcp_servers.a]\ncommand = \"x\"\n",
            "bad = +true\n[mcp_servers.a]\ncommand = \"x\"\n",
            "bad = 2024-13-01\n[mcp_servers.a]\ncommand = \"x\"\n",
            "bad = 2024-01-01 25:00:00\n[mcp_servers.a]\ncommand = \"x\"\n",
            "bad = 2024-01-01\t12:00:00\n[mcp_servers.a]\ncommand = \"x\"\n",
        ];
        let mut failures = Vec::new();
        for case in invalid_cases {
            assert!(
                toml::from_str::<CodexConfigOracle>(case).is_err(),
                "oracle unexpectedly accepted: {case:?}"
            );
            let got = codex_servers(case);
            if !got.is_empty() {
                failures.push((case, got));
            }
        }
        assert!(
            failures.is_empty(),
            "TomlParser accepted {} invalid cases: {failures:#?}",
            failures.len()
        );
    }

    #[test]
    fn claude_json_matches_oracle_on_duplicates_boundaries_and_edge_cases() {
        // 1. Chunk-boundary straddling (4096-byte StackReader buffer) across UTF-8 & surrogate pairs.
        for pad in 4080..4110 {
            let padding = "x".repeat(pad);
            let doc = format!(
                r#"{{"pad":"{padding}","mcpServers":{{"s":{{"command":"bin/\uD83D\uDE80","args":["🚀","mcp"]}}}}}}"#
            );
            let oracle: ClaudeConfigOracle = serde_json::from_str(&doc).unwrap();
            assert_eq!(
                claude_servers(&doc),
                oracle_signatures(oracle.mcp_servers.into_values()),
                "json boundary mismatch at pad={pad}"
            );
        }

        // 2. Duplicate server key in mcpServers: last value wins (matches HashMap / serde_json).
        let dup_server = r#"{"mcpServers":{"a":{"command":"old","args":["1"]},"a":{"command":"new","args":["2"]}}}"#;
        let oracle: ClaudeConfigOracle = serde_json::from_str(dup_server).unwrap();
        assert_eq!(
            claude_servers(dup_server),
            oracle_signatures(oracle.mcp_servers.into_values()),
            "duplicate server key mismatch"
        );

        let dup_server_to_http = r#"{"mcpServers":{"a":{"command":"old","args":["1"]},"a":{"url":"https://example.invalid"}}}"#;
        let oracle2: ClaudeConfigOracle = serde_json::from_str(dup_server_to_http).unwrap();
        assert_eq!(
            claude_servers(dup_server_to_http),
            oracle_signatures(oracle2.mcp_servers.into_values()),
            "duplicate server overwritten by http mismatch"
        );

        let dup_project = r#"{"projects":{"/p":{"mcpServers":{"a":{"command":"old","args":["1"]}}},"/p":{"mcpServers":{}}}}"#;
        let oracle3: ClaudeConfigOracle = serde_json::from_str(dup_project).unwrap();
        assert_eq!(
            claude_servers(dup_project),
            oracle_signatures(
                oracle3
                    .projects
                    .into_values()
                    .flat_map(|p| p.mcp_servers.into_values())
            ),
            "duplicate project overwritten by empty mismatch"
        );

        // 3. Duplicate struct fields rejected by serde_json::from_str::<ClaudeConfigOracle>.
        for dup_field in [
            r#"{"mcpServers":{"a":{"command":"x"}},"mcpServers":{"b":{"command":"y"}}}"#,
            r#"{"projects":{"/a":{"mcpServers":{"a":{"command":"x"}}}},"projects":{}}"#,
            r#"{"projects":{"/a":{"mcpServers":{"a":{"command":"x"}},"mcpServers":{}}}}"#,
        ] {
            assert!(
                serde_json::from_str::<ClaudeConfigOracle>(dup_field).is_err(),
                "oracle unexpectedly accepted: {dup_field}"
            );
            assert!(
                claude_servers(dup_field).is_empty(),
                "JsonParser accepted duplicate struct field: {dup_field}"
            );
        }
    }

    #[test]
    fn extra_mcp_configs_parse_antigravity_opencode_plugins_and_projects() {
        // Antigravity mcp_config.json + project .mcp.json (`mcpServers` wrapper).
        let antigravity = r#"{
            "mcpServers": {
                "codegraph": { "command": "/Users/u/.local/bin/codegraph", "args": ["serve", "--mcp"] },
                "remote": { "url": "https://example.invalid/mcp" }
            }
        }"#;
        assert_eq!(
            mcp_json_servers_from_reader(io::Cursor::new(antigravity.as_bytes())),
            [ServerSignature {
                command: "/Users/u/.local/bin/codegraph".into(),
                args: smallvec!["serve".into(), "--mcp".into()],
            }]
        );

        // Flat Claude plugin .mcp.json (no `mcpServers` wrapper).
        let flat_plugin = r#"{
            "playwright": { "command": "npx", "args": ["-y", "@playwright/mcp@latest"] },
            "http_only": { "type": "http", "url": "https://example.invalid" }
        }"#;
        assert_eq!(
            mcp_json_servers_from_reader(io::Cursor::new(flat_plugin.as_bytes())),
            [ServerSignature {
                command: "npx".into(),
                args: smallvec!["-y".into(), "@playwright/mcp@latest".into()],
            }]
        );

        // OpenCode opencode.jsonc with comments, trailing commas, command array,
        // and nested `"mcp": { "servers": { ... } }` map.
        let opencode = r#"{
            // OpenCode MCP config
            "mcp": {
                "railway": { "type": "local", "command": ["railway", "mcp"], "enabled": true, },
                /* remote server */
                "context7": { "type": "remote", "url": "https://mcp.example.invalid/not//comment" },
                "custom": { "command": "mcp-grafana", "args": ["--disable-write"], },
                "servers": {
                    "codegraph": { "type": "local", "command": ["codegraph", "serve", "--mcp"] },
                },
            },
        }"#;
        assert_eq!(
            opencode_servers_from_reader(io::Cursor::new(opencode.as_bytes())),
            [
                ServerSignature {
                    command: "codegraph".into(),
                    args: smallvec!["serve".into(), "--mcp".into()],
                },
                ServerSignature {
                    command: "mcp-grafana".into(),
                    args: smallvec!["--disable-write".into()],
                },
                ServerSignature {
                    command: "railway".into(),
                    args: smallvec!["mcp".into()],
                },
            ]
        );

        // JSONC comments / trailing commas in project `.mcp.json` or Gemini `settings.json`.
        let jsonc_mcp = r#"{
            // comment
            "mcpServers": {
                "creative": { "command": "node", "args": ["./mcp/server.bundle.mjs",], },
            },
        }"#;
        assert_eq!(
            mcp_json_servers_from_reader(io::Cursor::new(jsonc_mcp.as_bytes())),
            [ServerSignature {
                command: "node".into(),
                args: smallvec!["./mcp/server.bundle.mjs".into()],
            }]
        );

        // Claude config extracts both servers and project paths in one pass.
        let claude = r#"{
            "mcpServers": { "r": { "command": "railway", "args": ["mcp"] } },
            "projects": {
                "/Users/u/proj-a": { "mcpServers": {} },
                "/Users/u/proj-b": { "mcpServers": { "g": { "command": "gopls", "args": ["mcp"] } } }
            }
        }"#;
        let (servers, projects) = claude_config_from_reader(io::Cursor::new(claude.as_bytes()));
        assert_eq!(servers.len(), 2);
        assert_eq!(projects, ["/Users/u/proj-a", "/Users/u/proj-b"]);

        // Codex single-pass config extraction (both mcp_servers and projects).
        let codex_toml = "[mcp_servers.r]\ncommand = \"railway\"\nargs = [\"mcp\"]\n[projects.\"/Users/u/repo1\"]\ntrust_level = \"trusted\"\n[projects.\"/Users/u/repo2\"]\ntrust_level = \"trusted\"\n";
        let (codex_srvs, mut codex_projects) =
            codex_config_from_reader(io::Cursor::new(codex_toml.as_bytes()));
        assert_eq!(codex_srvs.len(), 1);
        assert_eq!(codex_srvs[0].command, "railway");
        codex_projects.sort();
        assert_eq!(codex_projects, ["/Users/u/repo1", "/Users/u/repo2"]);
        assert_eq!(
            codex_project_paths_from_reader(io::Cursor::new(codex_toml.as_bytes())).len(),
            2
        );

        let installed = r#"{
            "version": 2,
            "plugins": {
                "p@m": [{ "scope": "user", "installPath": "/Users/u/.claude/plugins/cache/p/1.0" }]
            }
        }"#;
        assert_eq!(
            claude_installed_plugin_paths_from_reader(io::Cursor::new(installed.as_bytes())),
            ["/Users/u/.claude/plugins/cache/p/1.0"]
        );
        // Zed `settings.json` (`context_servers` with object `command: {path, args}`) and VS Code `mcp.json` (`servers`) with BOM.
        let zed = "\u{feff}{\n  // Zed MCP config\n  \"context_servers\": {\n    \"pw\": { \"command\": { \"path\": \"npx\", \"args\": [\"-y\", \"@playwright/mcp@latest\"] } },\n  }\n}";
        assert_eq!(
            opencode_servers_from_reader(io::Cursor::new(zed.as_bytes())),
            [ServerSignature {
                command: "npx".into(),
                args: smallvec!["-y".into(), "@playwright/mcp@latest".into()],
            }]
        );
        let vscode = "\u{feff}{\"servers\":{\"r\":{\"type\":\"stdio\",\"command\":\"railway\",\"args\":[\"mcp\"]}}}";
        assert_eq!(
            mcp_json_servers_from_reader(io::Cursor::new(vscode.as_bytes())),
            [ServerSignature {
                command: "railway".into(),
                args: smallvec!["mcp".into()],
            }]
        );
    }

    #[test]
    fn parse_tty_id_distinguishes_controlling_terminals() {
        assert_eq!(super::parse_tty_id("??"), 0);
        assert_eq!(super::parse_tty_id("?"), 0);
        assert_eq!(super::parse_tty_id(""), 0);
        assert_eq!(super::parse_tty_id("console"), 1);
        let t0 = super::parse_tty_id("ttys000");
        let t1 = super::parse_tty_id("ttys001");
        let t2 = super::parse_tty_id("ttys002");
        assert_ne!(t0, 0);
        assert_ne!(t0, t1);
        assert_ne!(t1, t2);
        assert_eq!(t1, super::parse_tty_id("s001"));
        assert_ne!(t1, super::parse_tty_id("s+1"));
        assert_ne!(super::parse_tty_id("ttyp0"), 0);
        assert_ne!(super::parse_tty_id("ttyp0"), 1);
    }
}
