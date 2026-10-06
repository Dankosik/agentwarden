use clap::{Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

#[derive(Debug, Parser)]
#[command(version, about, arg_required_else_help = true)]
pub(crate) struct Cli {
    /// Output format (precedence: flag, environment, text)
    #[arg(
        long,
        global = true,
        env = "AGENTWARDEN_FORMAT",
        hide_env_values = true,
        value_enum,
        default_value_t = Format::Text
    )]
    pub format: Format,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Show who holds memory, what the rules would do now, and recent actions
    Status,
    /// Apply the rules once now
    Reclaim {
        /// Print the plan without stopping or restarting anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Apply the rules every 60 seconds; the LaunchAgent runs this
    Watch,
    /// Install the per-user LaunchAgent that runs `watch` (idempotent)
    Install {
        /// Stop and remove the LaunchAgent instead
        #[arg(long)]
        uninstall: bool,
    },
    /// Write a shell completion script to stdout
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum Format {
    #[default]
    Text,
    Json,
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{Cli, Command, Format};

    #[test]
    fn command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn global_format_applies_to_every_command() {
        let cli = Cli::try_parse_from(["agentwarden", "reclaim", "--dry-run", "--format", "json"])
            .unwrap();
        assert_eq!(cli.format, Format::Json);
        assert!(matches!(cli.command, Command::Reclaim { dry_run: true }));
        assert!(Cli::try_parse_from(["agentwarden", "--config", "x", "status"]).is_err());
    }
}
