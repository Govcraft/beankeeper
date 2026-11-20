"""Publish a missing library version using the release job's OIDC credential.

Only the `beankeeper` library is published to crates.io. The CLI
(`beankeeper-cli`, `publish = false`) ships as GitHub Release binaries.
"""

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


ROOT = Path(__file__).resolve().parents[1]
PUBLISHABLE = ("beankeeper",)
# crates.io rejects re-uploads of an existing version; another run may have won a race.
ALREADY_UPLOADED = re.compile(r"\balready (?:exists|uploaded)\b", re.IGNORECASE)


def is_published(crate, version):
    request = Request(
        f"https://crates.io/api/v1/crates/{crate}/{version}",
        headers={"User-Agent": "Govcraft/beankeeper release.yml"},
    )
    try:
        with urlopen(request, timeout=30) as response:
            metadata = json.load(response)
    except HTTPError as error:
        with error:
            if error.code == 404:
                return False
            raise
    if metadata["version"]["num"] != version:
        raise ValueError(f"Unexpected crates.io version for {crate}@{version}")
    return True


def publish(crate):
    if crate not in PUBLISHABLE:
        raise ValueError(f"{crate} is not published to crates.io")
    package = tomllib.loads((ROOT / crate / "Cargo.toml").read_text())["package"]
    version = package["version"]
    if is_published(crate, version):
        print(f"Skipping {crate}@{version}: already published on crates.io.", flush=True)
        return 0

    result = subprocess.run(
        ["cargo", "publish", "-p", crate, "--locked"],
        cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, check=False,
    )
    print(result.stdout, end="", flush=True)
    if result.returncode and ALREADY_UPLOADED.search(result.stdout) and is_published(crate, version):
        print(f"Skipping {crate}@{version}: already published on crates.io.", flush=True)
        return 0
    return result.returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("crate", choices=PUBLISHABLE)
    args = parser.parse_args()
    try:
        return publish(args.crate)
    except (OSError, URLError, ValueError, KeyError) as error:
        print(f"::error::Could not publish {args.crate}: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
