//! Parsers for the text that macOS system commands print, and for agent MCP
//! configuration. Pure functions, so they are tested on every platform.

use std::collections::HashMap;

use serde::Deserialize;

use crate::model::{Pressure, ServerSignature, Swap};

/// Fields from `ps -axww -o pid=,ppid=,uid=,tty=,time=,etime=,lstart=`.
#[derive(Debug, PartialEq, Eq)]
pub struct PsRow {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    pub has_tty: bool,
    pub cpu_centis: u64,
    pub age_secs: u64,
    pub started: String,
}

pub fn ps_rows(text: &str) -> Vec<PsRow> {
    text.lines().filter_map(ps_row).collect()
}

fn ps_row(line: &str) -> Option<PsRow> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    // pid ppid uid tty time etime + five lstart words (Day Mon DD HH:MM:SS YYYY).
    let [pid, ppid, uid, tty, time, etime, rest @ ..] = fields.as_slice() else {
        return None;
    };
    if rest.len() != 5 {
        return None;
    }
    Some(PsRow {
        pid: pid.parse().ok()?,
        ppid: ppid.parse().ok()?,
        uid: uid.parse().ok()?,
        has_tty: !tty.starts_with('?'),
        cpu_centis: clock_centis(time)?,
        age_secs: clock_centis(etime)? / 100,
        started: rest.join(" "),
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
            let fraction = format!("{fraction:0<2}");
            centis = centis.checked_add(fraction.get(..2)?.parse::<u64>().ok()?)?;
        }
    }
    days.checked_mul(86_400 * 100)?.checked_add(centis)
}

/// `ps -axww -o pid=,<column>=`: PID, then the rest of the line verbatim.
pub fn pid_column(text: &str) -> HashMap<i32, String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            Some((pid.parse().ok()?, rest.trim().to_owned()))
        })
        .collect()
}

/// `top -l 1 -stats pid,mem`: the MEM column is the physical footprint.
pub fn top_footprints(text: &str) -> HashMap<i32, u64> {
    text.lines()
        .skip_while(|line| !line.trim_start().starts_with("PID"))
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse().ok()?, size_bytes(fields.next()?)?))
        })
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

/// `ioreg -c IOHIDSystem -d 4`: `"HIDIdleTime" = <nanoseconds>`.
pub fn hid_idle_secs(text: &str) -> Option<u64> {
    let line = text.lines().find(|line| line.contains("\"HIDIdleTime\""))?;
    let nanos: u64 = line.rsplit('=').next()?.trim().parse().ok()?;
    Some(nanos / 1_000_000_000)
}

/// `launchctl list`: `PID Status Label`, PID `-` when the job is not running.
pub fn launchd_pids(text: &str) -> Vec<i32> {
    text.lines()
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

#[derive(Deserialize)]
struct CodexConfig {
    #[serde(default)]
    mcp_servers: HashMap<String, StdioServer>,
}

#[derive(Deserialize)]
struct ClaudeConfig {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: HashMap<String, StdioServer>,
    #[serde(default)]
    projects: HashMap<String, ClaudeProject>,
}

#[derive(Deserialize)]
struct ClaudeProject {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: HashMap<String, StdioServer>,
}

#[derive(Deserialize)]
struct StdioServer {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
}

/// Stdio servers in `~/.codex/config.toml`; servers with a URL are skipped.
pub fn codex_servers(text: &str) -> Vec<ServerSignature> {
    toml::from_str::<CodexConfig>(text)
        .map(|config| signatures(config.mcp_servers.into_values()))
        .unwrap_or_default()
}

/// Stdio servers in `~/.claude.json`, user scope and every project scope.
pub fn claude_servers(text: &str) -> Vec<ServerSignature> {
    serde_json::from_str::<ClaudeConfig>(text)
        .map(|config| {
            let projects = config
                .projects
                .into_values()
                .flat_map(|project| project.mcp_servers.into_values());
            signatures(config.mcp_servers.into_values().chain(projects))
        })
        .unwrap_or_default()
}

fn signatures(servers: impl Iterator<Item = StdioServer>) -> Vec<ServerSignature> {
    let mut signatures: Vec<ServerSignature> = servers
        .filter_map(|server| {
            Some(ServerSignature {
                command: server.command.filter(|command| !command.is_empty())?,
                args: server.args,
            })
        })
        .collect();
    signatures.sort_by(|a, b| (&a.command, &a.args).cmp(&(&b.command, &b.args)));
    signatures.dedup();
    signatures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_rows_parse_cpu_age_tty_and_start() {
        let text = "\
    1     0     0 ??        37:14.55    4-03:10:30 Fri Oct  2 18:15:22 2026
 9161  9160   501 ??         1:02:03.4     02:34:55 Tue Oct  6 12:37:26 2026
 9200  9161   501 ttys001    0:00.01        00:05 Tue Oct  6 15:12:20 2026
garbage line
";
        let rows = ps_rows(text);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].cpu_centis, (37 * 60 + 14) * 100 + 55);
        assert_eq!(rows[0].age_secs, 4 * 86_400 + 3 * 3600 + 10 * 60 + 30);
        assert_eq!(rows[0].started, "Fri Oct 2 18:15:22 2026");
        assert!(!rows[1].has_tty);
        assert_eq!(rows[1].cpu_centis, (3600 + 2 * 60 + 3) * 100 + 40);
        assert!(rows[2].has_tty);
        assert_eq!(rows[2].age_secs, 5);
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
        let text = "Processes: 3 total\nLoad Avg: 1\n\nPID    MEM\n99916  2176K\n97126  98M+\n53027  1.5G-\n";
        let map = top_footprints(text);
        assert_eq!(map[&99916], 2176 * 1024);
        assert_eq!(map[&97126], 98 * 1024 * 1024);
        assert_eq!(map[&53027], (1.5 * 1024.0 * 1024.0 * 1024.0) as u64);
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
}
