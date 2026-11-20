"""Check the source packages and binstall contract without publishing or downloading."""

from pathlib import Path
import re
import subprocess
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[2]


def manifest(crate):
    return tomllib.loads((ROOT / crate / "Cargo.toml").read_text())


class PublishMetadataTests(unittest.TestCase):
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

    def test_binstall_matches_release_assets_for_every_target(self):
        package = manifest("beankeeper-cli")["package"]
        metadata = package["metadata"]["binstall"]
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        targets = re.findall(r"- target: (\S+)", workflow)
        self.assertTrue(targets)
        archive_pattern = re.search(r'ARCHIVE_NAME="([^"]+)"', workflow).group(1)
        tag_pattern = re.search(r'- "(beankeeper-cli-v\*)"', workflow).group(1)
        self.assertEqual(metadata["pkg-fmt"], "tgz")
        for target in targets:
            with self.subTest(target=target):
                values = {
                    "repo": package["repository"], "version": package["version"],
                    "target": target, "bin": "bk", "binary-ext": "",
                }
                def render(template):
                    return re.sub(r"\{\s*([\w-]+)\s*\}", lambda match: values[match.group(1)], template)

                archive = archive_pattern.replace("${VERSION}", values["version"]).replace("${TARGET}", target)
                tag = tag_pattern.replace("*", values["version"])
                self.assertIn('tar czf "${ARCHIVE_NAME}.tar.gz" "${ARCHIVE_NAME}"', workflow)
                self.assertIn('cp "target/${TARGET}/release/${BIN_NAME}" "staging/${ARCHIVE_NAME}/"', workflow)
                self.assertEqual(render(metadata["pkg-url"]), f'{values["repo"]}/releases/download/{tag}/{archive}.tar.gz')
                self.assertEqual(render(metadata["bin-dir"]), f"{archive}/bk")


if __name__ == "__main__":
    unittest.main()
