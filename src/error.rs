use std::io;

#[derive(Debug, thiserror::Error)]
pub(crate) enum AppError {
    #[cfg(not(target_os = "macos"))]
    #[error("agentwarden supports macOS only")]
    Unsupported,
    #[error("HOME is not set")]
    NoHome,
    #[error("cannot run {program}: {source}")]
    Command {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("{program} failed: {status}")]
    CommandFailed { program: String, status: String },
    #[error("cannot write agentwarden state or log: {0}")]
    Store(#[source] io::Error),
    #[error("cannot install the LaunchAgent: {0}")]
    Install(#[source] io::Error),
    #[error("cannot encode JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cannot write standard output: {0}")]
    Output(#[source] io::Error),
}

impl AppError {
    pub fn is_stdout_closed(&self) -> bool {
        matches!(self, Self::Output(error) if error.kind() == io::ErrorKind::BrokenPipe)
    }
}

/// Keep untrusted paths and parser messages from issuing terminal commands.
pub(crate) fn diagnostic(error: &impl std::fmt::Display) -> String {
    let mut message = String::new();
    for character in error.to_string().chars() {
        if character.is_control() {
            message.extend(character.escape_default());
        } else {
            message.push(character);
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::{AppError, diagnostic};
    use std::io;

    #[test]
    fn only_stdout_broken_pipe_is_a_clean_pipeline_end() {
        assert!(AppError::Output(io::ErrorKind::BrokenPipe.into()).is_stdout_closed());
        assert!(!AppError::Store(io::ErrorKind::BrokenPipe.into()).is_stdout_closed());
        assert!(!AppError::Output(io::ErrorKind::PermissionDenied.into()).is_stdout_closed());
    }

    #[test]
    fn diagnostics_escape_terminal_controls() {
        assert_eq!(
            diagnostic(&"bad\u{1b}[2J\npath\t"),
            "bad\\u{1b}[2J\\npath\\t"
        );
    }
}
