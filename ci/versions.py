"""Version and checksum helpers the release scripts share (pure: no network, no environment).

ci/test_versions.py tests them: python3 -m unittest discover -s ci
"""

import re

VERSION = re.compile(r"^v?(\d+)\.(\d+)\.(\d+)$")


def parse_version(text):
    """(major, minor, patch) for "X.Y.Z" or "vX.Y.Z"; None for anything else (pre-releases too)."""
    match = VERSION.match(text.strip())
    return tuple(int(part) for part in match.groups()) if match else None


def cargo_version(text):
    """The workspace version in Cargo.toml's [workspace.package], which every crate (and so
    CARGO_PKG_VERSION, the version Link reports and compares when it updates itself) inherits."""
    section = None
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            section = stripped
            continue
        if section == "[workspace.package]":
            match = re.match(r'^version\s*=\s*"([^"]+)"', stripped)
            if match:
                return match.group(1)
    raise ValueError("Cargo.toml has no version in [workspace.package]")


def tag_matches_cargo(tag, cargo_toml):
    """None when tag vX.Y.Z is Cargo.toml's version, else why not. A tag that differs from
    CARGO_PKG_VERSION makes a release Link offers itself as an update forever (it compares the
    release's tag with its own version)."""
    version = cargo_version(cargo_toml)
    if tag != f"v{version}":
        return f"tag {tag} but Cargo.toml's [workspace.package] version is {version}: bump Cargo.toml and tag again"
    return None


def should_be_latest(version, published_tags):
    """Whether publishing `version` should mark it as the repository's latest release: only when
    no published (non-draft, non-prerelease) release is newer. Publishing an older version again
    (a signing fix for 0.1.9 after 0.1.10 is out) must not move "latest" back, because Link and
    RealTimeX update from releases/latest."""
    mine = parse_version(version)
    if mine is None:
        raise ValueError(f"not a release version: {version}")
    others = [v for v in (parse_version(tag) for tag in published_tags) if v is not None]
    return all(mine >= other for other in others)


def parse_sums(text):
    """{name: sha256} from a sha256sum listing."""
    sums = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 2 and len(parts[0]) == 64:
            sums[parts[1].lstrip("*")] = parts[0].lower()
    return sums


def format_sums(sums):
    return "".join(f"{sha}  {name}\n" for name, sha in sorted(sums.items()))
