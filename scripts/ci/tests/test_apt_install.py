import os
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[3]
APT_INSTALL = ROOT / "scripts/ci/apt_install.sh"


class AptInstallTests(unittest.TestCase):
    def run_apt_install(self, mode: str):
        with tempfile.TemporaryDirectory() as directory:
            temp = pathlib.Path(directory)
            fake_sudo = temp / "sudo"
            command_log = temp / "sudo-argv.log"
            state = temp / "state"
            fake_sudo.write_text(
                "#!/usr/bin/env bash\n"
                "set -euo pipefail\n"
                "printf '%s\\n' \"$*\" >> \"$FAKE_SUDO_LOG\"\n"
                "if [[ $1 == '-E' ]]; then\n"
                "  shift\n"
                "fi\n"
                "case \"$1 ${2:-}\" in\n"
                "  'apt-get update')\n"
                "    if [[ $FAKE_SUDO_MODE == update_timeout ]]; then\n"
                "      sleep 2\n"
                "    fi\n"
                "    exit 0\n"
                "    ;;\n"
                "  'apt-get install')\n"
                "    count=0\n"
                "    if [[ -f $FAKE_SUDO_STATE ]]; then\n"
                "      count=$(<\"$FAKE_SUDO_STATE\")\n"
                "    fi\n"
                "    count=$((count + 1))\n"
                "    printf '%s\\n' \"$count\" > \"$FAKE_SUDO_STATE\"\n"
                "    case $FAKE_SUDO_MODE in\n"
                "      install_first_success) exit 0 ;;\n"
                "      stale_lists) [[ $count -eq 1 ]] && exit 1 || exit 0 ;;\n"
                "      update_timeout) exit 1 ;;\n"
                "      install_timeout)\n"
                "        if [[ $count -eq 1 ]]; then\n"
                "          exit 1\n"
                "        fi\n"
                "        sleep 2\n"
                "        ;;\n"
                "    esac\n"
                "    ;;\n"
                "  'sed -i') exit 0 ;;\n"
                "esac\n",
                encoding="utf-8",
            )
            fake_sudo.chmod(0o755)
            environment = {
                **os.environ,
                "APT_ATTEMPTS": "1",
                "APT_UPDATE_TIMEOUT": "1",
                "APT_INSTALL_TIMEOUT": "1",
                "FAKE_SUDO_LOG": str(command_log),
                "FAKE_SUDO_MODE": mode,
                "FAKE_SUDO_STATE": str(state),
                "PATH": f"{temp}:{os.environ['PATH']}",
            }
            result = subprocess.run(
                ["bash", str(APT_INSTALL), "libexample-dev"],
                capture_output=True,
                text=True,
                env=environment,
                check=False,
            )
            commands = command_log.read_text(encoding="utf-8").splitlines()

        return result, commands

    def test_skips_update_when_install_uses_populated_lists(self):
        result, commands = self.run_apt_install("install_first_success")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("apt-get update skipped", result.stdout)
        self.assertFalse(any("apt-get update" in command for command in commands))

    def test_updates_after_install_cannot_use_stale_lists(self):
        result, commands = self.run_apt_install("stale_lists")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(any("apt-get update" in command for command in commands))

    def test_reports_update_timeout_with_its_budget(self):
        result, _ = self.run_apt_install("update_timeout")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("apt-get update timed out after 1s", result.stderr)

    def test_reports_install_timeout_with_its_budget(self):
        result, _ = self.run_apt_install("install_timeout")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("apt-get install timed out after 1s", result.stderr)
