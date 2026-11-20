"""Publish a missing crate version using the release job's OIDC credential."""

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
AUTH_REJECTION = re.compile(
    r"\bstatus(?: code)?[ :=]+(?:401|403)\b|unauthorized|forbidden|"
    r"not authorized|not allowed to publish|"
    r"token does not have|required (?:scope|permission)|trusted publish",
    re.IGNORECASE,
)


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
    if result.returncode and crate == "beankeeper-cli" and AUTH_REJECTION.search(result.stdout):
        print(
            "::warning::Skipping beankeeper-cli after a crates.io authorization rejection. "
            "If it has not been bootstrapped, the owner must publish it once manually with "
            "a token after the library is live, then configure the trusted publisher "
            "Govcraft/beankeeper, release.yml. See docs/RELEASE.md.",
            flush=True,
        )
        return 0
    return result.returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("crate", choices=("beankeeper", "beankeeper-cli"))
    args = parser.parse_args()
    try:
        return publish(args.crate)
    except (OSError, URLError, ValueError, KeyError) as error:
        print(f"::error::Could not publish {args.crate}: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
