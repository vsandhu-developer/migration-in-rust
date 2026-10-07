#!/usr/bin/env python3
"""End-to-end Daily Squirt migration: the old single-command flow (beba3fd main.rs) rebuilt from the
new pieces. Python 3 standard library only.

Stages, in order (each idempotent and resumable; completion is recorded in
<state-dir>/orchestrator-state.json, so a re-run skips completed stages/units):

  admin-users         Rust `--phase admin-users` (checkpoint <state>/checkpoints/admin-users)
  users               Rust `--phase users` (+ lookup reconciliation, inside the binary)
  generate            scripts/generate_wordpress_manifest.py -> <state>/manifests
                      (reader-user mappings only; anonymous fallback must be explicitly selected)
  register            GATE. Lists the manifests to commit to headless-strapi/config/ds-migration-manifests
                      through a reviewed PR and deploy. Never registers anything itself. Passed only by
                      re-running with --continue.
  inventory           `--phase inventory --dry-run` for every foundation chunk / article batch / comment chunk
  foundation          import, per type, per <= 1,000-ID chunk of foundation/selection.json
  articles            import of every articles-NN batch (--types article)
  comments            import of every articles-NN comment chunk (--types comment, <= 1,000 IDs)
  publish-foundation  `--phase publish --resume` per foundation chunk (same run/checkpoint as its import)
  publish-articles    `--phase publish --resume` per article batch
  release-comments    `--phase release-comments --resume` per comment chunk

Nothing secret is printed: the CMS token is read by the importer from the environment variable named
by the config's targetTokenEnv; this script only checks that the variable is set. Emails/usernames are
never read here (the users files are passed to the importer by path).
"""
import argparse
import datetime as dt
import fcntl
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
GENERATOR = REPO / "scripts" / "generate_wordpress_manifest.py"
STATE_FILE = "orchestrator-state.json"
STAGES = ("admin-users", "users", "generate", "register", "inventory", "foundation", "articles", "comments",
          "publish-foundation", "publish-articles", "release-comments")
POST_GATE = STAGES[STAGES.index("register") + 1:]
MAX_RUN_IDS = 1000
LABEL_RE = re.compile(r"^[A-Za-z0-9_-]{1,40}$")
DEFAULT_FALLBACK_USER_ID = 0  # Never attribute anonymous comments to an arbitrary real reader.
# Settings that shape run identities/manifests; fixed by the first real run of a state directory.
SETTINGS = ("config", "count", "batch_size", "foundation_scope", "comments_approval", "publication_approval",
            "fallback_user_id", "comment_user_mapping", "no_comments", "run_prefix", "users_file", "admin_users_file", "wp_api")
DEFAULTS = {"foundation_scope": "all", "run_prefix": "ds-full", "users_file": str(REPO / "data/source/users/users.json"),
            "admin_users_file": str(REPO / "data/source/users/admin_users.json"), "no_comments": False, "wp_api": None}


class Stop(Exception):
    """Operator-facing failure; message never contains secrets or personal data."""


def now():
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def say(message):
    print(message, flush=True)


# ---------------------------------------------------------------- state
class State:
    def __init__(self, directory, dry_run):
        self.dir = directory
        self.path = directory / STATE_FILE
        self.dry_run = dry_run
        self.data = {"version": 1, "settings": None, "stages": {}}
        if self.path.exists():
            self.data = json.loads(self.path.read_text())
            if self.data.get("version") != 1:
                raise Stop(f"{self.path}: unsupported state version")

    def save(self):
        if self.dry_run:
            return
        tmp = self.dir / f".{STATE_FILE}.tmp"
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as f:
            json.dump(self.data, f, indent=1, sort_keys=True)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, self.path)

    def stage(self, name):
        return self.data["stages"].setdefault(name, {"status": "pending", "units": {}})

    def done(self, name):
        return self.data["stages"].get(name, {}).get("status") == "completed"

    def unit_done(self, stage, unit):
        return self.data["stages"].get(stage, {}).get("units", {}).get(unit, {}).get("status") in ("completed", "completed-with-failures")

    def finish_unit(self, stage, unit, status, report):
        s = self.stage(stage)
        s["status"] = "running" if s["status"] != "completed" else s["status"]
        s["units"][unit] = {"status": status, "at": now(), "report": report}
        self.save()

    def finish_stage(self, name, **extra):
        s = self.stage(name)
        s.update(extra, status="completed", completedAt=now())
        self.save()


def external(path, label):
    p = Path(path).expanduser().resolve()
    lower = str(p).lower()
    if "daily-squirt-code" in lower or "migration-in-rust" in lower or str(p).startswith(str(REPO)):
        raise Stop(f"{label} must be outside the repository")
    return p


def resolve_settings(args, state):
    """Explicit arguments win only on the first real run; later runs must repeat them or omit them."""
    stored = state.data.get("settings") or {}
    given = {k: getattr(args, k) for k in SETTINGS}
    if args.no_fallback_user:
        if args.fallback_user_id is not None:
            raise Stop("--fallback-user-id and --no-fallback-user are mutually exclusive")
        given["fallback_user_id"] = 0
    if not args.no_comments:
        given["no_comments"] = None
    if given["config"] is not None:
        given["config"] = str(Path(given["config"]).expanduser().resolve())
    for k in ("users_file", "admin_users_file"):
        if given[k] is not None:
            given[k] = str(Path(given[k]).expanduser().resolve())
    if given["comment_user_mapping"] is not None:
        given["comment_user_mapping"] = [str(Path(p).expanduser().resolve()) for p in given["comment_user_mapping"]]
    settings = {}
    for k in SETTINGS:
        if k in stored:
            if given[k] is not None and given[k] != stored[k]:
                raise Stop(f"--{k.replace('_', '-')} differs from the value this state directory was started with; "
                           "use a new --state-dir for a different migration")
            settings[k] = stored[k]
        else:
            settings[k] = given[k] if given[k] is not None else DEFAULTS.get(k)
    if settings["fallback_user_id"] is None:
        settings["fallback_user_id"] = DEFAULT_FALLBACK_USER_ID
    missing = [k for k in ("config", "count", "batch_size", "publication_approval") if settings[k] in (None, "")]
    if not settings["no_comments"] and not settings["comments_approval"]:
        missing.append("comments_approval (or --no-comments)")
    if missing:
        raise Stop("missing required settings: " + ", ".join("--" + m.replace("_", "-") for m in missing))
    if not LABEL_RE.match(settings["run_prefix"]):
        raise Stop("--run-prefix must be 1..40 characters of A-Z a-z 0-9 _ -")
    if not (1 <= settings["count"] <= 20000 and 1 <= settings["batch_size"] <= MAX_RUN_IDS):
        raise Stop("--count 1..20000 and --batch-size 1..1000")
    if settings["fallback_user_id"] < 0:
        raise Stop("--fallback-user-id must be a positive native user id")
    return settings


# ---------------------------------------------------------------- plan
class Unit:
    def __init__(self, stage, name, cmd, kind, checkpoint=None, account_state=None):
        self.stage, self.name, self.cmd, self.kind = stage, name, cmd, kind
        self.checkpoint, self.account_state = checkpoint, account_state


class Plan:
    def __init__(self, args, settings, state_dir):
        self.args, self.s, self.dir = args, settings, state_dir
        self.manifests = state_dir / "manifests"
        self.checkpoints = state_dir / "checkpoints"
        self.config = settings["config"]
        self.prefix = settings["run_prefix"]

    def binary(self):
        return [str(self.args.binary)]

    def account_checkpoint(self, phase):
        return self.checkpoints / phase

    def account_unit(self, phase):
        checkpoint = self.account_checkpoint(phase)
        users_file = self.s["admin_users_file" if phase == "admin-users" else "users_file"]
        cmd = self.binary() + ["--config", self.config, "--phase", phase, "--run-id", f"{self.prefix}-{phase}",
                               "--users-file", users_file, "--checkpoint", str(checkpoint)]
        if self.args.account_batch_size:
            cmd += ["--batch-size", str(self.args.account_batch_size)]
        return Unit(phase, phase, cmd, "account", checkpoint, checkpoint / "users-state.json")

    def generate_cmd(self, out_dir):
        cmd = [sys.executable, str(self.args.generator), "--count", str(self.s["count"]), "--batch-size", str(self.s["batch_size"]),
               "--out-dir", str(out_dir), "--cache-dir", str(self.args.cache_dir or self.dir / "wp-cache"),
               "--foundation-scope", self.s["foundation_scope"], "--publication-approval", self.s["publication_approval"],
               "--download-workers", str(self.args.download_workers)]
        if self.s["wp_api"]:
            cmd += ["--wp-api", self.s["wp_api"]]
        if not self.s["no_comments"]:
            # Admin IDs belong to a separate table and cannot identify comment readers.
            paths = self.s["comment_user_mapping"] or [str(self.account_checkpoint("users") / "users-state.json")]
            for path in paths:
                cmd += ["--user-mapping", path]
            if self.s["fallback_user_id"]:
                cmd += ["--fallback-user-id", str(self.s["fallback_user_id"])]
            cmd += ["--comments-approval", self.s["comments_approval"]]
        return cmd

    # ---- generated manifests
    def generated(self):
        return (self.manifests / "SUMMARY.json").exists()

    def foundation_chunks(self):
        sel = json.loads((self.manifests / "foundation" / "selection.json").read_text())
        return [(f"fdn-{c['type']}-{c['part']}", c["type"], c["sourceIds"]) for c in sel["chunks"]]

    def article_batches(self):
        out = []
        for d in sorted(self.manifests.glob("articles-*")):
            ids = [i for i in (d / "source-ids.txt").read_text().strip().split(",") if i]
            if len(ids) > MAX_RUN_IDS:
                raise Stop(f"{d.name}: more than {MAX_RUN_IDS} article IDs")
            out.append((f"art-{d.name.split('-', 1)[1]}", d, ids))
        return out

    def comment_chunks(self):
        out = []
        if self.s["no_comments"]:
            return out
        for d in sorted(self.manifests.glob("articles-*")):
            f = d / "comment-source-ids.txt"
            if not f.exists():
                continue
            ids = [i for i in f.read_text().strip().split(",") if i]
            for n, start in enumerate(range(0, len(ids), MAX_RUN_IDS), 1):
                out.append((f"cmt-{d.name.split('-', 1)[1]}-{n}", d, ids[start:start + MAX_RUN_IDS]))
        return out

    def content_cmd(self, manifest, phase, label, types, ids, checkpoint=None, resume=False, dry=False):
        cmd = self.binary() + ["--config", self.config, "--manifest", str(manifest), "--phase", phase,
                               "--run-id", f"{self.prefix}-{label}", "--source-ids", ",".join(ids), "--types", types]
        if checkpoint is not None:
            cmd += ["--checkpoint", str(checkpoint)]
        if resume:
            cmd.append("--resume")
        if dry:
            cmd.append("--dry-run")
            if self.args.inventory_skip_images:
                cmd.append("--skip-images")
        cmd += ["--wp-per-page", str(self.args.wp_per_page)]
        if phase == "import" and self.args.media_concurrency:
            cmd += ["--media-concurrency", str(self.args.media_concurrency)]
        return cmd

    def content_units(self, stage):
        """Every (unit, cmd) of a post-gate stage. Import units resume iff their checkpoint exists."""
        fdir = self.manifests / "foundation" / "manifest.json"
        if stage in ("inventory", "foundation", "publish-foundation"):
            groups = [(label, fdir, typ, ids) for label, typ, ids in self.foundation_chunks()]
        else:
            groups = []
        if stage in ("inventory", "articles", "publish-articles"):
            groups += [(label, d / "manifest.json", "article", ids) for label, d, ids in self.article_batches()]
        if stage in ("inventory", "comments", "release-comments"):
            groups += [(label, d / "manifest.json", "comment", ids) for label, d, ids in self.comment_chunks()]
        units = []
        for label, manifest, typ, ids in groups:
            checkpoint = self.checkpoints / label
            if stage == "inventory":
                cmd = self.content_cmd(manifest, "inventory", label, typ, ids, dry=True)
                units.append(Unit(stage, label, cmd, "inventory"))
            elif stage in ("foundation", "articles", "comments"):
                units.append(Unit(stage, label, None, "import", checkpoint))
                units[-1].build = (manifest, label, typ, ids)
            else:
                phase = "release-comments" if stage == "release-comments" else "publish"
                cmd = self.content_cmd(manifest, phase, label, typ, ids, checkpoint, resume=True)
                units.append(Unit(stage, label, cmd, "transition", checkpoint))
        return units

    def import_cmd(self, unit):
        manifest, label, typ, ids = unit.build
        return self.content_cmd(manifest, "import", label, typ, ids, unit.checkpoint, resume=unit.checkpoint.exists())


# ---------------------------------------------------------------- execution
def token_check(config_path):
    try:
        config = json.loads(Path(config_path).read_text())
    except (OSError, ValueError):
        raise Stop("--config is unreadable or not JSON")
    env = config.get("targetTokenEnv")
    if not isinstance(env, str) or not re.fullmatch(r"[A-Z0-9_]+", env or ""):
        raise Stop("config targetTokenEnv is missing or invalid")
    if not os.environ.get(env):
        raise Stop(f"environment variable {env} (config targetTokenEnv) is not set; export the scoped CMS token first")
    return config


def summarize(report):
    keys = ("phase", "selected", "created", "updated", "existing", "skipped", "failed", "placeholderEmail", "alreadyComplete",
            "recovered", "mediaSelected", "mediaCreated", "mediaReused", "mediaFailed", "runId")
    out = {k: report[k] for k in keys if isinstance(report, dict) and k in report}
    if isinstance(report, dict) and report.get("failures"):
        codes = {}
        for f in report["failures"]:
            codes[f.get("code", "?")] = codes.get(f.get("code", "?"), 0) + 1
        out["failureCodes"] = codes
    return out


def run(cmd, log_dir, unit_name):
    """Runs one importer/generator command. stdout (a single JSON report without personal data) is
    parsed; stdout+stderr are also kept in <state>/logs/<unit>.log (0600) for the operator."""
    proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    log_dir.mkdir(mode=0o700, exist_ok=True)
    fd = os.open(log_dir / f"{unit_name}.log", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    with os.fdopen(fd, "a") as f:
        f.write(f"--- {now()} exit {proc.returncode}\n{proc.stdout}{proc.stderr}")
    report = None
    for line in reversed(proc.stdout.strip().splitlines()):
        try:
            report = json.loads(line)
            break
        except ValueError:
            continue
    error = None
    for line in reversed(proc.stderr.strip().splitlines()):
        try:
            parsed = json.loads(line)
            if isinstance(parsed, dict) and "code" in parsed:
                error = parsed["code"]
                break
        except ValueError:
            continue
    return proc.returncode, report, error


def clear_stale_checkpoint(unit):
    """A crash between directory creation and the first state write leaves a directory holding only
    writer.lock; the importer would then demand --resume and fail on the missing state. Remove only
    that exact empty shape."""
    cp = unit.checkpoint
    if cp is None or not cp.exists() or cp.is_symlink():
        return
    names = {p.name for p in cp.iterdir()}
    if names <= {"writer.lock"}:
        shutil.rmtree(cp)


def execute_unit(state, plan, unit, dry_run):
    if not dry_run and unit.kind in ("account", "import"):
        clear_stale_checkpoint(unit)
    if unit.kind == "import":
        cmd = plan.import_cmd(unit)  # --resume iff its checkpoint directory exists
    elif unit.kind == "account":
        cmd = list(unit.cmd) + (["--resume"] if unit.account_state.exists() else [])
    else:
        cmd = unit.cmd
    if dry_run:
        say(shlex.join(cmd))
        return
    if unit.kind == "transition" and not (unit.checkpoint / "run.json").exists():
        raise Stop(f"{unit.stage} {unit.name}: no import checkpoint; run its import stage first")
    say(f"[{unit.stage}] {unit.name}: running")
    code, report, error = run(cmd, plan.dir / "logs", f"{unit.stage}-{unit.name}")
    brief = summarize(report)
    if code == 0:
        state.finish_unit(unit.stage, unit.name, "completed", brief)
        say(f"[{unit.stage}] {unit.name}: ok {json.dumps(brief, sort_keys=True)}")
        return
    if unit.kind == "account" and plan.args.allow_account_failures and isinstance(report, dict) and report.get("failed", 0) > 0:
        state.finish_unit(unit.stage, unit.name, "completed-with-failures", brief)
        say(f"[{unit.stage}] {unit.name}: completed with {report['failed']} failed accounts (--allow-account-failures; "
            f"their comments use the fallback user) {json.dumps(brief, sort_keys=True)}")
        return
    state.finish_unit(unit.stage, unit.name, "failed", dict(brief, exitCode=code, error=error))
    raise Stop(f"[{unit.stage}] {unit.name}: exit {code}{' ' + error if error else ''} {json.dumps(brief, sort_keys=True)}; "
               f"see {plan.dir / 'logs'}; fix the cause and re-run the same command (the unit resumes from its checkpoint)")


def stage_generate(state, plan, dry_run):
    final = plan.manifests
    partial = plan.dir / "manifests.partial"
    if final.exists() and plan.generated():
        if not dry_run:
            state.finish_stage("generate", manifests=manifest_list(plan))
        return
    cmd = plan.generate_cmd(partial)
    if dry_run:
        say(shlex.join(cmd))
        say(f"# then: mv {shlex.quote(str(partial))} {shlex.quote(str(final))}")
        return
    if not plan.s["no_comments"]:
        paths = plan.s["comment_user_mapping"] or [str(plan.account_checkpoint("users") / "users-state.json")]
        for path in paths:
            if not Path(path).exists():
                raise Stop("generate needs all reader-user mappings; complete the users stage or supply --comment-user-mapping")
    if final.exists():
        raise Stop(f"{final} exists without SUMMARY.json; inspect and remove it before regenerating")
    if partial.exists():
        shutil.rmtree(partial)  # an interrupted earlier attempt; the WordPress cache makes the retry cheap
    say("[generate] running the manifest generator (cached WordPress reads, <= 2 req/s)")
    code, report, _ = run(cmd, plan.dir / "logs", "generate")
    if code != 0:
        raise Stop(f"[generate] exit {code}; see {plan.dir / 'logs' / 'generate.log'}")
    os.replace(partial, final)
    summary = json.loads((final / "SUMMARY.json").read_text())
    state.finish_stage("generate", manifests=manifest_list(plan),
                       counts=summary.get("counts"), skippedPosts=summary.get("skippedPosts"), comments=summary.get("comments", {}).get("fetched"))
    say(f"[generate] ok: {json.dumps(summary.get('counts'), sort_keys=True)}")


def manifest_list(plan):
    out = []
    for path in [plan.manifests / "foundation" / "manifest.json"] + sorted(plan.manifests.glob("articles-*/manifest.json")):
        m = json.loads(path.read_text())
        out.append({"label": path.parent.name, "path": str(path), "manifestHash": m["manifestHash"]})
    return out


def registered(directory, manifests):
    """Read-only check that each manifest is present (same hash and identical JSON) in a local
    checkout of headless-strapi/config/ds-migration-manifests."""
    found = {}
    for f in sorted(Path(directory).glob("*.json")):
        try:
            m = json.loads(f.read_text())
        except ValueError:
            continue
        if isinstance(m, dict) and isinstance(m.get("manifestHash"), str):
            found[m["manifestHash"]] = m
    missing = []
    for entry in manifests:
        ours = json.loads(Path(entry["path"]).read_text())
        if found.get(entry["manifestHash"]) != ours:
            missing.append(entry["label"])
    return missing


def stage_register(state, plan, args, dry_run):
    if not plan.generated():
        raise Stop("register: no generated manifests yet; run the generate stage first")
    manifests = manifest_list(plan)
    recorded = state.data["stages"].get("generate", {}).get("manifests")
    if recorded and [m["manifestHash"] for m in recorded] != [m["manifestHash"] for m in manifests]:
        raise Stop("register: generated manifests changed since the generate stage recorded them")
    if args.registered_manifest_dir:
        missing = registered(args.registered_manifest_dir, manifests)
        if missing:
            raise Stop("register: not found verbatim in --registered-manifest-dir: " + ", ".join(missing))
    if not args.continue_:
        say("\nSTOP: register the generated manifests before any CMS import.")
        say("The CMS accepts only registered manifests. Commit each file below VERBATIM to")
        say("headless-strapi/config/ds-migration-manifests/ through a reviewed pull request and deploy it.")
        say("This script never registers manifests itself.\n")
        for m in manifests:
            say(f"  {m['path']}")
            say(f"      manifestHash {m['manifestHash']}")
            say(f"      suggested name: ds-{plan.prefix}-{m['label']}-{m['manifestHash'][:12]}.json")
        say("\nAfter the deploy, re-run the same command with --continue "
            "(optionally --registered-manifest-dir <headless-strapi>/config/ds-migration-manifests to verify).")
        return False
    if dry_run:
        say("# register: --continue given; the gate would be marked passed")
        return True
    state.finish_stage("register", manifests=manifests, verifiedAgainst=str(args.registered_manifest_dir) if args.registered_manifest_dir else None)
    say(f"[register] gate passed for {len(manifests)} manifests")
    return True


def selected_stages(args):
    if args.only and args.from_stage:
        raise Stop("--only and --from are mutually exclusive")
    if args.only:
        names = [s.strip() for s in args.only.split(",") if s.strip()]
        bad = [s for s in names if s not in STAGES]
        if bad or not names:
            raise Stop("unknown stage(s): " + ", ".join(bad) + "; stages: " + ", ".join(STAGES))
        return [s for s in STAGES if s in names]
    if args.from_stage:
        if args.from_stage not in STAGES:
            raise Stop("unknown --from stage; stages: " + ", ".join(STAGES))
        return list(STAGES[STAGES.index(args.from_stage):])
    return list(STAGES)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--state-dir", type=Path, required=True, help="operator state directory outside the repository (checkpoints, manifests, logs)")
    ap.add_argument("--config", help="importer config JSON (target, targetTokenEnv, wordpress, ...)")
    ap.add_argument("--count", type=int, help="newest published posts to migrate (generator --count)")
    ap.add_argument("--batch-size", type=int, help="articles per article manifest/run (<= 1000)")
    ap.add_argument("--foundation-scope", choices=("all", "referenced"), help="default all (old Pre-phase parity)")
    ap.add_argument("--comments-approval", help="comments approval label (required unless --no-comments)")
    ap.add_argument("--publication-approval", help="publication approval label (publish + release comments)")
    ap.add_argument("--fallback-user-id", type=int, help="explicit native reader for unmapped/anonymous commenters; no default")
    ap.add_argument("--comment-user-mapping", action="append", help="reader checkpoint mapping for comments (repeatable); never use admin-user IDs")
    ap.add_argument("--no-fallback-user", action="store_true", help="drop unmapped/anonymous comments instead of using a fallback user")
    ap.add_argument("--no-comments", action="store_true", help="exclude comments entirely")
    ap.add_argument("--run-prefix", help="importer --run-id prefix (default ds-full)")
    ap.add_argument("--users-file", help="users JSON for the users phase (default data/source/users/users.json)")
    ap.add_argument("--admin-users-file", help="admin users JSON (default data/source/users/admin_users.json)")
    ap.add_argument("--wp-api", help="generator --wp-api (default https://daily.squirt.org/wp-json/wp/v2/)")
    ap.add_argument("--account-batch-size", type=int, help="importer --batch-size for the account phases")
    ap.add_argument("--cache-dir", type=Path, help="generator WordPress cache (default <state-dir>/wp-cache)")
    ap.add_argument("--download-workers", type=int, default=4, help="generator image download workers (1..8)")
    ap.add_argument("--wp-per-page", type=int, default=50, help="importer --wp-per-page (1..100)")
    ap.add_argument("--media-concurrency", type=int, help="importer --media-concurrency for imports (1..8)")
    ap.add_argument("--inventory-skip-images", action="store_true", help="inventory without downloading media (--skip-images)")
    ap.add_argument("--binary", type=Path, help="importer binary (default target/release/migration-system, else target/debug)")
    ap.add_argument("--generator", type=Path, default=GENERATOR, help=argparse.SUPPRESS)
    ap.add_argument("--only", help="comma-separated stages to run (completed ones are still skipped)")
    ap.add_argument("--from", dest="from_stage", help="run this stage and every later one")
    ap.add_argument("--continue", dest="continue_", action="store_true", help="manifests are registered and deployed: pass the register gate")
    ap.add_argument("--registered-manifest-dir", type=Path, help="local headless-strapi/config/ds-migration-manifests to verify registration (read-only)")
    ap.add_argument("--allow-account-failures", action="store_true",
                    help="continue when some accounts fail (old behaviour); their comments fall back to the fallback user")
    ap.add_argument("--dry-run", action="store_true", help="print the exact commands of every pending stage; execute and write nothing")
    args = ap.parse_args(argv)
    try:
        return orchestrate(args)
    except Stop as e:
        print(f"error: {e}", file=sys.stderr)
        return 1


def orchestrate(args):
    state_dir = external(args.state_dir, "--state-dir")
    if args.binary is None:
        release, debug = REPO / "target/release/migration-system", REPO / "target/debug/migration-system"
        args.binary = release if release.exists() or not debug.exists() else debug
    stages = selected_stages(args)
    if not args.dry_run:
        state_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
        lock = open(state_dir / "orchestrator.lock", "a")
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            raise Stop("another orchestrator run holds this state directory")
    state = State(state_dir, args.dry_run)
    settings = resolve_settings(args, state)
    if not args.dry_run:
        state.data["settings"] = settings
        state.save()
    plan = Plan(args, settings, state_dir)
    if not args.dry_run:
        plan.checkpoints.mkdir(mode=0o700, exist_ok=True)  # the importer needs an existing checkpoint parent
    needs_binary = any(s not in ("generate", "register") for s in stages if not state.done(s))
    if not args.dry_run and needs_binary and not Path(args.binary).exists():
        raise Stop(f"importer binary {args.binary} not found; build it with: cargo build --release --offline --locked")
    if args.dry_run:
        say(f"# dry run: commands of pending stages ({', '.join(stages)}); nothing is executed or written")
    for stage in stages:
        if state.done(stage):
            say(f"[{stage}] already completed; skipped")
            continue
        if stage in POST_GATE and not state.done("register") and "register" not in stages and not args.dry_run:
            raise Stop(f"{stage}: the register gate has not been passed; re-run with --only register --continue (or --from register --continue)")
        if args.dry_run:
            say(f"\n# stage {stage}")
        if stage in ("admin-users", "users"):
            if not args.dry_run:
                token_check(settings["config"])
            unit = plan.account_unit(stage)
            if not state.unit_done(stage, unit.name):
                execute_unit(state, plan, unit, args.dry_run)
            if not args.dry_run:
                state.finish_stage(stage)
        elif stage == "generate":
            if not args.dry_run:
                for prior in ("admin-users", "users"):
                    if not settings["no_comments"] and not state.done(prior):
                        raise Stop(f"generate: the {prior} stage is not complete (its mapping feeds comment authors)")
            stage_generate(state, plan, args.dry_run)
        elif stage == "register":
            if not plan.generated():
                if args.dry_run:
                    say("# register gate: lists the generated manifests and stops until re-run with --continue")
                    continue
            if not stage_register(state, plan, args, args.dry_run):
                if not args.dry_run:
                    return 0
                say("# (dry run continues past the gate to show the later commands)")
        else:
            if not plan.generated():
                if args.dry_run:
                    say(f"# {stage}: commands depend on the generated manifests ({plan.manifests}); per unit, e.g.")
                    say(f"#   {shlex.join(plan.binary())} --config {shlex.quote(settings['config'])} --manifest <manifests>/<dir>/manifest.json "
                        f"--phase <phase> --run-id {plan.prefix}-<unit> --source-ids <<=1000 ids> --types <type> --checkpoint {plan.checkpoints}/<unit> [--resume]")
                    continue
                raise Stop(f"{stage}: no generated manifests; run the generate stage first")
            if stage != "inventory" and not args.dry_run:
                token_check(settings["config"])
            for unit in plan.content_units(stage):
                if state.unit_done(stage, unit.name):
                    continue
                execute_unit(state, plan, unit, args.dry_run)
            if not args.dry_run:
                state.finish_stage(stage)
    if not args.dry_run:
        say("[done] selected stages completed" + ("" if state.done("release-comments") else "; later stages remain"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
