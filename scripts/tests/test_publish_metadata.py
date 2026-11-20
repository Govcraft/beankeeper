"""Check the source packages and crates.io publishing scope without publishing or downloading."""

from pathlib import Path
import subprocess
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[2]


def manifest(crate):
    return tomllib.loads((ROOT / crate / "Cargo.toml").read_text())


class PublishMetadataTests(unittest.TestCase):
    def test_workflows_fetch_dependencies_before_offline_packaging_tests(self):
        for workflow_name in ("ci.yml", "release.yml"):
            with self.subTest(workflow=workflow_name):
                workflow = (ROOT / ".github/workflows" / workflow_name).read_text()
                test_command = "python3 -B -m unittest discover -s scripts/tests -v"
                self.assertLess(workflow.index("cargo fetch --locked"), workflow.index(test_command))

    def test_cli_dependency_matches_local_library(self):
        dependency = manifest("beankeeper-cli")["dependencies"]["beankeeper"]
        self.assertEqual(dependency["version"], manifest("beankeeper")["package"]["version"])
        self.assertEqual((ROOT / "beankeeper-cli" / dependency["path"]).resolve(), ROOT / "beankeeper")

    def test_packages_ship_sources_tests_readme_and_both_licenses(self):
        for crate in ("beankeeper", "beankeeper-cli"):
            with self.subTest(crate=crate):
                package = manifest(crate)["package"]
                for field in ("description", "license", "repository", "readme", "include"):
                    self.assertTrue(package[field], field)
                result = subprocess.run(
                    ["cargo", "package", "--list", "--offline", "--allow-dirty", "-p", crate],
                    cwd=ROOT, capture_output=True, text=True, check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                files = set(result.stdout.splitlines())
                for directory in ("src", "tests"):
                    expected = {
                        str(path.relative_to(ROOT / crate))
                        for path in (ROOT / crate / directory).rglob("*.rs")
                    }
                    self.assertTrue(expected)
                    self.assertTrue(expected <= files, expected - files)
                self.assertIn("README.md", files)
                self.assertEqual((ROOT / crate / package["readme"]).resolve(), ROOT / "README.md")
                for license_name in ("LICENSE-MIT", "LICENSE-APACHE"):
                    self.assertIn(license_name, files)
                    self.assertEqual((ROOT / crate / license_name).read_bytes(), (ROOT / license_name).read_bytes())
                self.assertFalse(any(path.startswith((".github/", "scripts/", "target/")) for path in files))

    def test_only_the_library_is_published_to_crates_io(self):
        self.assertIs(manifest("beankeeper-cli")["package"]["publish"], False)
        self.assertNotIn("publish", manifest("beankeeper")["package"])
        self.assertNotIn("metadata", manifest("beankeeper-cli")["package"])
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn("python3 -B scripts/publish_crate.py beankeeper\n", workflow)
        self.assertNotIn("publish_crate.py beankeeper-cli", workflow)


if __name__ == "__main__":
    unittest.main()
