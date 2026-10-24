#!/usr/bin/env python3
"""Link's GitHub release, from the GitLab pipeline (GITHUB_LINK_TOKEN: Contents and Actions read;
publish also needs Contents write).

  release.py fetch VERSION OUT     the files to sign, from the build run of this release's commit
                                   (its Actions artifact `release-unsigned`, checked against its
                                   SHA256SUMS), and the release commit's packaging/ in OUT/tree
  release.py publish VERSION DIR   the signed files in DIR replace or join the draft's assets,
                                   SHA256SUMS is rewritten, and the draft is published (as Link's
                                   latest only when no newer release is out)

Where the files come from (provenance). A draft release's assets can be replaced by anyone holding
a Contents-write token, and its SHA256SUMS sits in the same draft, so the draft alone proves
nothing. What is checked instead, before anything is signed or published:

- The release's commit is GitLab's (this repository is the source of truth): CI_COMMIT_SHA on a
  vX.Y.Z tag pipeline; when signing again (LINK_SIGN_VERSION, a pipeline on main) the commit
  GitLab's own vX.Y.Z tag points at, fetched with the job's token.
- That commit's Cargo.toml says VERSION.
- The mirror's vX.Y.Z tag resolves to that same commit (the release is attached to it).
- The build run is the Build workflow (.github/workflows/build.yml), dispatched on the tag, on that
  commit, in the mirror, and it succeeded. On a tag pipeline it is the very run the build job
  waited for (LINK_BUILD_RUN_ID and LINK_BUILD_SHA, the build job's dotenv report). When signing
  again, no build job ran: it is the newest successful such run, which is the one whose draft is
  current (a rebuild replaces the draft).
- The files to sign come from that run's Actions artifacts, which only the run itself could have
  uploaded, never from the draft. Before publishing, every unsigned file left on the draft (the
  Linux packages and archives) must be one github-actions[bot] uploaded with the SHA-256 that run
  recorded (its `release-sums` artifact); SHA256SUMS is rewritten from that record, not from the
  draft's.

The release must still be a draft: publishing again never touches a published release.
"""

import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import zipfile

from versions import format_sums, parse_sums, should_be_latest, tag_matches_cargo

API = "https://api.github.com"
UPLOADS = "https://uploads.github.com"
REPOSITORY = os.environ.get("GITHUB_REPOSITORY", "therealtimex/realtimex-link")
WORKFLOW = ".github/workflows/build.yml"
BOT = "github-actions[bot]"
# What the build run keeps for the signing pipeline (build.yml, draft-release).
UNSIGNED_ARTIFACT = "release-unsigned"
SUMS_ARTIFACT = "release-sums"
# The archives the sign jobs sign; the other release files (Linux) are published as built.
TO_SIGN = [
    "realtimex-link-macos-arm64.tar.gz",
    "realtimex-link-macos-x86_64.tar.gz",
    "realtimex-link-windows-x86_64.tar.gz",
]
# From the release commit's tree: the .dmg and installer scripts and what they include.
TREE_PATHS = ["packaging", "assets/icon.ico"]


def headers(accept="application/vnd.github+json", content_type=None):
    result = {
        "Authorization": "Bearer " + os.environ["GITHUB_LINK_TOKEN"],
        "Accept": accept,
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "realtimex-link-gitlab-ci",
    }
    if content_type:
        result["Content-Type"] = content_type
    return result


def request(method, url, body=None, accept="application/vnd.github+json", content_type=None):
    req = urllib.request.Request(
        url if url.startswith("http") else API + url, data=body, method=method, headers=headers(accept, content_type)
    )
    try:
        with urllib.request.urlopen(req, timeout=600) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        raise SystemExit(f"{method} {url}: HTTP {error.code} {error.read().decode(errors='replace')[:300]}") from None


def call(method, url, payload=None):
    body = None if payload is None else json.dumps(payload).encode()
    text = request(method, url, body, content_type="application/json" if body else None)
    return json.loads(text) if text else None


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def download_redirected(url, path):
    """An Actions artifact: the API answers with a redirect to short-lived storage, which is
    fetched without the token (storage that isn't GitHub's must never see it)."""
    opener = urllib.request.build_opener(_NoRedirect)
    try:
        opener.open(urllib.request.Request(url, headers=headers()), timeout=60)
        raise SystemExit(f"GET {url}: no redirect to the file")
    except urllib.error.HTTPError as error:
        if error.code not in (301, 302, 303, 307, 308) or not error.headers.get("Location"):
            raise SystemExit(f"GET {url}: HTTP {error.code} {error.read().decode(errors='replace')[:300]}") from None
        location = error.headers["Location"]
    req = urllib.request.Request(location, headers={"User-Agent": "realtimex-link-gitlab-ci"})
    with urllib.request.urlopen(req, timeout=600) as response, open(path, "wb") as file:
        shutil.copyfileobj(response, file)


def git(*args):
    try:
        return subprocess.run(["git", *args], check=True, capture_output=True).stdout
    except subprocess.CalledProcessError as error:
        raise SystemExit(f"git {' '.join(args)}: {error.stderr.decode(errors='replace').strip()}") from None


# Pure checks (ci/test_release.py).


def run_problems(run, repository, commit, tag):
    """Why `run` (a workflow run object) is not the successful Build run of `commit` on `tag` in
    `repository`; empty when it is."""
    problems = []
    expected = {
        "path": WORKFLOW,
        "event": "workflow_dispatch",
        "head_branch": tag,
        "head_sha": commit,
        "status": "completed",
        "conclusion": "success",
    }
    for key, value in expected.items():
        if run.get(key) != value:
            problems.append(f"{key} is {run.get(key)!r}, not {value!r}")
    for key in ("repository", "head_repository"):
        name = (run.get(key) or {}).get("full_name")
        if name != repository:
            problems.append(f"{key} is {name!r}, not {repository!r}")
    return problems


def the_release(releases, tag):
    """The one release for `tag`, which must still be a draft."""
    matches = [item for item in releases if item.get("tag_name") == tag]
    if not matches:
        raise SystemExit(f"{REPOSITORY} has no release {tag}: the tag's build makes the draft")
    if len(matches) > 1:
        raise SystemExit(f"{REPOSITORY} has {len(matches)} releases for {tag}: delete the extra drafts")
    item = matches[0]
    if not item.get("draft"):
        raise SystemExit(f"release {tag} is already published ({item.get('html_url')}): a published release is never changed")
    return item


def asset_problems(assets, built, signed):
    """Check the draft's assets against what the build run recorded (`built`, {name: sha256}) and
    the files about to be uploaded (`signed`, same). Returns (problems, unchecked): `unchecked` are
    the assets GitHub gave no digest for, to download and hash."""
    problems = []
    unchecked = []
    present = set()
    for asset in assets:
        name = asset["name"]
        present.add(name)
        if name == "SHA256SUMS" or name in signed:
            continue  # rewritten / replaced
        if name not in built:
            problems.append(f"{name} is on the draft but the build run didn't make it")
            continue
        uploader = (asset.get("uploader") or {}).get("login")
        if uploader != BOT:
            problems.append(f"{name} was uploaded by {uploader!r}, not {BOT}")
        digest = asset.get("digest") or ""
        if digest.startswith("sha256:"):
            if digest[len("sha256:"):].lower() != built[name]:
                problems.append(f"{name}: SHA-256 {digest[7:]}, but the build run recorded {built[name]}")
        else:
            unchecked.append(asset)
    for name in sorted(set(built) - present - set(signed)):
        problems.append(f"{name} is missing from the draft")
    return problems, unchecked


# GitHub and git.


def all_releases():
    releases = []
    for page in range(1, 21):
        items = call("GET", f"/repos/{REPOSITORY}/releases?per_page=100&page={page}")
        releases.extend(items)
        if len(items) < 100:
            break
    return releases


def mirror_commit(tag):
    target = call("GET", f"/repos/{REPOSITORY}/git/ref/tags/{tag}")["object"]
    # An annotated tag points at a tag object; follow it to the commit.
    while target["type"] == "tag":
        target = call("GET", target["url"])["object"]
    return target["sha"]


def release_commit(tag):
    """The commit GitLab says the release is."""
    pipeline_tag = os.environ.get("CI_COMMIT_TAG")
    if pipeline_tag:
        if pipeline_tag != tag:
            raise SystemExit(f"this is the {pipeline_tag} pipeline, not {tag}'s")
        return os.environ["CI_COMMIT_SHA"]
    # Signing again, from main: GitLab's own tag, fetched with the job's token.
    git("fetch", "--no-tags", "--depth=1", "origin", f"+refs/tags/{tag}:refs/tags/{tag}")
    return git("rev-parse", "--verify", f"{tag}^{{commit}}").decode().strip()


def build_run(tag, commit):
    run_id = os.environ.get("LINK_BUILD_RUN_ID")
    if os.environ.get("CI_COMMIT_TAG"):
        # The tag pipeline: the run the build job waited for, never a search.
        if not run_id:
            raise SystemExit("LINK_BUILD_RUN_ID is not set: the build job's dotenv report (build.env) didn't arrive")
        if os.environ.get("LINK_BUILD_SHA") != commit:
            raise SystemExit(f"the build job built {os.environ.get('LINK_BUILD_SHA')}, not {commit}")
        return call("GET", f"/repos/{REPOSITORY}/actions/runs/{int(run_id)}")
    query = urllib.parse.urlencode(
        {"event": "workflow_dispatch", "head_sha": commit, "status": "success", "per_page": 100}
    )
    runs = call("GET", f"/repos/{REPOSITORY}/actions/workflows/build.yml/runs?{query}")["workflow_runs"]
    runs = [run for run in runs if run.get("head_branch") == tag]
    if not runs:
        raise SystemExit(f"no successful Build run of {tag} at {commit[:8]} in {REPOSITORY}: run the tag's pipeline")
    return max(runs, key=lambda run: run["created_at"])


def provenance(version):
    """(tag, commit, run) once every check in this file's docstring has passed."""
    tag = f"v{version}"
    commit = release_commit(tag)
    problem = tag_matches_cargo(tag, git("show", f"{commit}:Cargo.toml").decode())
    if problem:
        raise SystemExit(problem)
    mirrored = mirror_commit(tag)
    if mirrored != commit:
        raise SystemExit(f"{REPOSITORY}'s {tag} is {mirrored}, but GitLab's is {commit}: the mirror's tag was moved")
    run = build_run(tag, commit)
    problems = run_problems(run, REPOSITORY, commit, tag)
    if problems:
        raise SystemExit(f"Build run {run.get('html_url')} is not {tag}'s build: " + "; ".join(problems))
    print(f"{tag}: commit {commit}, built by {run['html_url']}")
    return tag, commit, run


def run_artifact(run, name, folder):
    """Unzip the run's Actions artifact `name` into `folder`; {file name: path}."""
    query = urllib.parse.urlencode({"name": name})
    artifacts = call("GET", f"/repos/{REPOSITORY}/actions/runs/{run['id']}/artifacts?{query}")["artifacts"]
    artifacts = [a for a in artifacts if a["name"] == name and (a.get("workflow_run") or {}).get("id") == run["id"]]
    if not artifacts:
        raise SystemExit(f"Build run {run['html_url']} has no artifact {name} (a build from before this pipeline change?)")
    artifact = max(artifacts, key=lambda a: a["created_at"])
    if artifact.get("expired"):
        raise SystemExit(f"artifact {name} of {run['html_url']} has expired: rebuild the tag")
    with tempfile.TemporaryDirectory() as work:
        archive = os.path.join(work, "artifact.zip")
        download_redirected(artifact["archive_download_url"], archive)
        files = {}
        with zipfile.ZipFile(archive) as zipped:
            for member in zipped.infolist():
                base = os.path.basename(member.filename)
                if member.is_dir() or not base or base != member.filename:
                    raise SystemExit(f"artifact {name}: unexpected entry {member.filename!r}")
                path = os.path.join(folder, base)
                with zipped.open(member) as source, open(path, "wb") as target:
                    shutil.copyfileobj(source, target)
                files[base] = path
    return files


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        for chunk in iter(lambda: file.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


# Commands.


def fetch(version, out):
    tag, commit, run = provenance(version)
    the_release(all_releases(), tag)  # still a draft: fail now, not after signing
    os.makedirs(out, exist_ok=True)
    files = run_artifact(run, UNSIGNED_ARTIFACT, out)
    if "SHA256SUMS" not in files:
        raise SystemExit(f"artifact {UNSIGNED_ARTIFACT} has no SHA256SUMS")
    with open(files.pop("SHA256SUMS"), encoding="utf-8") as file:
        sums = parse_sums(file.read())
    os.remove(os.path.join(out, "SHA256SUMS"))
    for name in TO_SIGN:
        if name not in files:
            raise SystemExit(f"artifact {UNSIGNED_ARTIFACT} has no {name}")
        actual = sha256_file(files[name])
        if sums.get(name) != actual:
            raise SystemExit(f"{name}: SHA-256 {actual}, but the build run recorded {sums.get(name)}")
        print(f"{name}: from {run['html_url']}, SHA-256 checked")
    for extra in set(files) - set(TO_SIGN):
        os.remove(files[extra])

    # The .dmg and installer scripts as the release's commit has them (when signing again, this
    # pipeline's own checkout is main's, and only ci/ is meant to come from there).
    tree = os.path.join(out, "tree")
    shutil.rmtree(tree, ignore_errors=True)
    os.makedirs(tree)
    with tarfile.open(fileobj=io.BytesIO(git("archive", "--format=tar", commit, *TREE_PATHS))) as archive:
        if hasattr(tarfile, "data_filter"):
            archive.extractall(tree, filter="data")
        else:
            archive.extractall(tree)
    print(f"{', '.join(TREE_PATHS)}: from {commit[:8]}")


def publish(version, folder):
    tag, commit, run = provenance(version)
    files = sorted(n for n in os.listdir(folder) if os.path.isfile(os.path.join(folder, n)))
    if not files:
        raise SystemExit(f"nothing signed in {folder}")
    signed = {name: sha256_file(os.path.join(folder, name)) for name in files}

    with tempfile.TemporaryDirectory() as work:
        recorded = run_artifact(run, SUMS_ARTIFACT, work)
        if "SHA256SUMS" not in recorded:
            raise SystemExit(f"artifact {SUMS_ARTIFACT} has no SHA256SUMS")
        with open(recorded["SHA256SUMS"], encoding="utf-8") as file:
            built = parse_sums(file.read())

    releases = all_releases()
    item = the_release(releases, tag)
    problems, unchecked = asset_problems(item["assets"], built, signed)
    for asset in unchecked:
        actual = hashlib.sha256(request("GET", asset["url"], accept="application/octet-stream")).hexdigest()
        if actual != built[asset["name"]]:
            problems.append(f"{asset['name']}: SHA-256 {actual}, but the build run recorded {built[asset['name']]}")
    if problems:
        raise SystemExit(f"draft {tag} isn't what {run['html_url']} built:\n  " + "\n  ".join(problems))

    published = [r["tag_name"] for r in releases if not r.get("draft") and not r.get("prerelease")]
    latest = should_be_latest(version, published)
    sums = format_sums({**built, **signed})

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
            "make_latest": "true" if latest else "false",
        },
    )
    print(f"published {done['html_url']}" + ("" if latest else " (not as latest: a newer release is out)"))
    print(sums, end="")


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


def main(argv):
    if not os.environ.get("GITHUB_LINK_TOKEN"):
        raise SystemExit("GITHUB_LINK_TOKEN is not set")
    if len(argv) == 3 and argv[0] == "fetch":
        fetch(argv[1].lstrip("v"), argv[2])
    elif len(argv) == 3 and argv[0] == "publish":
        publish(argv[1].lstrip("v"), argv[2])
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])
