"""The release scripts' pure checks. Run: python3 -m unittest discover -s ci -p 'test_*.py'"""

import unittest

import release
from versions import cargo_version, format_sums, parse_sums, parse_version, should_be_latest, tag_matches_cargo

CARGO = """[workspace]
members = ["crates/linkd"]

[workspace.package]
version = "0.1.10"
edition = "2021"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
"""

REPO = "therealtimex/realtimex-link"
SHA = "a" * 40


class Versions(unittest.TestCase):
    def test_cargo_version_is_the_workspace_package_one(self):
        self.assertEqual(cargo_version(CARGO), "0.1.10")
        with self.assertRaises(ValueError):
            cargo_version('[workspace.dependencies]\nversion = "1"\n')

    def test_tag_must_match_cargo(self):
        self.assertIsNone(tag_matches_cargo("v0.1.10", CARGO))
        self.assertIn("0.1.10", tag_matches_cargo("v0.1.11", CARGO))
        self.assertIsNotNone(tag_matches_cargo("0.1.10", CARGO))

    def test_parse_version(self):
        self.assertEqual(parse_version("v0.1.10"), (0, 1, 10))
        self.assertIsNone(parse_version("v0.1.10-rc1"))

    def test_latest_only_when_highest(self):
        self.assertTrue(should_be_latest("0.1.10", ["v0.1.9", "v0.1.2"]))
        # Numeric, not string, order: 0.1.10 > 0.1.9.
        self.assertFalse(should_be_latest("0.1.9", ["v0.1.10"]))
        self.assertTrue(should_be_latest("0.1.10", ["v0.1.10", "nightly"]))
        self.assertTrue(should_be_latest("0.2.0", []))

    def test_sums_round_trip(self):
        sums = {"b.tar.gz": "1" * 64, "a.deb": "2" * 64}
        self.assertEqual(parse_sums(format_sums(sums)), sums)
        self.assertEqual(parse_sums(f"{'3' * 64} *c.rpm\n"), {"c.rpm": "3" * 64})


def run(**changes):
    item = {
        "path": ".github/workflows/build.yml",
        "event": "workflow_dispatch",
        "head_branch": "v0.1.10",
        "head_sha": SHA,
        "status": "completed",
        "conclusion": "success",
        "repository": {"full_name": REPO},
        "head_repository": {"full_name": REPO},
    }
    item.update(changes)
    return item


class Provenance(unittest.TestCase):
    def test_the_tag_build_run_passes(self):
        self.assertEqual(release.run_problems(run(), REPO, SHA, "v0.1.10"), [])

    def test_other_runs_fail(self):
        for changes in (
            {"head_sha": "b" * 40},
            {"head_branch": "main"},
            {"path": ".github/workflows/other.yml"},
            {"conclusion": "failure"},
            {"event": "push"},
            {"head_repository": {"full_name": "someone/fork"}},
        ):
            self.assertTrue(release.run_problems(run(**changes), REPO, SHA, "v0.1.10"), changes)

    def test_the_release_must_be_a_single_draft(self):
        draft = {"tag_name": "v0.1.10", "draft": True}
        self.assertIs(release.the_release([draft, {"tag_name": "v0.1.9", "draft": False}], "v0.1.10"), draft)
        with self.assertRaises(SystemExit):
            release.the_release([{"tag_name": "v0.1.10", "draft": False}], "v0.1.10")
        with self.assertRaises(SystemExit):
            release.the_release([draft, dict(draft)], "v0.1.10")
        with self.assertRaises(SystemExit):
            release.the_release([], "v0.1.10")


def asset(name, sha=None, uploader="github-actions[bot]"):
    return {"name": name, "uploader": {"login": uploader}, "digest": f"sha256:{sha}" if sha else None}


class DraftAssets(unittest.TestCase):
    built = {"linux.deb": "1" * 64, "mac.tar.gz": "2" * 64}
    signed = {"mac.tar.gz": "9" * 64, "Link.dmg": "8" * 64}

    def test_as_built(self):
        assets = [asset("linux.deb", "1" * 64), asset("mac.tar.gz", "2" * 64), asset("SHA256SUMS")]
        self.assertEqual(release.asset_problems(assets, self.built, self.signed), ([], []))

    def test_a_partial_publish_is_replaced_again(self):
        # An earlier publish replaced the macOS archive and added the .dmg, then failed.
        assets = [asset("linux.deb", "1" * 64), asset("mac.tar.gz", "7" * 64, "someone"), asset("Link.dmg", "6" * 64, "someone")]
        self.assertEqual(release.asset_problems(assets, self.built, self.signed), ([], []))

    def test_swapped_or_foreign_files_fail(self):
        for assets in (
            [asset("linux.deb", "5" * 64), asset("mac.tar.gz")],
            [asset("linux.deb", "1" * 64, "someone"), asset("mac.tar.gz")],
            [asset("linux.deb", "1" * 64), asset("evil.exe", "4" * 64)],
            [asset("mac.tar.gz")],
        ):
            problems, _ = release.asset_problems(assets, self.built, self.signed)
            self.assertTrue(problems, assets)

    def test_no_digest_means_hash_it(self):
        assets = [asset("linux.deb"), asset("mac.tar.gz")]
        problems, unchecked = release.asset_problems(assets, self.built, self.signed)
        self.assertEqual(problems, [])
        self.assertEqual([a["name"] for a in unchecked], ["linux.deb"])


def released(tag, draft=False, prerelease=False, sums=True):
    assets = [{"name": "SHA256SUMS"}] if sums else []
    return {"tag_name": tag, "draft": draft, "prerelease": prerelease, "assets": assets, "id": tag}


class PreRelease(unittest.TestCase):
    def test_publish_is_always_a_pre_release_never_latest(self):
        payload = release.publish_payload("0.2.1")
        self.assertIs(payload["prerelease"], True)
        self.assertEqual(payload["make_latest"], "false")
        self.assertIs(payload["draft"], False)

    def test_promote_makes_the_newest_latest(self):
        releases = [released("v0.1.11"), released("v0.2.0", prerelease=True), released("v0.2.1", prerelease=True)]
        item, payload = release.promote_payload("0.2.1", releases)
        self.assertEqual(item["tag_name"], "v0.2.1")
        self.assertEqual(payload, {"prerelease": False, "make_latest": "true"})

    def test_promoting_an_older_version_never_moves_latest_back(self):
        releases = [released("v0.1.11"), released("v0.2.0", prerelease=True), released("v0.2.1")]
        _, payload = release.promote_payload("0.2.0", releases)
        self.assertEqual(payload, {"prerelease": False, "make_latest": "false"})

    def test_promote_refuses_a_draft_a_missing_release_or_no_sums(self):
        for releases in (
            [released("v0.2.1", draft=True)],
            [released("v0.2.0")],
            [released("v0.2.1", prerelease=True, sums=False)],
            [released("v0.2.1", prerelease=True), released("v0.2.1", prerelease=True)],
        ):
            with self.assertRaises(SystemExit):
                release.promote_payload("0.2.1", releases)


if __name__ == "__main__":
    unittest.main()
