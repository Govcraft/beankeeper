"""Dry-run the workflow's signing loop with stubbed cosign, without OIDC or a tag."""

import os
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = (ROOT / ".github/workflows/release.yml").read_text()
RELEASE_JOB = WORKFLOW.split("\n  release:\n", 1)[1].split("\n  aur:\n", 1)[0]
SIGNING_SCRIPT = textwrap.dedent(re.search(
    r"      - name: Sign release artifacts with Sigstore\n"
    r"        working-directory: release\n        run: \|\n((?:          .+\n)+)",
    RELEASE_JOB,
).group(1))


class ReleaseSigningTests(unittest.TestCase):
    def test_signs_each_archive_and_checksum_with_its_own_bundle(self):
        self.run_signing()

    def test_signing_failure_stops_release_step(self):
        self.run_signing(fail=True)

    def run_signing(self, *, fail=False):
        with tempfile.TemporaryDirectory(prefix="bk signing ") as directory:
            root = Path(directory)
            targets = re.findall(r"- target: (\S+)", WORKFLOW)
            self.assertEqual(len(targets), 4)
            artifacts = [f"bk-1.2.3-{target}.tar.gz" for target in targets]
            artifacts.append("checksums-sha256.txt")
            for name in artifacts:
                (root / name).write_text("release fixture\n")
            cosign = root / "cosign"
            cosign.write_text(
                '#!/bin/sh\n[ "$1" = sign-blob ] && [ "$2" = --yes ] && '
                '[ "$3" = --bundle ] && [ "$4" = "$5.sigstore" ] && '
                '[ -f "$5" ] || exit 2\n'
                + ('exit 9\n' if fail else 'printf "bundle\\n" > "$4"\n')
            )
            cosign.chmod(0o755)
            result = subprocess.run(
                ["bash", "-eo", "pipefail", "-c", SIGNING_SCRIPT],
                cwd=root, env={**os.environ, "PATH": f"{root}:{os.environ['PATH']}"},
                capture_output=True, text=True, check=False,
            )
            if fail:
                self.assertEqual(result.returncode, 9, result.stderr)
                self.assertFalse(list(root.glob("*.sigstore")))
            else:
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(
                    {path.name for path in root.glob("*.sigstore")},
                    {f"{name}.sigstore" for name in artifacts},
                )

    def test_release_wires_permissions_attestations_and_bundle_upload(self):
        for permission in ("contents", "id-token", "attestations"):
            self.assertIn(f"      {permission}: write\n", RELEASE_JOB)
        self.assertIn("uses: sigstore/cosign-installer@", RELEASE_JOB)
        self.assertIn("uses: actions/attest-build-provenance@", RELEASE_JOB)
        subjects = re.search(r"          subject-path: \|\n((?:            .+\n)+)", RELEASE_JOB)
        self.assertEqual(textwrap.dedent(subjects.group(1)).splitlines(), [
            "release/*.tar.gz", "release/checksums-sha256.txt",
        ])
        files = re.search(r"          files: \|\n((?:            .+\n)+)", RELEASE_JOB)
        self.assertEqual(textwrap.dedent(files.group(1)).splitlines(), [
            "release/*.tar.gz", "release/checksums-sha256.txt", "release/*.sigstore",
        ])
        self.assertIn("fail_on_unmatched_files: true", RELEASE_JOB)
        ordered_steps = [
            "name: Generate checksums", "name: Install cosign",
            "name: Sign release artifacts with Sigstore",
            "name: Attest release build provenance", "name: Create release",
        ]
        offsets = [RELEASE_JOB.index(step) for step in ordered_steps]
        self.assertEqual(offsets, sorted(offsets))


if __name__ == "__main__":
    unittest.main()
