import gzip
import hashlib
import os
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[3]
APT_INSTALL = ROOT / "scripts/ci/apt_install.sh"
RELEASE = "mirror_dists_noble_InRelease"
PACKAGES = "mirror_dists_noble_main_binary-amd64_Packages"
EXTRA_PACKAGES = "mirror_dists_noble_universe_binary-amd64_Packages"
ARCHIVE = "example_1%3a2.0_amd64.deb"


def package_record(package, version, content):
    return (f"Package: {package}\nVersion: {version}\nArchitecture: amd64\n"
            f"Size: {len(content)}\nSHA256: {hashlib.sha256(content).hexdigest()}\n\n").encode()


class AptInstallTests(unittest.TestCase):
    def run_apt_install(self, mode: str, *, cache: str = ""):
        with tempfile.TemporaryDirectory() as directory:
            temp = pathlib.Path(directory)
            fake_sudo = temp / "sudo"
            command_log = temp / "sudo-argv.log"
            state = temp / "state"
            archives = temp / "archives"
            archives.mkdir()
            lists = temp / "lists"
            lists.mkdir()
            (lists / "mirror_Packages").write_text("runner versions")
            (lists / "lock").touch()
            (lists / "partial").mkdir()
            (lists / "partial" / "incomplete_Packages").touch()
            cache_dir = temp / "cache"
            output = temp / "output"
            output.touch()
            for name, body in {
                "gpgv": (
                    'input=${!#}\n'
                    '[[ $* == *"--keyring /usr/share/keyrings/ubuntu-archive-keyring.gpg"* ]] || exit 2\n'
                    '[[ $(tail -n 1 "$input") == "-----END PGP SIGNATURE-----" ]] || exit 1\n'
                    'head -n -3 "$input" | tail -n +4\n'
                    'grep -qx valid-signature "$input"\n'
                ),
                "apt-get": (
                    '[[ $1 == indextargets && $2 == --no-release-info ]] || exit 2\n'
                    'for component in main universe; do\n'
                    '  printf "MetaKey: %s/binary-amd64/Packages\\nFilename: %s/mirror_dists_noble_%s_binary-amd64_Packages\\nRelease: noble\\nSourcesentry: %s/ubuntu.sources:1\\n\\n" "$component" "$APT_LISTS_DIR" "$component" "$FAKE_APT_ROOT"\n'
                    'done\n'
                ),
                "apt-helper": '[[ $1 == cat-file ]] && gzip -dc -- "$2"\n',
            }.items():
                path = temp / name
                path.write_text(f"#!/usr/bin/env bash\nset -euo pipefail\n{body}")
                path.chmod(0o755)
            (temp / "ubuntu.sources").write_text(
                "Types: deb\nSigned-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n"
            )
            if cache and cache != "cold":
                cache_dir.mkdir()
                (cache_dir / ARCHIVE).write_bytes(
                    b"wrong! deb" if cache == "bad_archive" else b"cached deb"
                )
                cached_lists = cache_dir / "lists"
                cached_lists.mkdir()
                packages = package_record("example", "1:2.0", b"cached deb")
                extra = package_record("extra", "1", b"extra deb")
                if cache == "warm_superseded":
                    packages += package_record("superseded", "1", b"old deb")
                    (cache_dir / "superseded_1_amd64.deb").write_bytes(b"old deb")
                checksums = "".join(
                    f" {hashlib.sha256(data).hexdigest()} {len(data)} {component}/binary-amd64/Packages\n"
                    for component, data in [("main", packages), ("universe", extra)]
                )
                signature = "bad-signature" if cache == "bad_signature" else "valid-signature"
                (cached_lists / RELEASE).write_text(
                    "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\n"
                    f"Suite: noble\nSHA256:\n{checksums}"
                    f"-----BEGIN PGP SIGNATURE-----\n{signature}\n-----END PGP SIGNATURE-----\n"
                )
                (cached_lists / PACKAGES).write_bytes(packages)
                if cache == "bad_index":
                    extra = extra.replace(b"Package: extra", b"Package: wrong")
                (cached_lists / f"{EXTRA_PACKAGES}.gz").write_bytes(gzip.compress(extra))
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
                "    printf 'updated versions' > \"$APT_LISTS_DIR/mirror_Packages\"\n"
                "    rm -f -- \"$APT_LISTS_DIR\"/mirror_dists_noble_*\n"
                "    exit 0\n"
                "    ;;\n"
                "  'apt-get install')\n"
                "    count=0\n"
                "    if [[ -f $FAKE_SUDO_STATE ]]; then\n"
                "      count=$(<\"$FAKE_SUDO_STATE\")\n"
                "    fi\n"
                "    count=$((count + 1))\n"
                "    printf '%s\\n' \"$count\" > \"$FAKE_SUDO_STATE\"\n"
                "    if [[ $FAKE_SUDO_MODE == require_cached_lists && $count -eq 1 ]]; then\n"
                f"      [[ -s $APT_LISTS_DIR/{PACKAGES} ]] || exit 1\n"
                "    fi\n"
                "    if [[ -n ${APT_DEB_CACHE_DIR:-} ]]; then\n"
                "      [[ $* == *APT::Keep-Downloaded-Packages=true* ]] || exit 2\n"
                f"      if [[ -f $APT_ARCHIVES_DIR/{ARCHIVE} ]]; then\n"
                "        echo 'Need to get 0 B/1 B of archives.'\n"
                "      else\n"
                "        echo 'Need to get 1 B of archives.'\n"
                f"        printf 'downloaded deb' > \"$APT_ARCHIVES_DIR/{ARCHIVE}\"\n"
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
                "  'apt-get autoclean')\n"
                "    rm -f -- \"$APT_ARCHIVES_DIR\"/superseded_*.deb\n"
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
                "APT_LISTS_DIR": str(lists),
                "APT_ATTEMPTS": "1",
                "APT_DEB_CACHE_DIR": str(cache_dir) if cache else "",
                "APT_UPDATE_TIMEOUT": "1",
                "APT_INSTALL_TIMEOUT": "1",
                "FAKE_SUDO_LOG": str(command_log),
                "FAKE_APT_ROOT": str(temp),
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
            deb = cache_dir / ARCHIVE
            cached_deb = deb.read_text() if deb.exists() else None
            self.cache_files = sorted(path.name for path in cache_dir.glob("*.deb"))
            cached_lists = cache_dir / "lists"
            self.cached_lists = {
                str(path.relative_to(cached_lists)): path.read_bytes() if path.is_file() else None
                for path in cached_lists.rglob("*")
            }
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

    def test_same_size_wrong_hash_archive_is_dropped(self):
        result, commands, deb, _ = self.run_apt_install("install_first_success", cache="bad_archive")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("apt_install: dropped 1 unverified cached archives", result.stderr)
        self.assertIn("Need to get 1 B of archives.", result.stdout)
        self.assertFalse(any("apt-get update" in command for command in commands))
        self.assertEqual(deb, "downloaded deb")

    def assert_rejected_lists(self, cache):
        result, commands, _, _ = self.run_apt_install("install_first_success", cache=cache)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("apt_install: restored package lists (0 files)", result.stdout)
        self.assertFalse(any(command.startswith("install -m") for command in commands))
        self.assertIn("apt-get update", commands[0])

    def test_bad_signature_rejects_cached_lists(self):
        self.assert_rejected_lists("bad_signature")

    def test_index_hash_mismatch_rejects_all_cached_lists(self):
        self.assert_rejected_lists("bad_index")

    def test_cold_cache_stages_deb_after_retry_succeeds(self):
        result, commands, deb, output = self.run_apt_install("stale_lists", cache="cold")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("apt_install: restored 0 cached .deb file(s)", result.stdout)
        self.assertIn("apt_install: staged 1 .deb file(s) for cache save", result.stdout)
        self.assertTrue(any("apt-get update" in command for command in commands))
        self.assertEqual(deb, "downloaded deb")
        self.assertEqual(output, "cache-ready=true\n")

    def test_warm_cache_restores_lists_before_initial_install(self):
        result, commands, _, _ = self.run_apt_install("require_cached_lists", cache="warm_lists")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("apt-get update skipped", result.stdout)
        self.assertIn("apt_install: restored package lists (3 files)", result.stdout)
        self.assertIn("Need to get 0 B/1 B of archives.", result.stdout)
        self.assertFalse(any("apt-get update" in command for command in commands))

    def test_warm_cache_replaces_lists_after_fallback_update(self):
        result, commands, _, output = self.run_apt_install("stale_lists", cache="warm_lists")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(any("apt-get update" in command for command in commands))
        self.assertIn("apt_install: restored package lists (3 files)", result.stdout)
        self.assertEqual(self.cached_lists, {"mirror_Packages": b"updated versions"})
        self.assertEqual(output, "cache-ready=true\n")

    def test_cold_cache_stages_lists_after_success(self):
        result, _, _, output = self.run_apt_install("install_first_success", cache="cold")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.cached_lists, {"mirror_Packages": b"runner versions"})
        self.assertEqual(output, "cache-ready=true\n")

    def test_failed_install_does_not_stage_deb(self):
        result, _, deb, output = self.run_apt_install("install_timeout", cache="cold")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("apt_install: all attempts exhausted", result.stderr)
        self.assertIn("Need to get 1 B of archives.", result.stdout)
        self.assertIsNone(deb)
        self.assertEqual(output, "")

    def test_stage_drops_superseded_archives_from_the_cache(self):
        result, commands, _, output = self.run_apt_install(
            "stale_lists", cache="warm_superseded"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("apt_install: restored 2 cached .deb file(s)", result.stdout)
        self.assertTrue(any("apt-get autoclean" in command for command in commands))
        self.assertIn("apt_install: autoclean removed 1 superseded .deb file(s)", result.stdout)
        self.assertIn("apt_install: staged 1 .deb file(s) for cache save", result.stdout)
        self.assertEqual(self.cache_files, [ARCHIVE])
        self.assertEqual(output, "cache-ready=true\n")
