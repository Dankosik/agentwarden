from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

INSTALL = Path(__file__).resolve().parents[2] / "install.sh"


def functions():
    """The installer's helper functions, without its download and install steps."""
    text = INSTALL.read_text(encoding="utf-8")
    start = text.index("say() {")
    return text[start:text.index("\n}\n", text.index("add_to_path() {")) + 3]


@unittest.skipIf(sys.platform == "win32" or shutil.which("sh") is None, "needs a POSIX shell")
class PathTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.home = Path(self.temporary.name)
        self.bin = self.home / ".local/bin"

    def add(self, shell, **env):
        script = functions() + '\nadd_to_path "$1"\n'
        environment = {"HOME": str(self.home), "SHELL": shell, "PATH": "/usr/bin:/bin", **env}
        return subprocess.run(
            ["sh", "-c", script, "sh", str(self.bin)],
            env=environment, capture_output=True, text=True, check=True,
        ).stdout

    def test_zsh_gets_one_line_with_home_kept_symbolic(self):
        output = self.add("/bin/zsh")
        self.add("/bin/zsh")
        profile = (self.home / ".zshrc").read_text()
        self.assertEqual(profile.count('export PATH="$HOME/.local/bin:$PATH"'), 1)
        self.assertIn("open a new terminal", output)

    def test_each_shell_has_its_startup_file(self):
        self.add("/opt/homebrew/bin/fish")
        self.assertIn('fish_add_path "$HOME/.local/bin"',
                      (self.home / ".config/fish/conf.d/agentwarden.fish").read_text())
        self.add("/bin/bash")
        self.assertIn('export PATH="$HOME/.local/bin:$PATH"', (self.home / ".bash_profile").read_text())
        self.add("/bin/dash")
        self.assertIn('export PATH="$HOME/.local/bin:$PATH"', (self.home / ".profile").read_text())

    def test_line_puts_the_directory_on_path_for_a_new_shell(self):
        self.add("/bin/sh")
        result = subprocess.run(
            ["sh", "-c", '. "$HOME/.profile"; printf %s "$PATH"'],
            env={"HOME": str(self.home), "PATH": "/usr/bin:/bin"},
            capture_output=True, text=True, check=True,
        )
        self.assertEqual(result.stdout.split(":")[0], str(self.bin))

    def test_opt_out_changes_nothing(self):
        output = self.add("/bin/zsh", AGENTWARDEN_NO_MODIFY_PATH="1")
        self.assertFalse((self.home / ".zshrc").exists())
        self.assertIn("add " + str(self.bin) + " to PATH", output)


if __name__ == "__main__":
    unittest.main()
