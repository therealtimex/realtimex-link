#!/usr/bin/env python3
"""Link's GitHub release, from the GitLab pipeline (GITHUB_LINK_TOKEN).

  release.py download VERSION OUT NAME...   the release's (draft) assets, checked against its
                                            SHA256SUMS
  release.py publish VERSION DIR            the signed files in DIR replace or join the release's
                                            assets, SHA256SUMS is rewritten, and the release is
                                            published as Link's latest

A draft's assets are not public: both go through the API with the token.
"""

import hashlib
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request

API = "https://api.github.com"
UPLOADS = "https://uploads.github.com"
REPOSITORY = os.environ.get("GITHUB_REPOSITORY", "therealtimex/realtimex-link")


def request(method, url, body=None, accept="application/vnd.github+json", content_type=None):
    headers = {
        "Authorization": "Bearer " + os.environ["GITHUB_LINK_TOKEN"],
        "Accept": accept,
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "realtimex-link-gitlab-ci",
    }
    if content_type:
        headers["Content-Type"] = content_type
    req = urllib.request.Request(url if url.startswith("http") else API + url, data=body, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=600) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        raise SystemExit(f"{method} {url}: HTTP {error.code} {error.read().decode(errors='replace')[:300]}") from None


def call(method, url, payload=None):
    body = None if payload is None else json.dumps(payload).encode()
    text = request(method, url, body, content_type="application/json" if body else None)
    return json.loads(text) if text else None


def release(version):
    for page in range(1, 4):
        for item in call("GET", f"/repos/{REPOSITORY}/releases?per_page=50&page={page}"):
            if item["tag_name"] == f"v{version}":
                return item
    raise SystemExit(f"{REPOSITORY} has no release v{version}: the build job makes the draft")


def asset_bytes(item, name):
    asset = next((a for a in item["assets"] if a["name"] == name), None)
    if not asset:
        raise SystemExit(f"release {item['tag_name']} has no {name}")
    return request("GET", asset["url"], accept="application/octet-stream")


def parse_sums(text):
    sums = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 2 and len(parts[0]) == 64:
            sums[parts[1].lstrip("*")] = parts[0].lower()
    return sums


def merge_sums(text, files):
    """SHA256SUMS with `files` ({name: sha256}) replacing or joining the listed ones, by name."""
    sums = parse_sums(text)
    sums.update(files)
    return "".join(f"{sha}  {name}\n" for name, sha in sorted(sums.items()))


def notes(version):
    return "\n".join(
        [
            f"RealTimeX Link {version}.",
            "",
            f"- macOS: `RealTimeX-Link-{version}-macos-arm64.dmg` (Apple silicon) or "
            f"`RealTimeX-Link-{version}-macos-x86_64.dmg` (Intel), signed and notarized. Open it and "
            "drag RealTimeX Link to Applications.",
            f"- Windows: `RealTimeX-Link-Setup-{version}.exe`, signed.",
            f"- Linux: `realtimex-link_{version}_amd64.deb` / `realtimex-link_{version}_arm64.deb`, or the `.rpm` files.",
            "",
            "Then sign in to your RealTimeX account in Link's window and turn on Allow remote control. "
            "SHA256SUMS lists every file.",
        ]
    )


def download(version, out, names):
    item = release(version)
    sums = parse_sums(asset_bytes(item, "SHA256SUMS").decode())
    os.makedirs(out, exist_ok=True)
    for name in names:
        data = asset_bytes(item, name)
        actual = hashlib.sha256(data).hexdigest()
        if sums.get(name) != actual:
            raise SystemExit(f"{name}: SHA-256 {actual}, but SHA256SUMS lists {sums.get(name)}")
        with open(os.path.join(out, name), "wb") as file:
            file.write(data)
        print(f"{name}: SHA-256 checked")


def publish(version, folder):
    item = release(version)
    files = sorted(n for n in os.listdir(folder) if os.path.isfile(os.path.join(folder, n)))
    if not files:
        raise SystemExit(f"nothing signed in {folder}")
    signed = {}
    for name in files:
        with open(os.path.join(folder, name), "rb") as file:
            signed[name] = hashlib.sha256(file.read()).hexdigest()
    sums = merge_sums(asset_bytes(item, "SHA256SUMS").decode(), signed)

    def upload(name, data):
        existing = next((a for a in item["assets"] if a["name"] == name), None)
        if existing:
            request("DELETE", f"/repos/{REPOSITORY}/releases/assets/{existing['id']}")
        query = urllib.parse.urlencode({"name": name})
        request(
            "POST",
            f"{UPLOADS}/repos/{REPOSITORY}/releases/{item['id']}/assets?{query}",
            data,
            content_type="application/octet-stream",
        )
        print(f"{'replaced' if existing else 'added'} {name}")

    for name in files:
        with open(os.path.join(folder, name), "rb") as file:
            upload(name, file.read())
    upload("SHA256SUMS", sums.encode())
    done = call(
        "PATCH",
        f"/repos/{REPOSITORY}/releases/{item['id']}",
        {
            "name": f"RealTimeX Link {version}",
            "body": notes(version),
            "draft": False,
            "prerelease": False,
            "make_latest": "true",
        },
    )
    print(f"published {done['html_url']}")
    print(sums, end="")


def main(argv):
    if not os.environ.get("GITHUB_LINK_TOKEN"):
        raise SystemExit("GITHUB_LINK_TOKEN is not set")
    if len(argv) >= 4 and argv[0] == "download":
        download(argv[1].lstrip("v"), argv[2], argv[3:])
    elif len(argv) == 3 and argv[0] == "publish":
        publish(argv[1].lstrip("v"), argv[2])
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])
