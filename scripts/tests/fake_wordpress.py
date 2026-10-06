"""Offline WordPress REST double for the manifest generator tests (no network, synthetic data only)."""
import contextlib
import importlib.util
import os
import json
import urllib.parse
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
ORIGIN = "https://daily.squirt.org"


def load_generator():
    spec = importlib.util.spec_from_file_location("generate_wordpress_manifest", SCRIPTS / "generate_wordpress_manifest.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def upload(name):
    return f"{ORIGIN}/wp-content/uploads/2026/01/{name}"


def post(pid, category, author, featured, inline=None, tags=()):
    body = f"<p>Synthetic body {pid}.</p>"
    if inline:
        body += f'<figure><img class="wp-image-{inline}" src="{upload(f"img-{inline}.png")}" alt=""></figure>'
    return {"id": pid, "status": "publish", "categories": [category], "author": author, "featured_media": featured,
            "title": {"rendered": f"Synthetic title {pid}"}, "content": {"rendered": body, "protected": False},
            "excerpt": {"rendered": f"<p>Excerpt {pid}</p>", "protected": False},
            "date_gmt": "2026-01-02T03:04:05", "modified_gmt": "2026-01-03T03:04:05", "slug": f"synthetic-{pid}",
            "link": f"{ORIGIN}/synthetic-{pid}/", "tags": list(tags), "comment_status": "open"}


def comment(cid, pid, author, text):
    return {"id": cid, "post": pid, "parent": 0, "author": author, "author_name": "x", "date_gmt": "2026-01-04T05:06:07",
            "content": {"rendered": f"<p>{text}</p>\n"}, "status": "approved", "type": "comment"}


POSTS = [
    # 44 -> porn-and-play/latest-studio-releases; tags: performer 6881, studio 3583
    post(101, 44, 7, 501, inline=502, tags=(6881, 3583)),
    # 3748 -> porn-and-play/erotic-stories
    post(102, 3748, 8, 503),
    # 9999 -> gated-cat/gated-sub (only in the gated test taxonomy); migrated with a public cover
    post(104, 9999, 7, 503),
]
COMMENTS = {
    101: [comment(9001, 101, 55, "mapped regular user"), comment(9002, 101, 0, "anonymous"),
          comment(9003, 101, 77, "mapped admin user"), comment(9004, 101, 66, "x" * 5001), comment(9005, 101, 66, "y" * 1667)],
    102: [],
    103: [comment(9101, 103, 12345, "unmapped user on an existing article")],
    104: [comment(9201, 104, 55, "comment on a gated article")],
}


class FakeClient:
    """Stands in for generate_wordpress_manifest.Client."""
    calls = []

    def __init__(self, cache, refresh_listing):
        self.requests = 0
        self.cache_hits = 0

    def json(self, url, listing=False):
        self.requests += 1
        FakeClient.calls.append(url)
        parsed = urllib.parse.urlsplit(url)
        query = dict(urllib.parse.parse_qsl(parsed.query))
        if parsed.path.endswith("/posts"):
            return {"x-wp-totalpages": "1", "x-wp-total": str(len(POSTS))}, json.loads(json.dumps(POSTS))
        if parsed.path.endswith("/media"):
            ids = [int(i) for i in query["include"].split(",")]
            return {}, [{"id": i, "source_url": upload(f"img-{i}.png"), "mime_type": "image/png", "alt_text": "",
                         "media_details": {"sizes": {}}} for i in ids if i in (501, 502, 503)]
        if parsed.path.endswith("/comments"):
            rows = COMMENTS.get(int(query["post"]), [])
            return {"x-wp-totalpages": "1" if rows else "0"}, json.loads(json.dumps(rows))
        raise AssertionError(f"unexpected request {url}")

    def media(self, url):
        self.requests += 1
        return b"\x89PNG\r\n\x1a\n" + url.encode(), "image/png"


def run_generator(argv, gen=None):
    gen = gen or load_generator()
    gen.Client = FakeClient
    with open(os.devnull, "w") as sink, contextlib.redirect_stdout(sink), contextlib.redirect_stderr(sink):
        gen.main(argv)
    return gen


def write_inputs(tmp):
    """Synthetic author list and users-state mappings (no real personal data)."""
    tmp = Path(tmp)
    (tmp / "authors.json").write_text(json.dumps([{"wpId": 7, "username": "Editor Seven", "email": "", "slug": "e7"},
                                                  {"wpId": 8, "username": "Editor Eight"}, {"wpId": 9, "username": "Unused Editor"},
                                                  {"wpId": 10, "username": "  "}]))
    (tmp / "users-state.json").write_text(json.dumps({"version": 1, "mapping": {"55": 900}}))
    (tmp / "admin-state.json").write_text(json.dumps({"version": 1, "mapping": {"55": 1, "77": 401}}))
    return tmp


def write_gated_taxonomy(tmp, source):
    """The real taxonomy plus a gated category/subCategory (WordPress category 9999)."""
    data = json.loads(Path(source).read_text())
    data["gated-cat"] = {"WPCategoryId": 9998, "dsName": "Gated", "dsSlug": "gated-cat", "accessLevel": "Gated",
                         "SubCategories": {"gated-sub": {"WPCategoryId": 9999, "dsName": "Gated Sub", "dsSlug": "gated-sub", "accessLevel": "Gated"}}}
    path = Path(tmp) / "gated-categories.json"
    path.write_text(json.dumps(data))
    return path
