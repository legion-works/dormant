import os
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[3]
APT_INSTALL = ROOT / "scripts/ci/apt_install.sh"


class AptInstallTests(unittest.TestCase):
    def run_apt_install(self, mode: str, *, cache: str = ""):
        with tempfile.TemporaryDirectory() as directory:
            temp = pathlib.Path(directory)
            fake_sudo = temp / "sudo"
            command_log = temp / "sudo-argv.log"
            state = temp / "state"
            archives = temp / "archives"
            archives.mkdir()
            cache_dir = temp / "cache"
            output = temp / "output"
            output.touch()
            if cache == "warm":
                cache_dir.mkdir()
                (cache_dir / "example.deb").write_text("cached deb")
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
                "    if [[ -n ${APT_DEB_CACHE_DIR:-} ]]; then\n"
                "      [[ $* == *APT::Keep-Downloaded-Packages=true* ]] || exit 2\n"
                "      if [[ -f $APT_ARCHIVES_DIR/example.deb ]]; then\n"
                "        echo 'Need to get 0 B/1 B of archives.'\n"
                "      else\n"
                "        echo 'Need to get 1 B of archives.'\n"
                "        printf 'downloaded deb' > \"$APT_ARCHIVES_DIR/example.deb\"\n"
                "      fi\n"
                "    fi\n"
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
                "  'install -m') \"$@\" ;;\n"
                "  'sed -i') exit 0 ;;\n"
                "esac\n",
                encoding="utf-8",
            )
            fake_sudo.chmod(0o755)
            environment = {
                **os.environ,
                "APT_ARCHIVES_DIR": str(archives),
                "APT_ATTEMPTS": "1",
                "APT_DEB_CACHE_DIR": str(cache_dir) if cache else "",
                "APT_UPDATE_TIMEOUT": "1",
                "APT_INSTALL_TIMEOUT": "1",
                "FAKE_SUDO_LOG": str(command_log),
                "FAKE_SUDO_MODE": mode,
                "FAKE_SUDO_STATE": str(state),
                "GITHUB_OUTPUT": str(output),
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
            deb = cache_dir / "example.deb"
            cached_deb = deb.read_text() if deb.exists() else None
            cache_output = output.read_text()

        return result, commands, cached_deb, cache_output

    def test_skips_update_when_install_uses_populated_lists(self):
        result, commands, _, _ = self.run_apt_install("install_first_success")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("apt-get update skipped", result.stdout)
        self.assertFalse(any("apt-get update" in command for command in commands))

    def test_updates_after_install_cannot_use_stale_lists(self):
        result, commands, _, _ = self.run_apt_install("stale_lists")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(any("apt-get update" in command for command in commands))

    def test_reports_update_timeout_with_its_budget(self):
        result, _, _, _ = self.run_apt_install("update_timeout")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("apt-get update timed out after 1s", result.stderr)

    def test_reports_install_timeout_with_its_budget(self):
        result, _, _, _ = self.run_apt_install("install_timeout")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("apt-get install timed out after 1s", result.stderr)

    def test_warm_cache_restores_deb_before_initial_install(self):
        result, commands, deb, _ = self.run_apt_install("install_first_success", cache="warm")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("apt_install: restored 1 cached .deb file(s)", result.stdout)
        self.assertIn("Need to get 0 B/1 B of archives.", result.stdout)
        self.assertFalse(any("apt-get update" in command for command in commands))
        self.assertEqual(deb, "cached deb")

    def test_cold_cache_stages_deb_after_retry_succeeds(self):
        result, commands, deb, output = self.run_apt_install("stale_lists", cache="cold")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("apt_install: restored 0 cached .deb file(s)", result.stdout)
        self.assertIn("apt_install: staged 1 .deb file(s) for cache save", result.stdout)
        self.assertTrue(any("apt-get update" in command for command in commands))
        self.assertEqual(deb, "downloaded deb")
        self.assertEqual(output, "cache-ready=true\n")

    def test_failed_install_does_not_stage_deb(self):
        result, _, deb, output = self.run_apt_install("install_timeout", cache="cold")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("apt_install: all attempts exhausted", result.stderr)
        self.assertIn("Need to get 1 B of archives.", result.stdout)
        self.assertIsNone(deb)
        self.assertEqual(output, "")
