"""Fetch the pinned agent-desktop CLI for one target (realtimex-ai-app#3055 §2.1).

  python3 ci/fetch_agent_desktop.py --target x86_64-unknown-linux-gnu --out DIR [--cache DIR]

The pin (third_party/agent-desktop.json) names the release asset and its SHA-256 for each
target; the hash in this repository is the authority. The asset is downloaded (or taken from
--cache when a file there has that hash), checked, and only the pinned files are taken out of it,
by name, into DIR: nothing else in the archive is written, and no path in it is followed. Any
mismatch fails the build.

`--check-upstream` also compares every pinned hash with the release's checksums.txt: for when the
pin is bumped (the build never trusts checksums.txt).
"""

import argparse
import hashlib
import io
import json
import os
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PIN = ROOT / "third_party" / "agent-desktop.json"
TIMEOUT = 120


class PinError(Exception):
    pass


def load_pin(path=PIN):
    with open(path, encoding="utf-8") as file:
        pin = json.load(file)
    for name in ("version", "source", "targets"):
        if name not in pin:
            raise PinError(f"{path}: no {name}")
    for target, entry in pin["targets"].items():
        sha = entry.get("sha256", "")
        if len(sha) != 64 or any(c not in "0123456789abcdef" for c in sha):
            raise PinError(f"{target}: sha256 must be 64 lowercase hex digits")
        if not entry.get("asset") or not entry.get("files"):
            raise PinError(f"{target}: asset and files are required")
        for name in entry["files"]:
            if "/" in name or "\\" in name or name in ("", ".", ".."):
                raise PinError(f"{target}: {name!r} must be a plain file name")
    return pin


def asset_url(pin, target):
    entry = pin["targets"].get(target)
    if entry is None:
        raise PinError(f"no agent-desktop pinned for {target}")
    return pin["source"].format(version=pin["version"], asset=entry["asset"])


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def verify(data, expected, what):
    actual = sha256(data)
    if actual != expected:
        raise PinError(f"{what}: SHA-256 {actual}, pinned {expected}")


def extract(data, files, out):
    """The pinned files only, by name, as regular files: links, devices and every other member
    are refused, and nothing is written outside `out`."""
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    wanted = set(files)
    found = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:*") as archive:
        for member in archive.getmembers():
            name = member.name
            while name.startswith("./"):
                name = name[2:]
            if name not in wanted:
                continue
            if not member.isreg():
                raise PinError(f"{name} in the archive is not a regular file")
            if name in found:
                raise PinError(f"{name} is in the archive twice")
            reader = archive.extractfile(member)
            if reader is None:
                raise PinError(f"{name} can't be read from the archive")
            found[name] = reader.read()
    missing = wanted - set(found)
    if missing:
        raise PinError(f"not in the archive: {', '.join(sorted(missing))}")
    for name, content in found.items():
        target = out / name
        tmp = out / f".{name}.tmp"
        tmp.write_bytes(content)
        os.chmod(tmp, 0o755)
        os.replace(tmp, target)
    return sorted(found)


def download(url):
    request = urllib.request.Request(url, headers={"User-Agent": "realtimex-link-ci"})
    with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
        return response.read()


def fetch(target, out, cache=None, pin=None, get=download):
    pin = pin or load_pin()
    entry = pin["targets"].get(target)
    if entry is None:
        raise PinError(f"no agent-desktop pinned for {target}")
    data = None
    cached = Path(cache) / entry["asset"] if cache else None
    if cached is not None and cached.is_file():
        candidate = cached.read_bytes()
        if sha256(candidate) == entry["sha256"]:
            data = candidate
    if data is None:
        data = get(asset_url(pin, target))
        verify(data, entry["sha256"], entry["asset"])
        if cached is not None:
            cached.parent.mkdir(parents=True, exist_ok=True)
            with tempfile.NamedTemporaryFile(dir=cached.parent, delete=False) as tmp:
                tmp.write(data)
            os.replace(tmp.name, cached)
    names = extract(data, entry["files"], out)
    (Path(out) / "agent-desktop.version").write_text(pin["version"] + "\n", encoding="utf-8")
    return names


def check_upstream(pin=None, get=download):
    """Every pinned hash against the release's checksums.txt (when bumping the pin)."""
    pin = pin or load_pin()
    sums = {}
    text = get(pin["checksums"].format(version=pin["version"])).decode("utf-8")
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 2:
            sums[parts[1].lstrip("*")] = parts[0].lower()
    problems = []
    for target, entry in pin["targets"].items():
        upstream = sums.get(entry["asset"])
        if upstream != entry["sha256"]:
            problems.append(f"{target}: pinned {entry['sha256']}, checksums.txt {upstream}")
    return problems


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--target")
    parser.add_argument("--out")
    parser.add_argument("--cache")
    parser.add_argument("--check-upstream", action="store_true")
    args = parser.parse_args(argv)
    try:
        if args.check_upstream:
            problems = check_upstream()
            for problem in problems:
                print(f"ERROR: {problem}", file=sys.stderr)
            if problems:
                return 1
            print("every pinned hash matches checksums.txt")
            return 0
        if not args.target or not args.out:
            parser.error("--target and --out are required")
        names = fetch(args.target, args.out, args.cache)
        print(f"agent-desktop {load_pin()['version']} for {args.target}: {', '.join(names)} in {args.out}")
        return 0
    except (PinError, OSError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

