//! The built binary: grammar, status, output framing. `install` is not run here
//! because it changes the user's launchd domain.

use std::time::Duration;

use assert_cmd::Command;

fn command(home: &std::path::Path) -> Command {
    let mut command = Command::cargo_bin("agentwarden").expect("binary is built");
    command
        .env_remove("AGENTWARDEN_FORMAT")
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .timeout(Duration::from_secs(60));
    command
}

#[test]
fn help_lists_the_commands_and_version_prints() {
    let home = tempfile::tempdir().unwrap();
    let output = command(home.path()).arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for name in ["status", "reclaim", "watch", "install", "completions"] {
        assert!(help.contains(name), "{name} missing from help:\n{help}");
    }
    let version = command(home.path()).arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!("agentwarden {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn completions_are_generated_without_touching_the_system() {
    let home = tempfile::tempdir().unwrap();
    let output = command(home.path())
        .args(["completions", "bash"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("agentwarden")
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn there_is_no_configuration_flag() {
    let home = tempfile::tempdir().unwrap();
    let output = command(home.path())
        .args(["--config", "x.toml", "status"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[cfg(target_os = "macos")]
#[test]
fn status_and_dry_run_report_json_and_write_nothing() {
    let home = tempfile::tempdir().unwrap();
    for args in [&["status"][..], &["reclaim", "--dry-run"][..]] {
        let output = command(home.path())
            .args(args)
            .args(["--format", "json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
        for key in [
            "taken_at",
            "pressure",
            "kernel_pressure",
            "swap",
            "user_idle_secs",
            "claude_sessions",
            "codex",
            "orphans",
            "planned",
            "applied",
            "recent",
        ] {
            assert!(report.get(key).is_some(), "{args:?}: {key} missing");
        }
        assert_eq!(report["applied"], serde_json::json!([]));
    }
    assert!(
        !home
            .path()
            .join("Library/Application Support/agentwarden")
            .exists()
    );

    let text = command(home.path()).arg("status").output().unwrap();
    assert!(text.status.success());
    assert!(
        String::from_utf8(text.stdout)
            .unwrap()
            .starts_with("memory: ")
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn other_platforms_get_a_clear_refusal() {
    let home = tempfile::tempdir().unwrap();
    for args in [&["status"][..], &["install"][..]] {
        let output = command(home.path()).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "error: agentwarden supports macOS only\n"
        );
    }
}
