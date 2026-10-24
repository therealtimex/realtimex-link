#!/usr/bin/env python3
"""Build this commit on GitHub Actions and wait for it (GitLab's `build` job).

GitLab is where Link's code lives; GitHub (a public mirror) has the build machines. This starts
the Build workflow on the mirror for the pipeline's branch or tag, checks the run is for this very
commit, waits for it and fails when it fails. A vX.Y.Z tag builds every platform and opens the
draft release the signing pipeline picks up.

On a tag it first checks that the tag is Cargo.toml's version (a release whose tag isn't the version
Link reports would be offered to every Link as an update, forever). When the run succeeds it writes
build.env (LINK_BUILD_RUN_ID, LINK_BUILD_SHA), the job's dotenv report: the verify job takes the
release's unsigned files from that very run's artifacts, not from whatever the draft holds by then.

Environment: GITHUB_LINK_TOKEN (Actions read/write on the mirror), CI_COMMIT_SHA, CI_COMMIT_TAG or
CI_COMMIT_BRANCH, and optionally GITHUB_REPOSITORY (default therealtimex/realtimex-link) and
BUILD_TARGETS (test|all; tags always build all).
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request

from versions import tag_matches_cargo

API = "https://api.github.com"
MIRROR_WAIT = 10 * 60
RUN_WAIT = 2 * 60 * 60


def call(method, path, body=None):
    request = urllib.request.Request(
        path if path.startswith("http") else API + path,
        method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={
            "Authorization": "Bearer " + os.environ["GITHUB_LINK_TOKEN"],
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "realtimex-link-gitlab-ci",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            text = response.read()
            return json.loads(text) if text else None
    except urllib.error.HTTPError as error:
        detail = error.read().decode(errors="replace")[:300]
        raise SystemExit(f"{method} {path}: HTTP {error.code} {detail}") from None


def main():
    if not os.environ.get("GITHUB_LINK_TOKEN"):
        raise SystemExit("GITHUB_LINK_TOKEN is not set (realtimex-link → Settings → CI/CD → Variables)")
    repo = os.environ.get("GITHUB_REPOSITORY", "therealtimex/realtimex-link")
    sha = os.environ["CI_COMMIT_SHA"]
    tag = os.environ.get("CI_COMMIT_TAG", "")
    ref = tag or os.environ["CI_COMMIT_BRANCH"]
    targets = "all" if tag else os.environ.get("BUILD_TARGETS", "test")

    if tag:
        with open("Cargo.toml", encoding="utf-8") as file:
            problem = tag_matches_cargo(tag, file.read())
        if problem:
            raise SystemExit(problem)
        print(f"{tag} is Cargo.toml's version")

    # The push mirror copies the commit to GitHub; wait until the ref points at it.
    kind = "tags" if tag else "heads"
    until = time.time() + MIRROR_WAIT
    while True:
        try:
            got = call("GET", f"/repos/{repo}/git/ref/{kind}/{ref}")
            # An annotated tag points at a tag object; follow it to the commit.
            target = got["object"]
            if target["type"] == "tag":
                target = call("GET", target["url"])["object"]
            if target["sha"] == sha:
                break
        except SystemExit as error:
            if "HTTP 404" not in str(error):
                raise
        if time.time() > until:
            raise SystemExit(f"{repo} has no {ref} at {sha[:8]} after {MIRROR_WAIT // 60} min: is the push mirror set up?")
        time.sleep(10)
    print(f"{repo} has {ref} at {sha[:8]}")

    started = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() - 5))
    call("POST", f"/repos/{repo}/actions/workflows/build.yml/dispatches", {"ref": ref, "inputs": {"targets": targets}})
    run = None
    for _ in range(60):
        runs = call(
            "GET",
            f"/repos/{repo}/actions/workflows/build.yml/runs?event=workflow_dispatch&head_sha={sha}&created=>={started}",
        )["workflow_runs"]
        # The same commit can be built for a branch and a tag at once: this ref's run only.
        runs = [r for r in runs if r.get("head_branch") == ref]
        if runs:
            run = runs[0]
            break
        time.sleep(5)
    if not run:
        raise SystemExit("GitHub Actions didn't start the build")
    print(f"GitHub Actions build ({targets}): {run['html_url']}", flush=True)

    until = time.time() + RUN_WAIT
    while run["status"] != "completed":
        if time.time() > until:
            raise SystemExit(f"the build didn't finish in {RUN_WAIT // 3600} h: {run['html_url']}")
        time.sleep(30)
        run = call("GET", f"/repos/{repo}/actions/runs/{run['id']}")
    jobs = call("GET", f"/repos/{repo}/actions/runs/{run['id']}/jobs")["jobs"]
    for job in jobs:
        print(f"  {job['name']}: {job['conclusion']}")
    if run["conclusion"] != "success":
        raise SystemExit(f"the build failed ({run['conclusion']}): {run['html_url']}")
    # For the later jobs (.gitlab-ci.yml: artifacts: reports: dotenv): which run built this commit.
    with open(os.environ.get("LINK_BUILD_ENV", "build.env"), "w", encoding="utf-8") as file:
        file.write(f"LINK_BUILD_RUN_ID={run['id']}\nLINK_BUILD_SHA={sha}\n")
    if tag:
        print(f"draft release: https://github.com/{repo}/releases (tag {tag})")


if __name__ == "__main__":
    sys.exit(main())
