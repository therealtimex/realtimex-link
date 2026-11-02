"""ci/fetch_agent_desktop.py's checks. Run: python3 -m unittest discover -s ci -p 'test_*.py'"""

import hashlib
import io
import tarfile
import tempfile
import unittest
from pathlib import Path

import fetch_agent_desktop as fetcher


def archive(members):
    """A .tar.gz of (name, bytes or None for a symlink)."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as tar:
        for name, content in members:
            info = tarfile.TarInfo(name)
            if content is None:
                info.type = tarfile.SYMTYPE
                info.linkname = "/etc/passwd"
                tar.addfile(info)
            else:
                info.size = len(content)
                tar.addfile(info, io.BytesIO(content))
    return buffer.getvalue()


def pin_for(data, files):
    return {
        "version": "1.0.2",
        "source": "https://example.invalid/v{version}/{asset}",
        "checksums": "https://example.invalid/v{version}/checksums.txt",
        "targets": {
            "x86_64-unknown-linux-gnu": {
                "asset": "ad.tar.gz",
                "sha256": hashlib.sha256(data).hexdigest(),
                "files": files,
            }
        },
    }


class Fetch(unittest.TestCase):
    def test_the_repository_pin_is_well_formed(self):
        pin = fetcher.load_pin()
        self.assertEqual(pin["version"], "1.0.2")
        for target in (
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
        ):
            self.assertIn(target, pin["targets"])
            self.assertIn("v1.0.2", fetcher.asset_url(pin, target))

    def test_only_the_pinned_files_come_out(self):
        data = archive([("agent-desktop", b"bin"), ("README.md", b"x"), ("./helper", b"h")])
        pin = pin_for(data, ["agent-desktop", "helper"])
        with tempfile.TemporaryDirectory() as out:
            names = fetcher.fetch("x86_64-unknown-linux-gnu", out, pin=pin, get=lambda url: data)
            self.assertEqual(names, ["agent-desktop", "helper"])
            self.assertEqual((Path(out) / "agent-desktop").read_bytes(), b"bin")
            self.assertFalse((Path(out) / "README.md").exists())
            self.assertEqual((Path(out) / "agent-desktop.version").read_text().strip(), "1.0.2")

    def test_a_wrong_hash_fails_and_writes_nothing(self):
        data = archive([("agent-desktop", b"bin")])
        pin = pin_for(data, ["agent-desktop"])
        tampered = archive([("agent-desktop", b"evil")])
        with tempfile.TemporaryDirectory() as out:
            with self.assertRaises(fetcher.PinError):
                fetcher.fetch("x86_64-unknown-linux-gnu", out, pin=pin, get=lambda url: tampered)
            self.assertEqual(list(Path(out).iterdir()), [])

    def test_links_and_missing_files_are_refused(self):
        data = archive([("agent-desktop", None)])
        with tempfile.TemporaryDirectory() as out:
            with self.assertRaises(fetcher.PinError):
                fetcher.extract(data, ["agent-desktop"], out)
            with self.assertRaises(fetcher.PinError):
                fetcher.extract(archive([("other", b"x")]), ["agent-desktop"], out)

    def test_a_cached_asset_is_used_only_with_the_pinned_hash(self):
        data = archive([("agent-desktop", b"bin")])
        pin = pin_for(data, ["agent-desktop"])
        with tempfile.TemporaryDirectory() as cache, tempfile.TemporaryDirectory() as out:
            calls = []

            def get(url):
                calls.append(url)
                return data

            fetcher.fetch("x86_64-unknown-linux-gnu", out, cache=cache, pin=pin, get=get)
            fetcher.fetch("x86_64-unknown-linux-gnu", out, cache=cache, pin=pin, get=get)
            self.assertEqual(len(calls), 1)
            (Path(cache) / "ad.tar.gz").write_bytes(b"corrupt")
            fetcher.fetch("x86_64-unknown-linux-gnu", out, cache=cache, pin=pin, get=get)
            self.assertEqual(len(calls), 2)

    def test_upstream_checksums_are_compared_when_bumping(self):
        data = archive([("agent-desktop", b"bin")])
        pin = pin_for(data, ["agent-desktop"])
        good = f"{hashlib.sha256(data).hexdigest()}  ad.tar.gz\n".encode()
        self.assertEqual(fetcher.check_upstream(pin, get=lambda url: good), [])
        bad = ("0" * 64 + "  ad.tar.gz\n").encode()
        self.assertEqual(len(fetcher.check_upstream(pin, get=lambda url: bad)), 1)

    def test_file_names_must_be_plain(self):
        data = archive([("agent-desktop", b"bin")])
        pin = pin_for(data, ["../agent-desktop"])
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as file:
            import json

            json.dump(pin, file)
        with self.assertRaises(fetcher.PinError):
            fetcher.load_pin(file.name)


if __name__ == "__main__":
    unittest.main()
