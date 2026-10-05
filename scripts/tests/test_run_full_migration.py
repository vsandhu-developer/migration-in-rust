"""Tests for scripts/run_full_migration.py with a fake importer binary and fake generator (no network)."""
import json
import os
import stat
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "run_full_migration.py"

FAKE_BINARY = textwrap.dedent('''\
    #!/usr/bin/env python3
    import json, os, sys
    from pathlib import Path
    argv = sys.argv[1:]
    def arg(name):
        return argv[argv.index(name) + 1] if name in argv else None
    with open(os.environ["FAKE_LOG"], "a") as f:
        f.write(json.dumps(argv) + "\\n")
    if not os.environ.get("DS_TEST_TOKEN"):
        print(json.dumps({"code": "target_token_missing"}), file=sys.stderr); sys.exit(1)
    phase, label, cp, resume = arg("--phase"), arg("--run-id"), arg("--checkpoint"), "--resume" in argv
    fail = os.environ.get("FAKE_FAIL_ONCE")
    marker = Path(os.environ["FAKE_LOG"] + ".failed")
    if phase == "inventory":
        assert "--dry-run" in argv and cp is None
    elif phase in ("publish", "release-comments"):
        assert resume and (Path(cp) / "run.json").exists(), "transition without import checkpoint"
    else:
        p = Path(cp)
        if p.exists() and not resume:
            print(json.dumps({"code": "checkpoint_exists_use_resume"}), file=sys.stderr); sys.exit(1)
        if resume and not p.exists():
            print(json.dumps({"code": "checkpoint_resume_missing"}), file=sys.stderr); sys.exit(1)
        p.mkdir(exist_ok=True)
        if phase in ("users", "admin-users"):
            (p / "users-state.json").write_text(json.dumps({"mapping": {"5": 50}}))
        else:
            (p / "run.json").write_text(json.dumps({"runId": "dsrun-x"}))
    if fail and phase == "import" and label.endswith(fail) and not marker.exists():
        marker.write_text("1")
        print(json.dumps({"selected": 1, "failed": 1, "failures": [{"code": "cms_unavailable"}]})); sys.exit(1)
    ids = (arg("--source-ids") or "").split(",")
    print(json.dumps({"phase": phase, "selected": len(ids), "created": len(ids), "failed": 0}))
''')

FAKE_GENERATOR = textwrap.dedent('''\
    #!/usr/bin/env python3
    import json, os, sys
    from pathlib import Path
    argv = sys.argv[1:]
    with open(os.environ["FAKE_LOG"], "a") as f:
        f.write(json.dumps(["GENERATOR"] + argv) + "\\n")
    for path in [argv[i + 1] for i, a in enumerate(argv) if a == "--user-mapping"]:
        json.loads(Path(path).read_text())["mapping"]
    out = Path(argv[argv.index("--out-dir") + 1])
    out.mkdir(parents=True)
    def manifest(d, h):
        d.mkdir()
        (d / "manifest.json").write_text(json.dumps({"manifestHash": h * 64, "types": {}}))
    manifest(out / "foundation", "a")
    perf = [str(i) for i in range(1, 1501)]
    chunks = [{"type": "category", "part": 1, "sourceIds": ["c1"]}, {"type": "subCategory", "part": 1, "sourceIds": ["c1/s1"]},
              {"type": "author", "part": 1, "sourceIds": ["7"]}, {"type": "performer", "part": 1, "sourceIds": perf[:1000]},
              {"type": "performer", "part": 2, "sourceIds": perf[1000:]}, {"type": "studio", "part": 1, "sourceIds": ["3583"]}]
    (out / "foundation" / "selection.json").write_text(json.dumps({"chunks": chunks}))
    manifest(out / "articles-01", "b")
    (out / "articles-01" / "source-ids.txt").write_text("101,102\\n")
    (out / "articles-01" / "comment-source-ids.txt").write_text(",".join(str(i) for i in range(1, 1201)) + "\\n")
    manifest(out / "articles-02", "c")
    (out / "articles-02" / "source-ids.txt").write_text("103\\n")
    (out / "SUMMARY.json").write_text(json.dumps({"counts": {"article": 3}, "skippedPosts": 0, "comments": {"fetched": 1200}}))
''')


def executable(path, text):
    path.write_text(text)
    path.chmod(path.stat().st_mode | stat.S_IEXEC)
    return path


class Orchestrator(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-orch-test-"))
        self.state = self.tmp / "state"
        self.log = self.tmp / "calls.log"
        self.binary = executable(self.tmp / "fake-importer", FAKE_BINARY)
        self.generator = executable(self.tmp / "fake-generator.py", FAKE_GENERATOR)
        self.config = self.tmp / "config.json"
        self.config.write_text(json.dumps({"target": "https://cms.example.invalid/", "targetTokenEnv": "DS_TEST_TOKEN"}))
        self.env = dict(os.environ, FAKE_LOG=str(self.log), DS_TEST_TOKEN="t" * 32)

    def run_orch(self, *extra, env=None, base=True):
        argv = [sys.executable, str(SCRIPT), "--state-dir", str(self.state), "--binary", str(self.binary), "--generator", str(self.generator)]
        if base:
            argv += ["--config", str(self.config), "--count", "3", "--batch-size", "2", "--comments-approval", "CMT", "--publication-approval", "PUB"]
        return subprocess.run(argv + list(extra), capture_output=True, text=True, env=env or self.env)

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def summary(self, calls):
        out = []
        for c in calls:
            if c[0] == "GENERATOR":
                out.append("generate")
            else:
                out.append(f"{c[c.index('--phase') + 1]}:{c[c.index('--run-id') + 1]}" + (":resume" if "--resume" in c else ""))
        return out

    def test_dry_run_prints_commands_and_writes_nothing(self):
        r = self.run_orch("--dry-run")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertFalse(self.state.exists())
        self.assertFalse(self.log.exists())
        self.assertIn("--phase admin-users", r.stdout)
        self.assertIn("--phase users", r.stdout)
        gen_line = next(line for line in r.stdout.splitlines() if str(self.generator) in line)
        users = gen_line.index("checkpoints/users/users-state.json")
        admins = gen_line.index("checkpoints/admin-users/users-state.json")
        self.assertLess(users, admins)
        self.assertIn("--fallback-user-id 3", gen_line)
        self.assertIn("--comments-approval CMT", gen_line)
        self.assertNotIn("t" * 32, r.stdout + r.stderr)

    def test_full_flow_stops_at_gate_then_continues_and_is_idempotent(self):
        r = self.run_orch()
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("STOP: register the generated manifests", r.stdout)
        self.assertIn(str(self.state / "manifests" / "foundation" / "manifest.json"), r.stdout)
        first = self.summary(self.calls())
        self.assertEqual(first, ["admin-users:ds-full-admin-users", "users:ds-full-users", "generate"])
        gen = self.calls()[2]
        maps = [gen[i + 1] for i, a in enumerate(gen) if a == "--user-mapping"]
        self.assertEqual([Path(m).parent.name for m in maps], ["users", "admin-users"])
        self.assertEqual(gen[gen.index("--fallback-user-id") + 1], "3")
        state = json.loads((self.state / "orchestrator-state.json").read_text())
        self.assertEqual({k: v["status"] for k, v in state["stages"].items()},
                         {"admin-users": "completed", "users": "completed", "generate": "completed"})

        # Without --continue, the gate stays closed.
        r = self.run_orch()
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.calls()), 3)

        r = self.run_orch("--continue")
        self.assertEqual(r.returncode, 0, r.stderr)
        second = self.summary(self.calls()[3:])
        fdn = ["fdn-category-1", "fdn-subCategory-1", "fdn-author-1", "fdn-performer-1", "fdn-performer-2", "fdn-studio-1"]
        arts, cmts = ["art-01", "art-02"], ["cmt-01-1", "cmt-01-2"]
        expected = ([f"inventory:ds-full-{u}" for u in fdn + arts + cmts] + [f"import:ds-full-{u}" for u in fdn + arts + cmts]
                    + [f"publish:ds-full-{u}:resume" for u in fdn + arts] + [f"release-comments:ds-full-{u}:resume" for u in cmts])
        self.assertEqual(second, expected)
        comment_chunks = [c for c in self.calls()[3:] if c[c.index("--phase") + 1] == "import" and "comment" in c]
        self.assertEqual([len(c[c.index("--source-ids") + 1].split(",")) for c in comment_chunks], [1000, 200])
        for c in self.calls()[3:]:
            self.assertLessEqual(len(c[c.index("--source-ids") + 1].split(",")), 1000)

        n = len(self.calls())
        r = self.run_orch()
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.calls()), n)
        self.assertIn("already completed", r.stdout)

    def test_failed_unit_resumes_from_its_checkpoint(self):
        self.assertEqual(self.run_orch().returncode, 0)
        env = dict(self.env, FAKE_FAIL_ONCE="fdn-performer-2")
        r = self.run_orch("--continue", env=env)
        self.assertEqual(r.returncode, 1)
        self.assertIn("fdn-performer-2", r.stderr)
        self.assertIn("cms_unavailable", r.stderr)
        before = len(self.calls())
        r = self.run_orch("--from", "foundation", env=env)
        self.assertEqual(r.returncode, 0, r.stderr)
        resumed = self.summary(self.calls()[before:])
        self.assertEqual(resumed[0], "import:ds-full-fdn-performer-2:resume")
        self.assertNotIn("import:ds-full-fdn-performer-1", resumed)

    def test_changed_settings_are_rejected(self):
        self.assertEqual(self.run_orch("--only", "admin-users").returncode, 0)
        r = subprocess.run([sys.executable, str(SCRIPT), "--state-dir", str(self.state), "--binary", str(self.binary),
                            "--count", "99", "--only", "users"], capture_output=True, text=True, env=self.env)
        self.assertEqual(r.returncode, 1)
        self.assertIn("--count differs", r.stderr)
        # Omitted settings are taken from the state directory.
        r = subprocess.run([sys.executable, str(SCRIPT), "--state-dir", str(self.state), "--binary", str(self.binary),
                            "--only", "users"], capture_output=True, text=True, env=self.env)
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_post_gate_stage_requires_register(self):
        r = self.run_orch("--only", "foundation")
        self.assertEqual(r.returncode, 1)
        self.assertIn("register gate", r.stderr)

    def test_missing_token_stops_before_any_call(self):
        env = {k: v for k, v in self.env.items() if k != "DS_TEST_TOKEN"}
        r = self.run_orch(env=env)
        self.assertEqual(r.returncode, 1)
        self.assertIn("DS_TEST_TOKEN", r.stderr)
        self.assertEqual(self.calls(), [])

    def test_fallback_can_be_disabled_and_overridden(self):
        r = self.run_orch("--dry-run", "--no-fallback-user")
        self.assertNotIn("--fallback-user-id", r.stdout)
        r = self.run_orch("--dry-run", "--fallback-user-id", "42")
        self.assertIn("--fallback-user-id 42", r.stdout)

    def test_state_dir_inside_repository_is_rejected(self):
        r = subprocess.run([sys.executable, str(SCRIPT), "--state-dir", str(SCRIPT.parent / "state"), "--dry-run"],
                           capture_output=True, text=True, env=self.env)
        self.assertEqual(r.returncode, 1)
        self.assertIn("outside the repository", r.stderr)


if __name__ == "__main__":
    unittest.main()
