#!/usr/bin/env python3
"""
Retry failed comments from data/incomplete/failed-comments.json.

Resolution sources (read-only):
  - data/mappings/users/users.json       : wpUserId -> strapiUserId
  - data/mappings/users/admin-users.json : wpUserId -> strapiUserId
  - logs/migration-*.jsonl               : wpPostId -> strapiDocId
                                           (built by scanning article_post_success events)

POSTs each retryable comment to /migration/add-comment using MIGRATION_API_TOKEN.

On success, removes the entry from the in-memory failed list. At the end, rewrites
failed-comments.json with only the comments that still couldn't be posted.

Usage:
  STRAPI_BASE_URL=http://localhost:1337 \
  MIGRATION_API_TOKEN=<token> \
    python3 scripts/retry_failed_comments.py [--dry-run] [--limit N]
"""

import argparse
import datetime
import glob
import json
import os
import sys
import time
import urllib.error
import urllib.request
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
USERS_PATH = ROOT / "data/mappings/users/users.json"
ADMINS_PATH = ROOT / "data/mappings/users/admin-users.json"
FAILED_PATH = ROOT / "data/incomplete/failed-comments.json"
LOG_GLOB = str(ROOT / "logs/migration-*.jsonl")


def load_json(path):
    with open(path) as f:
        return json.load(f)


def build_post_map():
    """Scan structured logs for article_post_success events -> {wpPostId: strapiDocId}."""
    mapping = {}
    for log_path in glob.glob(LOG_GLOB):
        with open(log_path) as f:
            for line in f:
                if '"operation":"article_post_success"' not in line:
                    continue
                try:
                    d = json.loads(line)
                except Exception:
                    continue
                wp = d.get("wpPostId")
                doc = d.get("strapiDocId")
                if wp and doc:
                    # last write wins (most recent run)
                    mapping[int(wp)] = doc
    return mapping


def post_comment(base_url, token, article_doc_id, user_id, comment_text, commented_at):
    url = f"{base_url.rstrip('/')}/migration/add-comment"
    payload = {
        "comment": comment_text,
        "user": user_id,
        "article": article_doc_id,
    }
    if commented_at:
        payload["commentedAt"] = commented_at
    body = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=body,
        method="POST",
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return (r.status, r.read().decode("utf-8", errors="replace"))
    except urllib.error.HTTPError as e:
        return (e.code, e.read().decode("utf-8", errors="replace"))
    except Exception as e:
        return (0, str(e))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true", help="Resolve everything but don't POST.")
    ap.add_argument("--limit", type=int, default=0, help="Cap how many comments to attempt (0 = all).")
    args = ap.parse_args()

    base_url = os.environ.get("STRAPI_BASE_URL", "http://localhost:1337")
    token = os.environ.get("MIGRATION_API_TOKEN", "")
    if not token:
        print("ERROR: set MIGRATION_API_TOKEN env var", file=sys.stderr)
        sys.exit(2)

    print(f"Strapi base URL: {base_url}")
    print(f"Loading mappings...")

    users = load_json(USERS_PATH)
    admins = load_json(ADMINS_PATH) if ADMINS_PATH.exists() else {}
    post_map = build_post_map()
    cache = load_json(FAILED_PATH)
    failed_list = cache.get("failedComments", [])

    print(f"  users mapping:   {len(users)} entries")
    print(f"  admins mapping:  {len(admins)} entries")
    print(f"  post -> docId:   {len(post_map)} entries (from structured logs)")
    print(f"  failed comments: {len(failed_list)} entries")
    print()

    if args.limit:
        failed_list_view = failed_list[: args.limit]
        print(f"--limit {args.limit}: attempting first {len(failed_list_view)} of {len(failed_list)}")
    else:
        failed_list_view = failed_list

    ok = 0
    still_failed = []
    skip_reasons = Counter()
    error_status_counter = Counter()

    start = time.time()
    for i, c in enumerate(failed_list_view):
        author = c.get("authorId")
        wp_post = c.get("wpPostId")
        text = c.get("commentContent") or ""
        when = c.get("commentedAt") or c.get("Commented_At")

        # Resolve article
        doc = post_map.get(int(wp_post)) if wp_post else None
        if not doc:
            skip_reasons["no_article_mapping"] += 1
            c["_retry_skip"] = "no_article_mapping"
            still_failed.append(c)
            continue

        # Resolve user
        user_id = users.get(str(author)) or admins.get(str(author))
        if not user_id:
            skip_reasons["no_user_mapping"] += 1
            c["_retry_skip"] = "no_user_mapping"
            still_failed.append(c)
            continue

        if args.dry_run:
            ok += 1
            continue

        status, body = post_comment(base_url, token, doc, int(user_id), text, when)
        if 200 <= status < 300:
            ok += 1
        else:
            error_status_counter[status] += 1
            c["_retry_status"] = status
            c["_retry_body_snippet"] = body[:200]
            still_failed.append(c)

        if (i + 1) % 100 == 0:
            elapsed = time.time() - start
            rate = (i + 1) / elapsed
            remaining = len(failed_list_view) - (i + 1)
            eta = remaining / rate if rate > 0 else 0
            print(
                f"  progress: {i+1}/{len(failed_list_view)}  ok={ok}  "
                f"rate={rate:.0f}/s  eta={int(eta)}s"
            )

    elapsed = time.time() - start
    print()
    print(f"=== DONE in {elapsed:.0f}s ===")
    print(f"Attempted:       {len(failed_list_view)}")
    print(f"Posted OK:       {ok}")
    print(f"Skipped/failed:  {len(still_failed)}")
    if skip_reasons:
        print("Skip reasons:")
        for r, n in skip_reasons.most_common():
            print(f"  {r:25}: {n}")
    if error_status_counter:
        print("HTTP error statuses:")
        for s, n in error_status_counter.most_common():
            print(f"  HTTP {s}: {n}")

    if args.dry_run:
        print("\n(dry-run: cache file not modified)")
        return

    # Rewrite cache with only the still-failed entries (plus any beyond --limit).
    if args.limit:
        carry_over = failed_list[args.limit :]
    else:
        carry_over = []
    cache["failedComments"] = still_failed + carry_over
    cache["totalFailedComments"] = len(cache["failedComments"])
    cache["lastUpdated"] = datetime.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")
    with open(FAILED_PATH, "w") as f:
        json.dump(cache, f, indent=2)
    print(
        f"\nCache rewritten: {FAILED_PATH.name} now has "
        f"{cache['totalFailedComments']} entries (was {len(failed_list)})"
    )


if __name__ == "__main__":
    main()
