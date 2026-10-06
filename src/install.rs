//! The LaunchAgent that keeps `agentwarden watch` running for the user.

use std::path::Path;

pub const LABEL: &str = "io.github.dankosik.agentwarden";

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Runs at login, restarts after a crash, at background priority.
pub fn plist(exe: &Path, home: &Path) -> String {
    let exe = xml_escape(&exe.to_string_lossy());
    let log = xml_escape(&home.join("Library/Logs/agentwarden.log").to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>watch</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ThrottleInterval</key>
	<integer>60</integer>
	<key>ProcessType</key>
	<string>Background</string>
	<key>Nice</key>
	<integer>10</integer>
	<key>LowPriorityIO</key>
	<true/>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::plist;

    #[test]
    fn plist_runs_watch_from_the_installed_binary_and_escapes_paths() {
        let text = plist(
            Path::new("/Users/a&b/.cargo/bin/agentwarden"),
            Path::new("/Users/a&b"),
        );
        assert!(text.contains(
            "<string>/Users/a&amp;b/.cargo/bin/agentwarden</string>\n\t\t<string>watch</string>"
        ));
        assert!(text.contains("<string>/Users/a&amp;b/Library/Logs/agentwarden.log</string>"));
        assert!(text.contains("<key>KeepAlive</key>\n\t<true/>"));
        assert!(!text.contains("a&b"));
    }
}
