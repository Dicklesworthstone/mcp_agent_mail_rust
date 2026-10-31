#!/usr/bin/env python3
"""Real Git regressions for the generated immutable-commit guard.

Cargo supplies native-rendered templates. The standalone mode extracts the
same literal template and performs the renderer's substitutions; it is useful
when the full Rust workspace cannot be built, but is not a native CLI test.
Every fixture is private and retained, including failures.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


OPTIONS = argparse.ArgumentParser(add_help=False)
OPTIONS.add_argument("--templates", type=Path)
ARGS, REMAINING = OPTIONS.parse_known_args()
ROOT = Path(tempfile.mkdtemp(prefix="am-guard-commit-admission-"))
SOURCE = Path(__file__).resolve().parents[1] / "src" / "lib.rs"
TEMPLATE = SOURCE.read_text().split('    let template = r##"', 1)[1].split('"##;', 1)[0]
POLICY = hashlib.sha256(TEMPLATE.encode()).hexdigest()
PROJECT_TOKEN = "__ADMISSION_TEST_PROJECT__"


def render(project, hook):
    if ARGS.templates:
        script = (ARGS.templates / hook).read_text()
        if f'ADMISSION_POLICY = "{POLICY}"' not in script:
            raise AssertionError("native renderer policy digest differs from its literal template")
        return script.replace(json.dumps(PROJECT_TOKEN), json.dumps(str(project))).replace(
            PROJECT_TOKEN, str(project)
        )
    return (
        TEMPLATE.replace("__HOOK_NAME_TEXT__", hook)
        .replace("__PROJECT_TEXT__", str(project))
        .replace("__PROJECT_JSON__", json.dumps(str(project)))
        .replace("__HOOK_NAME_JSON__", json.dumps(hook))
        .replace("__STORAGE_ROOT_JSON__", '""')
        .replace("__ADMISSION_POLICY_SHA256__", POLICY)
    )


class Repository:
    def __init__(self, name, initial=True):
        self.root = ROOT / name
        self.repo = self.root / "repo"
        self.repo.mkdir(parents=True)
        self.home = self.root / "home"
        self.home.mkdir()
        empty_config = self.root / "empty.gitconfig"
        empty_config.write_text("")
        self.env = {
            "PATH": os.environ["PATH"],
            "HOME": str(self.home),
            "TMPDIR": str(self.root),
            "STORAGE_ROOT": str(self.root / "archive"),
            "AGENT_NAME": "Publisher",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": str(empty_config),
            "GIT_TERMINAL_PROMPT": "0",
        }
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Publisher")
        self.git("config", "user.email", "publisher@example.invalid")
        self.git("config", "core.hooksPath", str(self.repo / ".git" / "hooks"))
        self.base = self.commit_file("leased.txt", "base\n", "base") if initial else None

    def run(self, *args, data=None, env=None, check=True):
        combined = dict(self.env)
        if env:
            combined.update(env)
        result = subprocess.run(
            args, cwd=self.repo, env=combined, input=data, text=True,
            capture_output=True, timeout=20,
        )
        if check and result.returncode:
            raise AssertionError((args, result.returncode, result.stdout, result.stderr))
        return result

    def git(self, *args, **kwargs):
        return self.run("git", *args, **kwargs).stdout.strip()

    def install(self, *hooks):
        for hook in hooks:
            path = self.repo / ".git" / "hooks" / hook
            path.write_text(render(self.repo, hook))
            path.chmod(0o755)

    def commit_file(self, path, content, message, env=None):
        target = self.repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)
        self.git("add", "--", path)
        self.git("commit", "-qm", message, env=env)
        return self.git("rev-parse", "HEAD")

    def lease(self, pattern="leased.txt", holder="Editor", directory=None):
        directory = directory or self.repo / "file_reservations"
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / "lease.json"
        path.write_text(json.dumps({
            "path_pattern": pattern, "agent_name": holder, "exclusive": True,
            "expires_ts": "2099-01-01T00:00:00Z", "released_ts": None,
        }))
        return path

    def records(self):
        return list((self.repo / ".git" / "agent-mail-guard-admissions-v1").glob("*.json"))

    def guard(self, hook, base=None, tip=None, env=None):
        path = self.repo / ".git" / "hooks" / hook
        if not path.exists():
            self.install(hook)
        data = None
        if hook == "pre-push":
            tip = tip or self.git("rev-parse", "HEAD")
            base = self.base if base is None else base
            data = f"refs/heads/main {tip} refs/heads/main {base}\n"
        return self.run(sys.executable, str(path), data=data, env=env, check=False)


@unittest.skipUnless(os.name == "posix", "admission requires POSIX directory-relative no-follow I/O")
class CommitAdmissionTests(unittest.TestCase):
    def repository(self, suffix="", initial=True):
        return Repository(self._testMethodName + suffix, initial)

    def assert_refused(self, result):
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("reservation conflict", result.stderr)

    def test_later_lease_allows_real_push_without_touching_dirty_index(self):
        repo = self.repository()
        remote = repo.root / "remote.git"
        repo.git("init", "-q", "--bare", str(remote))
        repo.git("push", "-q", str(remote), "main")
        repo.install("pre-commit", "post-commit", "pre-push")
        tip = repo.commit_file("leased.txt", "committed snapshot\n", "snapshot")
        self.assertEqual(len(repo.records()), 1)
        repo.lease()
        target = repo.repo / "leased.txt"
        target.write_text("staged edit\n")
        repo.git("add", "leased.txt")
        target.write_text("unstaged edit\n")
        index = repo.repo / ".git" / "index"
        before = (index.read_bytes(), target.read_bytes())
        pushed = repo.run("git", "push", "-q", str(remote), "main", check=False)
        self.assertEqual(pushed.returncode, 0, pushed.stderr)
        self.assertEqual(before, (index.read_bytes(), target.read_bytes()))
        self.assertIn(tip, repo.git("ls-remote", str(remote), "refs/heads/main"))
        self.assert_refused(repo.run("git", "commit", "-m", "foreign edit", check=False))

    def test_admitted_tip_never_exempts_unadmitted_ancestor(self):
        repo = self.repository()
        repo.commit_file("leased.txt", "unchecked ancestor\n", "unchecked")
        repo.install("post-commit", "pre-push")
        repo.commit_file("other.txt", "checked tip\n", "checked")
        self.assertEqual(len(repo.records()), 1)
        repo.lease()
        self.assert_refused(repo.guard("pre-push"))

    def test_explicit_bypass_warn_and_disabled_enforcement_never_record(self):
        for key, value in [
            ("AGENT_MAIL_BYPASS", "1"),
            ("AGENT_MAIL_GUARD_MODE", "warn"),
            ("FILE_RESERVATIONS_ENFORCEMENT_ENABLED", "false"),
        ]:
            with self.subTest(key=key):
                repo = self.repository("-" + key)
                repo.install("post-commit", "pre-push")
                repo.commit_file("leased.txt", "unchecked\n", "unchecked", env={key: value})
                self.assertEqual(repo.records(), [])
                repo.lease()
                self.assert_refused(repo.guard("pre-push"))

    def test_post_commit_independently_checks_no_verify_commit(self):
        repo = self.repository()
        repo.install("pre-commit", "post-commit", "pre-push")
        repo.lease()
        (repo.repo / "leased.txt").write_text("foreign mutation\n")
        repo.git("add", "leased.txt")
        result = repo.run("git", "commit", "--no-verify", "-qm", "unchecked")
        self.assertIn("commit remains unadmitted", result.stderr)
        self.assertEqual(repo.records(), [])
        self.assert_refused(repo.guard("pre-push"))

    def test_foreign_repository_key_and_receipts_cannot_be_copied(self):
        source = self.repository("-source")
        source.install("post-commit")
        tip = source.commit_file("leased.txt", "copied commit\n", "checked")
        self.assertEqual(len(source.records()), 1)
        target = self.repository("-target")
        target.git("fetch", "-q", str(source.repo), tip)
        target.git("checkout", "--detach", "-q", tip)
        namespace = "agent-mail-guard-admissions-v1"
        shutil.copytree(source.repo / ".git" / namespace, target.repo / ".git" / namespace)
        target.lease()
        self.assert_refused(target.guard("pre-push", base="0" * 40))

    def test_tampered_receipt_and_changed_project_policy_are_not_authority(self):
        repo = self.repository()
        repo.install("post-commit", "pre-push")
        repo.commit_file("leased.txt", "checked\n", "checked")
        record = repo.records()[0]
        data = json.loads(record.read_text())
        data["payload"]["mode"] = "warn"
        record.write_text(json.dumps(data))
        repo.lease()
        self.assert_refused(repo.guard("pre-push"))
        data["payload"]["mode"] = "block"
        record.write_text(json.dumps(data))
        program = repo.repo / ".git" / "hooks" / "pre-push"
        # A different slug still uses the local repository archive/anchor,
        # so the conflicting path remains in scope while receipt identity differs.
        program.write_text(render("different-project", "pre-push"))
        repo.lease("**")
        self.assert_refused(repo.guard("pre-push"))

    def test_replacement_refs_and_grafts_cannot_hide_committed_mutations(self):
        for mode in ("replace", "graft"):
            with self.subTest(mode=mode):
                repo = self.repository("-" + mode)
                danger = repo.commit_file("leased.txt", "foreign mutation\n", "danger")
                repo.lease()
                repo.install("post-commit", "pre-push")
                if mode == "replace":
                    repo.git("replace", danger, repo.base)
                else:
                    tree = repo.git("rev-parse", danger + "^{tree}")
                    parent = repo.run("git", "commit-tree", tree, data="fake parent\n").stdout.strip()
                    (repo.repo / ".git" / "info" / "grafts").write_text(danger + " " + parent + "\n")
                post = repo.guard("post-commit")
                self.assertEqual(post.returncode, 0, post.stderr)
                self.assertIn("commit remains unadmitted", post.stderr)
                self.assertEqual(repo.records(), [])
                self.assert_refused(repo.guard("pre-push"))

    def test_merge_admission_checks_every_parent_and_push_keeps_branch_history(self):
        repo = self.repository()
        repo.git("checkout", "-qb", "side")
        repo.commit_file("leased.txt", "unadmitted branch\n", "side change")
        repo.git("checkout", "-q", "main")
        repo.commit_file("other.txt", "main change\n", "main change")
        repo.git("merge", "--no-ff", "-qm", "merge", "side")
        repo.lease()
        repo.install("post-commit", "pre-push")
        post = repo.guard("post-commit")
        self.assertIn("commit remains unadmitted", post.stderr)
        self.assertEqual(repo.records(), [])
        self.assert_refused(repo.guard("pre-push"))

    def test_shallow_boundary_cannot_certify_a_hidden_deletion(self):
        repo = self.repository()
        repo.git("update-index", "--force-remove", "leased.txt")
        repo.git("commit", "-qm", "delete reserved pathname")
        tip = repo.git("rev-parse", "HEAD")
        (repo.repo / ".git" / "shallow").write_text(tip + "\n")
        repo.lease()
        repo.install("post-commit")
        result = repo.guard("post-commit")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("commit remains unadmitted", result.stderr)
        self.assertEqual(repo.records(), [])

    def test_rename_checks_both_names_and_root_commit_can_be_admitted(self):
        repo = self.repository("-rename")
        repo.git("mv", "leased.txt", "renamed.txt")
        repo.git("commit", "-qm", "rename")
        repo.lease("leased.txt")
        repo.install("post-commit", "pre-push")
        self.assertIn("commit remains unadmitted", repo.guard("post-commit").stderr)
        self.assertEqual(repo.records(), [])
        self.assert_refused(repo.guard("pre-push"))
        root = self.repository("-root", initial=False)
        root.install("post-commit", "pre-push")
        root.commit_file("leased.txt", "root snapshot\n", "root")
        self.assertEqual(len(root.records()), 1)
        root.lease()
        self.assertEqual(root.guard("pre-push", base="0" * 40).returncode, 0)

    def test_scan_cap_and_malformed_reservations_never_admit(self):
        repo = self.repository("-cap")
        repo.install("post-commit", "pre-push")
        (repo.repo / "a.txt").write_text("a\n")
        (repo.repo / "b.txt").write_text("b\n")
        repo.git("add", "a.txt", "b.txt")
        repo.git("commit", "-qm", "capped", env={"AGENT_MAIL_GUARD_PUSH_MAX_PATHS": "1"})
        self.assertEqual(repo.records(), [])
        repo.lease("a.txt")
        self.assert_refused(repo.guard("pre-push"))
        bad = self.repository("-malformed")
        lease = bad.lease()
        lease.write_text("{malformed")
        bad.install("post-commit")
        bad.commit_file("leased.txt", "unverified\n", "unverified")
        self.assertEqual(bad.records(), [])

    def test_fifo_key_and_linked_namespace_refuse_without_blocking(self):
        for mode in ("fifo", "symlink"):
            repo = self.repository("-" + mode)
            namespace = repo.repo / ".git" / "agent-mail-guard-admissions-v1"
            if mode == "fifo":
                namespace.mkdir(mode=0o700)
                os.mkfifo(namespace / "key", 0o600)
            else:
                sentinel = repo.root / "outside"
                sentinel.mkdir(mode=0o700)
                namespace.symlink_to(sentinel, target_is_directory=True)
            repo.install("post-commit", "pre-push")
            repo.commit_file("leased.txt", "checked but no receipt\n", "changed")
            self.assertEqual(repo.records(), [])
            repo.lease()
            self.assert_refused(repo.guard("pre-push"))

    def test_new_external_archive_invalidates_absent_local_authority(self):
        repo = self.repository()
        repo.install("post-commit", "pre-push")
        repo.commit_file("leased.txt", "checked locally\n", "checked")
        self.assertEqual(len(repo.records()), 1)
        archive = Path(repo.env["STORAGE_ROOT"]) / "projects" / "registered-project"
        repo.lease(directory=archive / "file_reservations")
        (archive / "project.json").write_text(json.dumps({
            "slug": "registered-project", "human_key": str(repo.repo),
        }))
        self.assert_refused(repo.guard("pre-push"))

    def test_unexpected_receipt_reader_failure_cannot_hide_the_commit(self):
        repo = self.repository()
        tip = repo.commit_file("leased.txt", "unchecked\n", "unchecked")
        repo.install("pre-push")
        program = repo.repo / ".git" / "hooks" / "pre-push"
        probe = """
import runpy, sys, time
guard = runpy.run_path(sys.argv[1], run_name='guard_fault_probe')
class FailedReceiptRead:
    def admitted(self, oid):
        raise RuntimeError('receipt read failed')
paths = set()
guard['_scan_push_commits'](
    ['git', 'rev-list', sys.argv[2]], paths, 100, time.monotonic() + 5,
    admissions=FailedReceiptRead(),
)
raise AssertionError('an uninspected commit was reported as checked')
"""
        result = repo.run(sys.executable, "-c", probe, str(program),
                          f"{repo.base}..{tip}", check=False)
        self.assertEqual(result.returncode, 2, result.stderr)

    def test_deeply_malformed_receipt_falls_back_to_the_real_path_check(self):
        repo = self.repository()
        repo.install("post-commit", "pre-push")
        repo.commit_file("leased.txt", "checked\n", "checked")
        receipt = repo.records()[0]
        receipt.write_text('{"payload":' + '[' * 2000 + '0' + ']' * 2000 + '}')
        repo.lease()
        self.assert_refused(repo.guard("pre-push"))

    def test_replaced_receipt_directory_invalidates_an_already_open_store(self):
        repo = self.repository()
        repo.install("post-commit", "pre-push")
        tip = repo.commit_file("leased.txt", "checked\n", "checked")
        program = repo.repo / ".git" / "hooks" / "pre-push"
        probe = """
import os, runpy, sys
guard = runpy.run_path(sys.argv[1], run_name='guard_authority_probe')
store = guard['_AdmissionStore'](archive_root=None)
assert store.admitted(sys.argv[2])
namespace = os.path.join(store.common, guard['ADMISSION_DIRECTORY'])
os.rename(namespace, namespace + '.retained')
os.mkdir(namespace, 0o700)
assert not store.admitted(sys.argv[2]), 'replaced namespace reused stale authority'
store.close()
"""
        repo.run(sys.executable, "-c", probe, str(program), tip)

    def test_replaced_archive_invalidates_an_already_open_store(self):
        for mode in ("directory", "symlink"):
            with self.subTest(mode=mode):
                repo = self.repository("-" + mode)
                archive = Path(repo.env["STORAGE_ROOT"]) / "projects" / "registered-project"
                repo.lease(holder="Publisher", directory=archive / "file_reservations")
                (archive / "project.json").write_text(json.dumps({
                    "slug": "registered-project", "human_key": str(repo.repo),
                }))
                repo.install("post-commit", "pre-push")
                tip = repo.commit_file("leased.txt", "checked\n", "checked")
                self.assertEqual(len(repo.records()), 1)
                program = repo.repo / ".git" / "hooks" / "pre-push"
                probe = """
import json, os, pathlib, runpy, sys
program, oid, archive_text, mode = sys.argv[1:]
guard = runpy.run_path(program, run_name='guard_archive_authority_probe')
archive = pathlib.Path(archive_text)
store = guard['_AdmissionStore'](archive_root=archive_text)
assert store.admitted(oid)
metadata = (archive / 'project.json').read_bytes()
retained = archive.with_name(archive.name + '.retained')
os.rename(archive, retained)
if mode == 'symlink':
    archive.symlink_to(retained, target_is_directory=True)
else:
    (archive / 'file_reservations').mkdir(parents=True)
    (archive / 'project.json').write_bytes(metadata)
    (archive / 'file_reservations' / 'lease.json').write_text(json.dumps({
        'path_pattern': 'leased.txt', 'agent_name': 'Editor', 'exclusive': True,
        'expires_ts': '2099-01-01T00:00:00Z', 'released_ts': None,
    }))
    active, _ = guard['get_active_reservations'](strict=True)
    assert active[0]['agent_name'] == 'Editor'
assert not store.admitted(oid), 'replaced archive reused stale commit admission'
assert retained.is_dir(), 'the old archive must remain preserved'
store.close()
"""
                repo.run(sys.executable, "-c", probe, str(program), tip, str(archive), mode)

    def test_final_push_exclusions_recheck_changed_archive_authority(self):
        for mode in ("new-external", "replaced-external"):
            with self.subTest(mode=mode):
                repo = self.repository("-" + mode)
                archive = Path(repo.env["STORAGE_ROOT"]) / "projects" / "registered-project"
                if mode == "replaced-external":
                    repo.lease(holder="Publisher", directory=archive / "file_reservations")
                    (archive / "project.json").write_text(json.dumps({
                        "slug": "registered-project", "human_key": str(repo.repo),
                    }))
                repo.install("post-commit", "pre-push")
                tip = repo.commit_file("leased.txt", "checked\n", "checked")
                self.assertEqual(len(repo.records()), 1)
                program = repo.repo / ".git" / "hooks" / "pre-push"
                probe = """
import io, json, os, pathlib, runpy, sys
program, oid, base, archive_text, mode = sys.argv[1:]
guard = runpy.run_path(program, run_name='guard_final_authority_probe')
admissions = guard['_PushAdmissions']()
sys.stdin = io.StringIO(f'refs/heads/main {oid} refs/heads/main {base}\\n')
files, unchecked = guard['get_push_files'](admissions)
assert files == [] and unchecked == [], (files, unchecked)
archive = pathlib.Path(archive_text)
if mode == 'replaced-external':
    os.rename(archive, archive.with_name(archive.name + '.retained'))
(archive / 'file_reservations').mkdir(parents=True)
(archive / 'project.json').write_text(json.dumps({
    'slug': 'registered-project', 'human_key': str(pathlib.Path.cwd()),
}))
(archive / 'file_reservations' / 'lease.json').write_text(json.dumps({
    'path_pattern': 'leased.txt', 'agent_name': 'Editor', 'exclusive': True,
    'expires_ts': '2099-01-01T00:00:00Z', 'released_ts': None,
}))
active, _ = guard['get_active_reservations'](strict=True)
assert active[0]['agent_name'] == 'Editor'
try:
    admissions.verify_exclusions()
finally:
    admissions.close()
raise AssertionError('a changed archive authority accepted stale push exclusions')
"""
                result = repo.run(sys.executable, "-c", probe, str(program), tip,
                                  repo.base, str(archive), mode, check=False)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assert_refused(repo.guard("pre-push"))

    def test_later_lease_in_same_archive_preserves_valid_push_exclusions(self):
        repo = self.repository()
        archive = Path(repo.env["STORAGE_ROOT"]) / "projects" / "registered-project"
        repo.lease(holder="Publisher", directory=archive / "file_reservations")
        (archive / "project.json").write_text(json.dumps({
            "slug": "registered-project", "human_key": str(repo.repo),
        }))
        repo.install("post-commit", "pre-push")
        tip = repo.commit_file("leased.txt", "checked\n", "checked")
        self.assertEqual(len(repo.records()), 1)
        program = repo.repo / ".git" / "hooks" / "pre-push"
        probe = """
import io, json, pathlib, runpy, sys
program, oid, base, archive_text = sys.argv[1:]
guard = runpy.run_path(program, run_name='guard_stable_authority_probe')
admissions = guard['_PushAdmissions']()
sys.stdin = io.StringIO(f'refs/heads/main {oid} refs/heads/main {base}\\n')
files, unchecked = guard['get_push_files'](admissions)
assert files == [] and unchecked == [], (files, unchecked)
archive = pathlib.Path(archive_text)
(archive / 'file_reservations' / 'lease.json').write_text(json.dumps({
    'path_pattern': 'leased.txt', 'agent_name': 'Editor', 'exclusive': True,
    'expires_ts': '2099-01-01T00:00:00Z', 'released_ts': None,
}))
active, _ = guard['get_active_reservations'](strict=True)
assert active[0]['agent_name'] == 'Editor'
admissions.verify_exclusions()
assert admissions.admitted(oid), 'a later lease changed the same archive authority'
admissions.close()
"""
        repo.run(sys.executable, "-c", probe, str(program), tip, repo.base, str(archive))
        result = repo.guard("pre-push")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_admission_does_not_inspect_or_change_an_alternate_index(self):
        repo = self.repository()
        tip = repo.commit_file("leased.txt", "committed snapshot\n", "snapshot")
        repo.install("post-commit", "pre-push")
        alternate = repo.root / "alternate-index"
        repo.git("read-tree", repo.base, env={"GIT_INDEX_FILE": str(alternate)})
        target = repo.repo / "leased.txt"
        target.write_text("staged work\n")
        repo.git("add", "leased.txt")
        target.write_text("unstaged work\n")
        untracked = repo.repo / "untracked.txt"
        untracked.write_text("untracked\n")
        primary = repo.repo / ".git" / "index"
        before = tuple(path.read_bytes() for path in (primary, alternate, target, untracked))
        result = repo.guard("post-commit", env={"GIT_INDEX_FILE": str(alternate)})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(repo.records()), 1, result.stderr)
        self.assertEqual(before, tuple(path.read_bytes() for path in (primary, alternate, target, untracked)))
        repo.lease()
        self.assertEqual(repo.guard("pre-push", tip=tip).returncode, 0)

    def test_receipt_cannot_replay_for_another_oid_or_case_policy(self):
        repo = self.repository()
        repo.install("post-commit", "pre-push")
        admitted = repo.commit_file("leased.txt", "checked\n", "checked")
        receipt = repo.records()[0]
        unadmitted = repo.commit_file("leased.txt", "unchecked\n", "unchecked",
                                    env={"AGENT_MAIL_BYPASS": "1"})
        copied = receipt.with_name(receipt.name.replace(admitted, unadmitted))
        shutil.copyfile(receipt, copied)
        copied.chmod(0o600)
        repo.lease()
        self.assert_refused(repo.guard("pre-push", base=admitted))
        repo.git("config", "core.ignorecase", "true")
        self.assert_refused(repo.guard("pre-push", tip=admitted))

    def test_undecodable_git_path_is_never_certified_as_empty(self):
        repo = self.repository()
        path = os.fsencode(repo.repo) + b"/\xff"
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        os.write(fd, b"opaque path\n")
        os.close(fd)
        repo.git("add", "--all")
        repo.git("commit", "-qm", "opaque path")
        repo.install("post-commit")
        self.assertIn("commit remains unadmitted", repo.guard("post-commit").stderr)
        self.assertEqual(repo.records(), [])


if __name__ == "__main__":
    print("Retained real-Git fixtures:", ROOT, flush=True)
    unittest.main(argv=[__file__, *REMAINING], verbosity=2)
