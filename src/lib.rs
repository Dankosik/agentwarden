//! agentwarden keeps idle AI coding agent helpers from filling a Mac's memory.

mod actions;
mod cli;
mod error;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod install;
mod model;
mod output;
mod owners;
mod platform;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod probe;
mod report;
mod rules;
mod state;
mod store;

use std::{io, io::Write, process::ExitCode, time::Duration};

use clap::{CommandFactory, Parser};

use crate::{actions::System, cli::Cli, error::AppError, store::Store};

const WATCH_INTERVAL: Duration = Duration::from_secs(60);

/// Exit status of `reclaim` when at least one action failed.
const PARTIAL: u8 = 3;

/// Parse process arguments, perform the requested command, and report once.
pub fn run() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => return report_parser_error(error),
    };
    match execute(cli, &mut io::stdout().lock()) {
        Ok(code) => code,
        Err(error) if error.is_stdout_closed() => ExitCode::SUCCESS,
        Err(error) => {
            let mut stderr = io::stderr().lock();
            // Reporting failure cannot turn an already failed operation into success.
            let _ = writeln!(stderr, "error: {}", error::diagnostic(&error));
            let _ = stderr.flush();
            ExitCode::FAILURE
        }
    }
}

fn execute(cli: Cli, stdout: &mut impl Write) -> Result<ExitCode, AppError> {
    match cli.command {
        cli::Command::Completions { shell } => {
            let mut command = Cli::command();
            let name = command.get_name().to_owned();
            // The generator's writer API is infallible. Generate this bounded,
            // known command definition in memory, then handle stdout ourselves.
            let mut bytes = Vec::new();
            clap_complete::generate(shell, &mut command, name, &mut bytes);
            output::write_bytes(stdout, &bytes)?;
            Ok(ExitCode::SUCCESS)
        }
        cli::Command::Status => {
            let (system, store) = environment()?;
            let report = report::pass(&system, &store, false)?;
            output::report(stdout, &report, cli.format)?;
            Ok(ExitCode::SUCCESS)
        }
        cli::Command::Reclaim { dry_run } => {
            let (system, store) = environment()?;
            let report = report::pass(&system, &store, !dry_run)?;
            output::report(stdout, &report, cli.format)?;
            Ok(if report.has_failures() {
                ExitCode::from(PARTIAL)
            } else {
                ExitCode::SUCCESS
            })
        }
        cli::Command::Watch => {
            let (system, store) = environment()?;
            loop {
                // One failed pass must not stop the next; launchd keeps stderr.
                if let Err(error) = report::pass(&system, &store, true) {
                    let mut stderr = io::stderr().lock();
                    let _ = writeln!(stderr, "pass failed: {}", error::diagnostic(&error));
                }
                system.sleep(WATCH_INTERVAL);
            }
        }
        cli::Command::Install { uninstall } => {
            let message = install(uninstall)?;
            output::line(stdout, &message)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn environment() -> Result<(impl System, Store), AppError> {
    let system = platform::system()?;
    Ok((system, Store::for_home(&platform::home()?)))
}

#[cfg(target_os = "macos")]
fn install(uninstall: bool) -> Result<String, AppError> {
    platform::macos::install(&platform::home()?, uninstall)
}

#[cfg(not(target_os = "macos"))]
fn install(_uninstall: bool) -> Result<String, AppError> {
    Err(AppError::Unsupported)
}

fn report_parser_error(mut error: clap::Error) -> ExitCode {
    let to_stderr = error.use_stderr();
    if to_stderr {
        use clap::error::{ContextKind, ContextValue};

        // Escape user-provided fragments before clap adds trusted styling and
        // layout. Sanitizing the finished message would also damage its help.
        let replacements: Vec<_> = error
            .context()
            .filter_map(|(kind, value)| {
                let sanitized = match value {
                    ContextValue::String(value) if value.chars().any(char::is_control) => {
                        ContextValue::String(error::diagnostic(value))
                    }
                    ContextValue::Strings(values)
                        if values
                            .iter()
                            .any(|value| value.chars().any(char::is_control)) =>
                    {
                        ContextValue::Strings(values.iter().map(error::diagnostic).collect())
                    }
                    _ => return None,
                };
                Some((kind, sanitized))
            })
            .collect();
        if !replacements.is_empty() {
            // clap's preformatted suggestions can repeat the original fragment.
            // Keep the escaped error and usage, without that unsafe duplicate.
            error.remove(ContextKind::Suggested);
            for (kind, value) in replacements {
                error.insert(kind, value);
            }
        }
    }
    let result = error.print().and_then(|()| {
        if to_stderr {
            io::stderr().lock().flush()
        } else {
            io::stdout().lock().flush()
        }
    });
    match result {
        Ok(()) => ExitCode::from(error.exit_code() as u8),
        Err(error) if !to_stderr && error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
