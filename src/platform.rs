//! The operating system behind [`System`]. Only macOS is supported.

use std::path::PathBuf;

use crate::{actions::System, error::AppError};

pub fn home() -> Result<PathBuf, AppError> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or(AppError::NoHome)
}

#[cfg(target_os = "macos")]
pub fn system() -> Result<impl System, AppError> {
    // As root, `uid` checks would admit other users' and system processes.
    if nix::unistd::geteuid().is_root() {
        return Err(AppError::Root);
    }
    Ok(macos::MacOs { home: home()? })
}

#[cfg(not(target_os = "macos"))]
pub fn system() -> Result<NoSystem, AppError> {
    Err(AppError::Unsupported)
}

/// Placeholder that never exists at run time on unsupported platforms.
#[cfg(not(target_os = "macos"))]
pub enum NoSystem {}

#[cfg(not(target_os = "macos"))]
impl System for NoSystem {
    fn snapshot(&self) -> Result<crate::model::Snapshot, AppError> {
        match *self {}
    }
    fn identify(&self, _: i32) -> Option<(String, u32)> {
        match *self {}
    }
    fn signal(&self, _: i32, _: crate::actions::Signal) -> std::io::Result<()> {
        match *self {}
    }
    fn launch_app(&self, _: &std::path::Path) -> std::io::Result<()> {
        match *self {}
    }
    fn sleep(&self, _: std::time::Duration) {
        match *self {}
    }
    fn now(&self) -> u64 {
        match *self {}
    }
}

#[cfg(target_os = "macos")]
pub mod macos {
    use std::{
        fs,
        io::{self, Read},
        path::{Path, PathBuf},
        process::{Command, Stdio},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use nix::{
        sys::signal::{Signal as NixSignal, kill},
        unistd::{Pid, getuid},
    };

    use crate::{
        actions::{Signal, System},
        error::AppError,
        model::{Process, Snapshot},
        probe,
    };

    pub struct MacOs {
        pub home: PathBuf,
    }

    /// A system command that has not finished by then is stuck; under heavy
    /// swapping `top` takes seconds, not tens of seconds.
    const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

    /// Run a system command in the C locale, so its output has the format the
    /// parsers read whatever the user's language, and give up when it hangs.
    fn run(program: &str, args: &[&str]) -> Result<String, AppError> {
        let failed = |source| AppError::Command {
            program: program.to_owned(),
            source,
        };
        let mut child = Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(failed)?;
        // Read concurrently: `ps` output overflows the pipe buffer.
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| failed(io::Error::other("no stdout")))?;
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).map(|_| bytes)
        });
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait().map_err(failed)? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AppError::CommandTimedOut {
                    program: program.to_owned(),
                    seconds: COMMAND_TIMEOUT.as_secs(),
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let bytes = reader
            .join()
            .map_err(|_| failed(io::Error::other("output reader panicked")))?
            .map_err(failed)?;
        if !status.success() {
            return Err(AppError::CommandFailed {
                program: program.to_owned(),
                status: status.to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn unix_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }

    /// Newest write to a Codex session file; Codex appends to it on every turn.
    fn codex_activity_at(home: &Path) -> Option<u64> {
        walkdir::WalkDir::new(home.join(".codex/sessions"))
            .max_depth(4)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy();
                name.starts_with("rollout-") && name.ends_with(".jsonl")
            })
            .filter_map(|entry| entry.metadata().ok()?.modified().ok())
            .filter_map(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_secs())
            .max()
    }

    impl System for MacOs {
        fn snapshot(&self) -> Result<Snapshot, AppError> {
            let rows = probe::ps_rows(&run(
                "/bin/ps",
                &[
                    "-axww",
                    "-o",
                    "pid=,ppid=,uid=,tty=,state=,time=,etime=,lstart=",
                ],
            )?)
            .ok_or_else(|| AppError::Unreadable {
                program: "/bin/ps".into(),
            })?;
            let exes = probe::pid_column(&run("/bin/ps", &["-axww", "-o", "pid=,comm="])?);
            let mut args = probe::pid_column(&run("/bin/ps", &["-axww", "-o", "pid=,args="])?);
            let footprints =
                probe::top_footprints(&run("/usr/bin/top", &["-l", "1", "-stats", "pid,mem"])?);
            let processes = rows
                .into_iter()
                // A zombie has exited already; nothing can stop it again.
                .filter(|row| !row.zombie)
                .filter_map(|row| {
                    // A process that exited between the commands is skipped.
                    let exe = exes.get(&row.pid)?.clone();
                    Some(Process {
                        pid: row.pid,
                        ppid: row.ppid,
                        uid: row.uid,
                        started: row.started,
                        age_secs: row.age_secs,
                        has_tty: row.has_tty,
                        cpu_centis: row.cpu_centis,
                        footprint_bytes: footprints.get(&row.pid).copied(),
                        args: args.remove(&row.pid).unwrap_or_else(|| exe.clone()),
                        exe,
                    })
                })
                .collect();
            let read = |path: PathBuf| fs::read_to_string(path).unwrap_or_default();
            let mut mcp_servers = probe::codex_servers(&read(self.home.join(".codex/config.toml")));
            mcp_servers.extend(probe::claude_servers(&read(self.home.join(".claude.json"))));
            Ok(Snapshot {
                taken_at: unix_now(),
                user: getuid().as_raw(),
                pressure: run(
                    "/usr/sbin/sysctl",
                    &["-n", "kern.memorystatus_vm_pressure_level"],
                )
                .ok()
                .and_then(|text| probe::pressure(&text))
                .unwrap_or_default(),
                swap: run("/usr/sbin/sysctl", &["-n", "vm.swapusage"])
                    .ok()
                    .and_then(|text| probe::swap(&text))
                    .unwrap_or_default(),
                user_idle_secs: run("/usr/sbin/ioreg", &["-c", "IOHIDSystem", "-d", "4"])
                    .ok()
                    .and_then(|text| probe::hid_idle_secs(&text)),
                codex_activity_at: codex_activity_at(&self.home),
                launchd_jobs: run("/bin/launchctl", &["list"])
                    .ok()
                    .map(|text| probe::launchd_pids(&text)),
                mcp_servers,
                processes,
            })
        }

        fn identify(&self, pid: i32) -> Option<(String, u32)> {
            let text = run("/bin/ps", &["-o", "uid=,lstart=", "-p", &pid.to_string()]).ok()?;
            let mut words = text.split_whitespace();
            let uid = words.next()?.parse().ok()?;
            let started: Vec<&str> = words.collect();
            (started.len() == 5).then(|| (started.join(" "), uid))
        }

        fn signal(&self, pid: i32, signal: Signal) -> io::Result<()> {
            // 0 and negative PIDs address process groups; 1 is launchd.
            if pid <= 1 {
                return Err(io::Error::other(format!("refusing to signal PID {pid}")));
            }
            let signal = match signal {
                Signal::Term => NixSignal::SIGTERM,
                Signal::Kill => NixSignal::SIGKILL,
            };
            kill(Pid::from_raw(pid), signal).map_err(io::Error::from)
        }

        fn launch_app(&self, bundle: &Path) -> io::Result<()> {
            run(
                "/usr/bin/open",
                &["-g", "-j", "-a", &bundle.to_string_lossy()],
            )
            .map(drop)
            .map_err(|error| io::Error::other(error.to_string()))
        }

        fn sleep(&self, duration: Duration) {
            std::thread::sleep(duration);
        }

        fn now(&self) -> u64 {
            unix_now()
        }
    }

    /// Install or remove the per-user LaunchAgent that runs `watch`.
    pub fn install(home: &Path, uninstall: bool) -> Result<String, AppError> {
        let label = crate::install::LABEL;
        let plist = home.join(format!("Library/LaunchAgents/{label}.plist"));
        let domain = format!("gui/{}", getuid().as_raw());
        let service = format!("{domain}/{label}");
        let loaded = || run("/bin/launchctl", &["print", &service]).is_ok();
        let io_error = |source| AppError::Install(source);
        if uninstall {
            // A failed bootout leaves the agent running; say so instead of "removed".
            if loaded() {
                run("/bin/launchctl", &["bootout", &service])?;
            }
            match fs::remove_file(&plist) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(error)),
            }
            return Ok(format!("removed {label}"));
        }
        let exe = std::env::current_exe()
            .and_then(fs::canonicalize)
            .map_err(io_error)?;
        let contents = crate::install::plist(&exe, home);
        if fs::read_to_string(&plist).is_ok_and(|current| current == contents) && loaded() {
            // The binary may have been replaced in place by an upgrade.
            run("/bin/launchctl", &["kickstart", "-k", &service])?;
            return Ok(format!(
                "{label} already installed; restarted {}",
                exe.display()
            ));
        }
        fs::create_dir_all(plist.parent().unwrap_or(home)).map_err(io_error)?;
        fs::create_dir_all(home.join("Library/Logs")).map_err(io_error)?;
        fs::write(&plist, contents).map_err(io_error)?;
        if loaded() {
            let _ = run("/bin/launchctl", &["bootout", &service]);
        }
        run(
            "/bin/launchctl",
            &["bootstrap", &domain, &plist.to_string_lossy()],
        )?;
        Ok(format!("installed {label}, running {}", exe.display()))
    }
}
