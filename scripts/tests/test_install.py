"""Exercise the real installer offline, without touching an installed bk or Sigstore."""

import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
VERSION = "1.2.3"
ARCHIVE = f"bk-{VERSION}-x86_64-unknown-linux-gnu.tar.gz"
IDENTITY = (
    "https://github.com/Govcraft/beankeeper/.github/workflows/"
    f"release.yml@refs/tags/beankeeper-cli-v{VERSION}"
)
ISSUER = "https://token.actions.githubusercontent.com"


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="bk installer ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.assets = self.root / "assets"
        self.assets.mkdir()
        self.install = self.root / "installed"
        self.install.mkdir()
        self.installed = self.install / "bk"
        self.installed.write_text("existing installation\n")
        self.log = self.root / "calls.jsonl"
        # Deliberately exclude the host's cosign and curl, including on CI runners.
        for command in ("sh", "mktemp", "rm", "awk", "grep", "sha256sum", "gzip"):
            executable = shutil.which(command)
            self.assertIsNotNone(executable, command)
            (self.bin / command).symlink_to(executable)
        self.stub("uname", 'print("Linux" if sys.argv[1] == "-s" else "x86_64")')
        self.stub("curl", '''
args = sys.argv[1:]
if "%{redirect_url}" in args:
    print("https://github.com/Govcraft/beankeeper/releases/tag/beankeeper-cli-v1.2.3")
    sys.exit(0)
url = args[1]
assert url.startswith("https://github.com/Govcraft/beankeeper/releases/download/beankeeper-cli-v1.2.3/"), url
name = url.rsplit("/", 1)[1]
source = Path(os.environ["TEST_ASSETS"]) / name
if not source.is_file():
    sys.exit(22)
shutil.copyfile(source, args[args.index("-o") + 1])
''')
        for command in ("tar", "install"):
            self.stub(command, f"os.execv({shutil.which(command)!r}, [{command!r}, *sys.argv[1:]])")
        payload = f"#!/bin/sh\nprintf 'bk {VERSION}\\n'\n".encode()
        with tarfile.open(self.assets / ARCHIVE, "w:gz") as archive:
            member = tarfile.TarInfo(f"{ARCHIVE.removesuffix('.tar.gz')}/bk")
            member.size = len(payload)
            member.mode = 0o755
            archive.addfile(member, io.BytesIO(payload))
        digest = hashlib.sha256((self.assets / ARCHIVE).read_bytes()).hexdigest()
        (self.assets / "checksums-sha256.txt").write_text(f"{digest}  {ARCHIVE}\n")
        self.write_bundles()
        self.env = {
            **os.environ,
            "PATH": str(self.bin),
            "BK_VERSION": VERSION,
            "BK_INSTALL": str(self.install),
            "TEST_ASSETS": str(self.assets),
            "TEST_LOG": str(self.log),
        }

    def stub(self, name, code):
        import sys

        script = self.bin / name
        script.write_text(
            f"#!{sys.executable}\n"
            "import hashlib, json, os, shutil, sys\nfrom pathlib import Path\n"
            "with open(os.environ['TEST_LOG'], 'a') as log:\n"
            f"    log.write(json.dumps([{name!r}, *sys.argv[1:]]) + '\\n')\n"
            + code + "\n"
        )
        script.chmod(0o755)

    def write_bundles(self, *, identity=IDENTITY, issuer=ISSUER):
        for name in (ARCHIVE, "checksums-sha256.txt"):
            (self.assets / f"{name}.sigstore").write_text(json.dumps({
                "digest": hashlib.sha256((self.assets / name).read_bytes()).hexdigest(),
                "identity": identity,
                "issuer": issuer,
            }))

    def with_cosign(self):
        # A verifier stub checks the bytes and trust policy passed by install.sh.
        self.stub("cosign", '''
args = sys.argv[1:]
assert args[0] == "verify-blob", args
assert len(args) == 8, args
bundle = json.loads(Path(args[args.index("--bundle") + 1]).read_text())
assert bundle["identity"] == args[args.index("--certificate-identity") + 1]
assert bundle["issuer"] == args[args.index("--certificate-oidc-issuer") + 1]
assert bundle["digest"] == hashlib.sha256(Path(args[-1]).read_bytes()).hexdigest()
''')

    def calls(self, command):
        return [args for line in self.log.read_text().splitlines()
                if (args := json.loads(line))[0] == command]

    def run_install(self, *, error=None):
        result = subprocess.run(
            [str(self.bin / "sh"), str(ROOT / "install.sh")],
            env=self.env, capture_output=True, text=True, check=False,
        )
        if error is None:
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(f"installed bk v{VERSION}", result.stdout)
            self.assertNotEqual(self.installed.read_text(), "existing installation\n")
            self.assertEqual(len(self.calls("install")), 1)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn(error, result.stderr)
            self.assertEqual(self.installed.read_text(), "existing installation\n")
            self.assertFalse(self.calls("tar"))
            self.assertFalse(self.calls("install"))
        return result

    def test_cosign_verifies_archive_and_manifest_before_extraction(self):
        self.with_cosign()
        result = self.run_install()
        self.assertNotIn("only the SHA-256", result.stderr)
        calls = self.calls("cosign")
        self.assertEqual([Path(call[-1]).name for call in calls], [ARCHIVE, "checksums-sha256.txt"])
        all_calls = [json.loads(line)[0] for line in self.log.read_text().splitlines()]
        self.assertLess(max(i for i, name in enumerate(all_calls) if name == "cosign"), all_calls.index("tar"))

    def test_without_cosign_checks_checksum_and_warns(self):
        result = self.run_install()
        self.assertIn("only the SHA-256 checksum will be checked", result.stderr)
        self.assertIn("release authenticity is not verified", result.stderr)
        self.assertFalse(self.calls("cosign"))
        self.assertFalse(any(".sigstore" in " ".join(call) for call in self.calls("curl")))

    def test_latest_release_uses_resolved_tag_identity(self):
        self.with_cosign()
        self.env.pop("BK_VERSION")
        self.run_install()
        self.assertEqual(len(self.calls("cosign")), 2)

    def test_missing_bundle_stops_installation(self):
        self.with_cosign()
        for name in (ARCHIVE, "checksums-sha256.txt"):
            with self.subTest(artifact=name):
                self.write_bundles()
                (self.assets / f"{name}.sigstore").unlink()
                self.run_install(error="could not download Sigstore bundle")

    def test_rejects_wrong_identity_tag_repository_workflow_or_issuer(self):
        self.with_cosign()
        for identity, issuer in (
            (IDENTITY.replace("v1.2.3", "v1.2.4"), ISSUER),
            (IDENTITY.replace("Govcraft", "another-owner"), ISSUER),
            (IDENTITY.replace("release.yml", "ci.yml"), ISSUER),
            (IDENTITY.replace("refs/tags/beankeeper-cli-v1.2.3", "refs/heads/main"), ISSUER),
            (IDENTITY, "https://accounts.google.com"),
        ):
            with self.subTest(identity=identity, issuer=issuer):
                self.write_bundles(identity=identity, issuer=issuer)
                self.run_install(error="Sigstore verification failed")

    def test_tampered_archive_or_manifest_stops_installation(self):
        self.with_cosign()
        for name in (ARCHIVE, "checksums-sha256.txt"):
            with self.subTest(artifact=name):
                path = self.assets / name
                original = path.read_bytes()
                path.write_bytes(original + b"tampered")
                self.run_install(error=f"Sigstore verification failed for {name}")
                path.write_bytes(original)

    def test_invalid_bundle_stops_installation(self):
        self.with_cosign()
        (self.assets / f"{ARCHIVE}.sigstore").write_text("invalid bundle")
        self.run_install(error="Sigstore verification failed")

    def test_checksum_mismatch_stops_installation_with_or_without_cosign(self):
        (self.assets / "checksums-sha256.txt").write_text(f"{'0' * 64}  {ARCHIVE}\n")
        self.run_install(error="checksum mismatch")
        self.write_bundles()
        self.with_cosign()
        self.run_install(error="checksum mismatch")

    def test_checksum_requires_exact_archive_name(self):
        path = self.assets / "checksums-sha256.txt"
        path.write_text(path.read_text().replace(ARCHIVE, f"prefix-{ARCHIVE}"))
        self.run_install(error="no checksum found")


if __name__ == "__main__":
    unittest.main()
