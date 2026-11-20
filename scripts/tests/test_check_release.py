"""Exercise release preflight failures offline using temporary manifests and binaries."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "check-release.sh"


class ReleasePreflightTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="bk release ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "scripts").mkdir()
        self.script = self.root / "scripts/check-release.sh"
        shutil.copyfile(SCRIPT, self.script)
        (self.root / "beankeeper-cli").mkdir()
        self.manifest = self.root / "beankeeper-cli/Cargo.toml"
        self.write_version("0.21.0")
        self.changelog = self.root / "CHANGELOG.md"
        self.changelog.write_text("# Changelog\n\n## [0.21.0] - Unreleased\n")
        self.binary = self.root / "target/debug/bk"
        self.write_binary(self.binary, "bk 0.21.0")
        self.env = os.environ.copy()
        self.env.pop("CARGO_TARGET_DIR", None)

    def write_version(self, version):
        self.manifest.write_text(
            f'[package]\nname = "beankeeper-cli"\nversion = "{version}"\n'
            '[dependencies.example]\nversion = "99.0.0"\n'
        )

    @staticmethod
    def write_binary(path, output):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            '#!/bin/sh\n[ "$#" = 1 ] && [ "$1" = --version ] || exit 2\n'
            f"printf '%s\\n' '{output}'\n"
        )
        path.chmod(0o755)

    def run_check(self, *args, error=None):
        result = subprocess.run(
            ["sh", str(self.script), *map(str, args)],
            cwd=self.root.parent,
            env=self.env,
            capture_output=True,
            text=True,
            check=False,
        )
        if error is None:
            self.assertEqual(result.returncode, 0, result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn(error, result.stderr)
        return result

    def test_matching_debug_binary_from_another_directory(self):
        result = self.run_check()
        self.assertIn("Release preflight OK: bk 0.21.0", result.stdout)

    def test_explicit_release_binary(self):
        binary = self.root / "custom/release/bk"
        self.write_binary(binary, "bk 0.21.0")
        self.run_check(binary)

    def test_custom_target_directory(self):
        for target in (self.root / "build output", Path("relative output")):
            with self.subTest(target=target):
                resolved = target if target.is_absolute() else self.root / target
                self.write_binary(resolved / "debug/bk", "bk 0.21.0")
                self.env["CARGO_TARGET_DIR"] = str(target)
                self.run_check()

    def test_invalid_stable_versions(self):
        for version in ("0.21", "v0.21.0", "0.21.0-rc.1", "0.21.0+build", "00.21.0"):
            with self.subTest(version=version):
                self.write_version(version)
                self.run_check(error="must declare a stable X.Y.Z")

    def test_missing_package_version(self):
        self.manifest.write_text('[dependencies.example]\nversion = "0.21.0"\n')
        self.run_check(error="must declare a stable X.Y.Z")

    def test_changelog_mismatch_is_not_satisfied_by_prose_or_similar_version(self):
        self.changelog.write_text("# Changelog\nMentions [0.21.0]\n## [0.210.0]\n")
        self.run_check(error="needs a section headed ## [0.21.0]")

    def test_missing_changelog(self):
        self.changelog.unlink()
        self.run_check(error="CHANGELOG.md is missing")

    def test_stale_binary(self):
        self.write_binary(self.binary, "bk 0.10.0")
        self.run_check(error="binary reports 'bk 0.10.0', expected 'bk 0.21.0'")

    def test_missing_binary(self):
        self.binary.unlink()
        self.run_check(error="build bk first")

    def test_binary_execution_failure(self):
        self.binary.write_text("#!/bin/sh\nexit 7\n")
        self.run_check(error="could not execute")

    def test_unexpected_binary_output(self):
        self.write_binary(self.binary, "bk 0.21.0\nextra output")
        self.run_check(error="expected 'bk 0.21.0'")

    def test_matching_tag(self):
        self.run_check("--tag", "beankeeper-cli-v0.21.0")

    def test_wrong_tag(self):
        for tag in ("beankeeper-cli-v0.10.0", "beankeeper-v0.21.0", "v0.21.0"):
            with self.subTest(tag=tag):
                self.run_check("--tag", tag, error="must equal beankeeper-cli-v0.21.0")

    def test_metadata_only_needs_no_build(self):
        self.binary.unlink()
        self.run_check("--metadata-only")

    def test_metadata_only_still_rejects_changelog_mismatch(self):
        self.changelog.write_text("## [0.10.0]\n")
        self.run_check("--metadata-only", error="needs a section headed ## [0.21.0]")

    def test_metadata_only_still_rejects_tag_mismatch(self):
        self.run_check("--metadata-only", "--tag", "wrong", error="must equal")

    def test_invalid_arguments(self):
        for args, message in (
            (("--unknown",), "unknown option"),
            (("--tag",), "--tag requires a tag name"),
            (("--tag", ""), "--tag requires a tag name"),
            (("one", "two"), "provide only one binary path"),
            (("--metadata-only", self.binary), "cannot be combined"),
        ):
            with self.subTest(args=args):
                self.run_check(*args, error=message)


if __name__ == "__main__":
    unittest.main()
