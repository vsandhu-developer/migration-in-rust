#!/usr/bin/env python3
"""Build approvable Daily Squirt manifests from the live public WordPress REST API.

Outputs (under a NEW --out-dir outside this repository):
  foundation/        manifest.json + category/subCategory/author/performer/studio exports and
                     selection.json (ordered per-type run chunks of <= 1,000 source IDs).
                     --foundation-scope all (default, old Pre/Authors parity): every record of the
                     source files; referenced: only records the selected posts use
  articles-NN/       manifest.json + article.json + media.json (+ comment.json), --batch-size newest posts each
  SUMMARY.json       counts, media classification/bytes, skipped posts with reasons

Comments-for-existing-articles mode (--post-ids / --post-ids-file, no --count):
  comments-NN/       comments-only manifest.json + comment.json + comment-source-ids.txt for posts
                     whose articles already exist on the target (dependencies.article)

Formats follow src/manifest.rs, src/source.rs (snapshot checksum) and the CMS
validator (ds-migration-manifest.js: manifestDigest = sha256 of JSON with keys
sorted recursively, excluding manifestHash).

Network policy: public endpoints only, no auth, sequential, <= 2 requests/second,
per_page=100. Raw responses and image bytes are cached under --cache-dir (outside
the repository) and reused on later runs. Personal data from the local exports is
never printed; author records carry only the WordPress username that the old
importer used as the author name.
"""
import argparse
import csv
import datetime as dt
import hashlib
import html
import html.parser
import json
import re
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
DEFAULT_API = "https://daily.squirt.org/wp-json/wp/v2/"
SOURCE_SYSTEM = "wordpress"
ALLOWED_MIME = ("image/png", "image/jpeg", "image/webp", "image/avif")
MAX_MEDIA_BYTES = 20 * 1024 * 1024
SLUG_RE = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
USER_AGENT = "PTP-DailySquirt-migration-manifest/1.0 (+public REST read; contact: platform team)"
MIN_INTERVAL = 0.5  # seconds between WordPress REST requests (<= 2 req/s, sequential)
MAX_DOWNLOAD_WORKERS = 8


# ---------------------------------------------------------------- canonical JSON
def _order(k):
    # ECMAScript enumerates canonical array-index keys first, ascending.
    numeric = k.isdigit() and str(int(k)) == k and int(k) < 4294967295
    return (0, int(k), "") if numeric else (1, 0, k.encode("utf-16-be"))


def encode(value):
    if isinstance(value, dict):
        return {k: encode(value[k]) for k in sorted(value, key=_order)}
    if isinstance(value, list):
        return [encode(v) for v in value]
    if isinstance(value, float):
        raise ValueError("floats are not emitted (ECMAScript number formatting)")
    return value


def canonical(value):
    return json.dumps(encode(value), ensure_ascii=False, separators=(",", ":")).encode()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def source_key(source_id):
    return json.dumps([SOURCE_SYSTEM, source_id], ensure_ascii=False, separators=(",", ":"))


def ref(source_id):
    return {"sourceKey": source_key(source_id)}


def with_publication(manifest, approval):
    """Explicit publish/unpublish approval for exactly the manifest's own selected records (CMS
    migration_publication_invalid rules: source keys of manifest.types, comments only under releaseComments)."""
    if not approval:
        return manifest
    keys = {t: [source_key(i) for i in ids] for t, ids in manifest["types"].items() if t != "comment" and ids}
    publication = {"approval": approval}
    if keys:  # a comments-only manifest has nothing to publish, only comments to release
        publication.update(publish=keys, unpublish={t: list(v) for t, v in keys.items()})
    if manifest["types"].get("comment"):
        publication["releaseComments"] = {"comment": [source_key(i) for i in manifest["types"]["comment"]]}
    manifest["publication"] = publication
    return manifest


def with_hash(manifest):
    content = {k: v for k, v in manifest.items() if k != "manifestHash"}
    manifest["manifestHash"] = sha256(canonical(content))
    return manifest


# ---------------------------------------------------------------- WordPress HTML
def stable_wordpress_html(value):
    """Port of source.rs stable_wordpress_html: request-scoped gallery ordinals,
    [video]/[audio] IE9 shims, player instance ids and ?_=N cache-busters."""
    value = stable_gallery_html(value)
    value = re.sub(r"<!--\[if lt IE 9\]><script>document\.createElement\('video'\);</script><!\[endif\]-->\n?", "", value)
    value = re.sub(r"<!--\[if lt IE 9\]><script>document\.createElement\('audio'\);</script><!\[endif\]-->\n?", "", value)
    value = re.sub(r'(id="(?:video|audio)-[0-9]+-)[0-9]+(?=")', r"\1instance", value)
    return re.sub(r"(?:\?|&|&#038;|&amp;)_=[0-9]+(?=[\"'])", "", value)


def stable_gallery_html(value):
    prefix, out, rem = "wp-block-gallery-", [], value
    while True:
        pos = rem.find(prefix)
        if pos < 0:
            break
        out.append(rem[:pos])
        after = rem[pos + len(prefix):]
        digits = len(after) - len(after.lstrip("0123456789"))
        boundary = digits == len(after) or after[digits] in " \t\n\x0c\r\"'"
        if digits > 0 and boundary:
            out.append("wp-block-gallery-instance")
            rem = after[digits:]
        else:
            out.append(prefix)
            rem = after
    out.append(rem)
    return "".join(out)


def snapshot_checksum(post):
    content = dict(post["content"])
    content["rendered"] = stable_wordpress_html(post["content"]["rendered"])
    snap = {k: post[k] for k in ("id", "title", "excerpt", "date_gmt", "modified_gmt", "status", "categories", "author", "slug", "link")}
    snap["content"] = content
    return sha256(canonical(snap))


def sanitize_slug(raw, wp_id):
    """Exact port of the old ArticleTransformer::sanitize_slug (also in source.rs)."""
    out, last_dash = [], False
    for ch in raw:
        if (ch.isascii() and ch.isalnum()) or ch in "-_.~":
            out.append(ch)
            last_dash = ch == "-"
        elif not last_dash:
            out.append("-")
            last_dash = True
    trimmed = "".join(out).strip("-")
    return trimmed or f"post-{wp_id}"


CTA_CLASSES = {"wp-block-button__link", "has-white-color", "has-text-color", "has-background"}


def count_cta_links(body):
    """Approximate: anchors carrying all four old CTA classes. The importer's
    content_parser decides exactly which top-level ones become registrationCtas."""
    return sum(1 for m in re.finditer(r'<a\b[^>]*class="([^"]*)"', body) if CTA_CLASSES <= set(m.group(1).split()))


def gmt_millis(value):
    return dt.datetime.fromisoformat(value.rstrip("Z")).strftime("%Y-%m-%dT%H:%M:%S.000Z")


def plain(text):
    return re.sub(r"\s+", " ", html.unescape(re.sub(r"<[^>]*>", " ", text or ""))).strip()


class Sources(html.parser.HTMLParser):
    """Collects every attribute the importer's DOM rewrite requires to be mapped."""

    REQUIRED = ("src", "poster", "data-src", "data-lazy-src")

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.required, self.frames, self.problems, self.classes, self.videos = [], [], [], [], []

    def handle_starttag(self, tag, attrs):
        for name, value in attrs:
            if value is None:
                continue
            if tag == "iframe" and name == "src":
                self.frames.append(value)
            elif tag in ("video", "source") and name == "src":
                # Passed through unchanged by the importer; never rehosted.
                self.videos.append(value)
            elif name == "srcset":
                for candidate in value.split(","):
                    parts = candidate.split()
                    if not parts or len(parts) > 2:
                        self.problems.append("srcset_unparseable")
                    else:
                        self.required.append((tag, parts[0]))
            elif name in self.REQUIRED:
                self.required.append((tag, value))
            if tag == "img" and name == "class":
                for m in re.finditer(r"\bwp-image-(\d+)\b", value):
                    self.classes.append(int(m.group(1)))

    handle_startendtag = handle_starttag


def rust_url(value):
    """Approximates url::Url::as_str() normalisation for approved iframe sources."""
    u = urllib.parse.urlsplit(value)
    netloc = (u.hostname or "").lower()
    if u.port and not (u.scheme == "https" and u.port == 443):
        netloc += f":{u.port}"
    return urllib.parse.urlunsplit((u.scheme.lower(), netloc, u.path or "/", u.query, u.fragment))


def sniff(data):
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return "image/png"
    if data.startswith(b"\xff\xd8\xff"):
        return "image/jpeg"
    if len(data) >= 12 and data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    if len(data) >= 16 and data[4:8] == b"ftyp" and any(data[8 + i:12 + i] in (b"avif", b"avis") for i in range(0, 32, 4)):
        return "image/avif"
    return None


# ---------------------------------------------------------------- polite cached HTTP
class Client:
    def __init__(self, cache, refresh_listing):
        self.cache = cache
        self.refresh_listing = refresh_listing
        self.last = 0.0
        self.requests = 0
        self.cache_hits = 0
        self.lock = threading.Lock()

    def _count(self, field):
        with self.lock:
            setattr(self, field, getattr(self, field) + 1)

    def _get(self, url, accept, throttle=True):
        for attempt in range(5):
            if throttle:
                wait = self.last + MIN_INTERVAL - time.monotonic()
                if wait > 0:
                    time.sleep(wait)
                self.last = time.monotonic()
            self._count("requests")
            req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT, "Accept": accept})
            try:
                with urllib.request.urlopen(req, timeout=60) as r:
                    body = r.read(MAX_MEDIA_BYTES + 1)
                    return r.status, {k.lower(): v for k, v in r.headers.items()}, body, r.geturl()
            except urllib.error.HTTPError as e:
                if e.code in (429, 500, 502, 503, 504) and attempt < 4:
                    retry = e.headers.get("Retry-After", "")
                    time.sleep(min(int(retry), 60) if retry.isdigit() else 2 ** attempt)
                    continue
                return e.code, {k.lower(): v for k, v in e.headers.items()}, b"", url
            except (urllib.error.URLError, TimeoutError, ConnectionError):
                if attempt < 4:
                    time.sleep(2 ** attempt)
                    continue
                return 0, {}, b"", url
        return 0, {}, b"", url

    def json(self, url, listing=False):
        path = self.cache / "wp" / f"{sha256(url.encode())}.json"
        if path.exists() and not (listing and self.refresh_listing):
            self._count("cache_hits")
            saved = json.loads(path.read_text())
            return saved["headers"], saved["body"]
        status, headers, body, _ = self._get(url, "application/json")
        if status != 200 or not headers.get("content-type", "").startswith("application/json"):
            raise RuntimeError(f"WordPress request failed: HTTP {status} {url}")
        keep = {k: headers[k] for k in ("x-wp-total", "x-wp-totalpages") if k in headers}
        saved = {"url": url, "fetchedAt": now(), "headers": keep, "body": json.loads(body)}
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(saved, ensure_ascii=False))
        return keep, saved["body"]

    def media(self, url):
        """Returns (bytes, content-type) or raises ValueError(code). Thread-safe:
        called from the bounded download pool; never throttled with REST calls."""
        base = self.cache / "media" / sha256(url.encode())
        meta = base.with_suffix(".json")
        if meta.exists():
            m = json.loads(meta.read_text())
            if m.get("error"):
                raise ValueError(m["error"])
            self._count("cache_hits")
            return base.with_suffix(".bin").read_bytes(), m["contentType"]
        status, headers, body, final = self._get(url, "image/avif,image/webp,image/png,image/jpeg", throttle=False)
        error = None
        ctype = headers.get("content-type", "").split(";")[0].strip()
        if status != 200:
            error = f"media_http_{status}"
        elif final != url:
            error = "media_redirected"
        elif len(body) > MAX_MEDIA_BYTES or not body:
            error = "media_size_invalid"
        elif sniff(body) is None:
            error = "media_type_unsupported"
        elif ctype != sniff(body):
            error = "media_content_type_mismatch"
        meta.parent.mkdir(parents=True, exist_ok=True)
        if error:
            meta.write_text(json.dumps({"url": url, "error": error, "status": status}))
            raise ValueError(error)
        # Bytes first, metadata last: an interrupted write is re-downloaded next run.
        base.with_suffix(".bin").write_bytes(body)
        meta.write_text(json.dumps({"url": url, "contentType": ctype, "size": len(body), "fetchedAt": now()}))
        return body, ctype


def now():
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# ---------------------------------------------------------------- local approved exports
def access(value, where):
    v = str(value).strip().lower()
    if v == "public":
        return "public"
    if v in ("gated", "private", "members", "explicit"):
        return "gated"
    raise SystemExit(f"unknown accessLevel in {where}")


def load_taxonomy(path):
    raw = json.loads(Path(path).read_text())
    categories, subs, wp_map = {}, {}, {}
    for wp_slug, c in raw.items():
        cid = c["dsSlug"]
        if not SLUG_RE.match(cid):
            raise SystemExit(f"category slug not CMS-safe: {cid}")
        # Old load_categories defaulted a missing accessLevel to "Public".
        categories[cid] = {"name": c["dsName"], "slug": cid, "accessLevel": access(c.get("accessLevel", "Public"), "category"), "wpCategoryId": c["WPCategoryId"]}
        for sub_wp_slug, s in c.get("SubCategories", {}).items():
            if not SLUG_RE.match(s["dsSlug"]):
                raise SystemExit(f"subCategory slug not CMS-safe: {cid}/{s['dsSlug']}")
            sid = f"{cid}/{s['dsSlug']}"
            level = access(s.get("accessLevel", "Public"), "subCategory")
            if categories[cid]["accessLevel"] == "gated" and level != "gated":
                raise SystemExit(f"public subCategory under gated category: {sid}")
            prev = subs.get(sid)
            if prev and (prev["name"], prev["accessLevel"]) != (s["dsName"], level):
                raise SystemExit(f"conflicting definitions for subCategory {sid}")
            subs.setdefault(sid, {"name": s["dsName"], "slug": s["dsSlug"], "accessLevel": level, "parent": cid, "wpCategoryIds": []})
            subs[sid]["wpCategoryIds"].append(s["WPCategoryId"])
            if wp_map.get(s["WPCategoryId"], sid) != sid:
                raise SystemExit(f"WordPress category {s['WPCategoryId']} maps to two subCategories")
            wp_map[s["WPCategoryId"]] = sid
    return categories, subs, wp_map


INT_RE = re.compile(r"[+-]?[0-9]+")
MAX_SLUG = 128  # CMS uid validation (ds-validation.js) and content contract limit
MAX_NAME = 255


def utf16_len(text):
    """String length as the CMS (JavaScript) measures it."""
    return len(text.encode("utf-16-le")) // 2


def cms_slug(value):
    """CMS uid fields accept only ^[a-z0-9]+(?:-[a-z0-9]+)*$ (max 128); the old CMS also took '_'."""
    return re.sub(r"[^a-z0-9]+", "-", value.lower()).strip("-")[:MAX_SLUG].rstrip("-")


def load_tags(path, kind):
    """Positional parity with the old importer's load_performers/load_studios: the header row is
    skipped, column 0 is the WordPress tag id (must parse and be > 0), column 1 the trimmed name
    (required), column 2 the trimmed slug (may be empty). Rows failing that were skipped by the old
    loader too. The CMS now validates uid slugs strictly, so a slug is lower-cased/hyphenated, an
    empty one is derived from the name (what the old CMS uid targetField did) and a collision gets
    a "-<id>" suffix. Every adjustment is counted in SUMMARY.json."""
    out, used = {}, set()
    stats = {"rows": 0, "loaded": 0, "skippedInvalidIdOrName": 0, "duplicateIds": 0, "slugNormalised": 0,
             "slugDerivedFromName": 0, "slugDeduplicated": 0, "nameTruncated": 0}
    with open(path, newline="", encoding="utf-8-sig") as f:
        reader = csv.reader(f)
        next(reader, None)
        for row in reader:
            stats["rows"] += 1
            raw_id = row[0].strip() if row else ""
            tag = int(raw_id) if INT_RE.fullmatch(raw_id) else 0
            name = row[1].strip() if len(row) > 1 else ""
            slug = row[2].strip() if len(row) > 2 else ""
            if tag <= 0 or not name:
                stats["skippedInvalidIdOrName"] += 1
                continue
            if tag in out:
                stats["duplicateIds"] += 1  # the old cache kept the last create; keep the first, deterministically
                continue
            if utf16_len(name) > MAX_NAME:
                name = name[:MAX_NAME]
                while utf16_len(name) > MAX_NAME:
                    name = name[:-1]
                stats["nameTruncated"] += 1
            clean = cms_slug(slug)
            if not clean:
                clean = cms_slug(name) or f"{kind}-{tag}"
                stats["slugDerivedFromName"] += 1
            elif clean != slug:
                stats["slugNormalised"] += 1
            if clean in used:
                clean = f"{clean[:MAX_SLUG - len(str(tag)) - 1].rstrip('-')}-{tag}"
                stats["slugDeduplicated"] += 1
            used.add(clean)
            out[tag] = {"name": name, "slug": clean}
            stats["loaded"] += 1
    return out, stats


def load_authors(path):
    """All author.json entries with an integer wpId and a username (old AuthorMigrator sent
    Name=username for every entry). Emails/images are never read into the manifests."""
    out = {}
    for a in json.loads(Path(path).read_text()):
        if isinstance(a, dict) and isinstance(a.get("wpId"), int) and a["wpId"] > 0 and (a.get("username") or "").strip():
            out[a["wpId"]] = {"name": a["username"].strip()[:MAX_NAME], "slug": (a.get("slug") or "").strip()}
    return out


# ---------------------------------------------------------------- media resolution
class MediaIndex:
    def __init__(self, client, api, site_origin):
        self.client, self.api, self.site = client, api, site_origin
        self.attachments = {}  # attachment id -> json
        self.by_url = {}       # absolute url -> media source id
        self.media = {}        # media source id -> {url, aliases:set, mime, size, checksum, error}

    def prefetch(self, ids):
        missing = sorted({i for i in ids if i and i not in self.attachments})
        for start in range(0, len(missing), 100):
            chunk = missing[start:start + 100]
            url = self.api + "media?" + urllib.parse.urlencode({"include": ",".join(map(str, chunk)), "per_page": 100, "_fields": "id,source_url,mime_type,alt_text,media_details"})
            _, rows = self.client.json(url)
            for row in rows:
                self.attachments[row["id"]] = row
                mid = f"wp-attachment-{row['id']}"
                urls = [row.get("source_url")] + [s.get("source_url") for s in (row.get("media_details") or {}).get("sizes", {}).values() if isinstance(s, dict)]
                for u in filter(None, urls):
                    self.by_url.setdefault(u, mid)
            for i in chunk:
                self.attachments.setdefault(i, None)

    def canonical_https(self, absolute):
        u = urllib.parse.urlsplit(absolute)
        if u.scheme == "http":
            return urllib.parse.urlunsplit(("https",) + tuple(u[1:]))
        return absolute

    def resolve(self, raw, base, class_ids):
        """Returns (media id, error). Registers raw as an alias of the media."""
        if raw.startswith("data:"):
            return None, "data_uri_image"
        absolute = urllib.parse.urljoin(base, raw)
        mid = self.by_url.get(absolute) or self.by_url.get(self.canonical_https(absolute))
        if mid is None:
            known = [i for i in class_ids if self.attachments.get(i)]
            if len(known) == 1:
                mid = f"wp-attachment-{known[0]}"
        if mid is None:
            url = self.canonical_https(absolute)
            if urllib.parse.urlsplit(url).scheme != "https":
                return None, "media_url_not_https"
            mid = f"wp-url-{sha256(url.encode())[:40]}"
            self.by_url[url] = mid
            self.media.setdefault(mid, {"url": url, "aliases": set()})
        entry = self.entry(mid)
        if entry.get("error"):
            return None, entry["error"]
        for alias in (raw, absolute):
            if alias != entry["url"]:
                entry["aliases"].add(alias)
        return mid, None

    def entry(self, mid):
        entry = self.media.get(mid)
        if entry is None:
            att = self.attachments[int(mid.rsplit("-", 1)[1])]
            entry = {"url": self.canonical_https(att["source_url"]), "aliases": set()}
            for s in (att.get("media_details") or {}).get("sizes", {}).values():
                if isinstance(s, dict) and s.get("source_url") and s["source_url"] != entry["url"]:
                    entry["aliases"].add(s["source_url"])
            if att["source_url"] != entry["url"]:
                entry["aliases"].add(att["source_url"])
            self.media[mid] = entry
        return entry

    def download(self, mids, workers):
        """Fetches bytes for checksums with a bounded parallel pool (one job per URL)."""
        jobs = {}
        for mid in mids:
            e = self.media[mid]
            if "checksum" not in e and "error" not in e:
                jobs.setdefault(e["url"], []).append(e)

        def fetch(url):
            try:
                data, mime = self.client.media(url)
                if mime not in ALLOWED_MIME:
                    raise ValueError("media_type_unsupported")
                return url, {"checksum": sha256(data), "size": len(data), "mimeType": mime}
            except ValueError as e:
                return url, {"error": str(e)}

        with ThreadPoolExecutor(max_workers=workers) as pool:
            for url, result in pool.map(fetch, sorted(jobs)):
                for e in jobs[url]:
                    e.update(result)

    def error(self, mid):
        return self.media[mid].get("error")


# ---------------------------------------------------------------- main
def external_dir(path, label, must_be_new):
    p = path.resolve()
    lower = str(p).lower()
    if "daily-squirt-code" in lower or "migration-in-rust" in lower or str(p).startswith(str(REPO)):
        raise SystemExit(f"{label} must be outside the repository")
    if must_be_new and p.exists() and any(p.iterdir()):
        raise SystemExit(f"{label} must be a new or empty directory")
    p.mkdir(mode=0o700, parents=True, exist_ok=True)
    return p


def write_export(directory, name, meta, records):
    body = canonical(dict(meta, records=records))
    (directory / name).write_bytes(body)
    return {"path": name, "sha256": sha256(body)}


FOUNDATION_ORDER = ("category", "subCategory", "author", "performer", "studio")
MAX_RUN_IDS = 1000          # CMS migrationRunRequest.sourceIds / importer --source-ids limit
MAX_COMMENT_CHARS = 5000    # ds-comment.comment maxLength for migrated comments (headless-strapi 4b061c2; UTF-16 units)


def selection_chunks(types, order=FOUNDATION_ORDER, limit=MAX_RUN_IDS):
    """Ordered per-type runs of at most `limit` source IDs (dependencies first)."""
    chunks = []
    for t in order:
        ids = types.get(t, [])
        parts = max(1, -(-len(ids) // limit))
        for n in range(parts):
            part = ids[n * limit:(n + 1) * limit]
            if part:
                chunks.append({"index": len(chunks) + 1, "type": t, "part": n + 1, "parts": parts, "count": len(part), "sourceIds": part})
    return chunks


def build_foundation(out, meta, site_origin, taxonomy, authors, performers, studios, used, publication_approval):
    """foundation/: one manifest (registered once) whose selection is imported as ordered per-type
    chunks of <= 1,000 IDs (selection.json). `used` is None for the full (old Pre-phase parity) scope,
    else {category, subCategory, author, performer, studio: ids} for the referenced scope."""
    categories, subs, _ = taxonomy
    if used is None:
        used = {"category": sorted(categories), "subCategory": sorted(subs), "author": sorted(authors),
                "performer": sorted(performers), "studio": sorted(studios)}
    rows = {
        "category": [{"sourceId": c, "sourceUrl": f"{site_origin}/?cat={categories[c]['wpCategoryId']}",
                      "data": {"name": categories[c]["name"], "slug": categories[c]["slug"], "accessLevel": categories[c]["accessLevel"]}} for c in used["category"]],
        "subCategory": [{"sourceId": s, "sourceUrl": f"{site_origin}/?cat={subs[s]['wpCategoryIds'][0]}",
                         "data": {"name": subs[s]["name"], "slug": subs[s]["slug"], "accessLevel": subs[s]["accessLevel"], "category": ref(subs[s]["parent"])}} for s in used["subCategory"]],
        "author": [{"sourceId": str(i), "sourceUrl": f"{site_origin}/?author={i}", "data": {"name": authors[i]["name"]}} for i in used["author"]],
        "performer": [{"sourceId": str(t), "sourceUrl": f"{site_origin}/?tag_id={t}", "data": {"name": performers[t]["name"], "slug": performers[t]["slug"]}} for t in used["performer"]],
        "studio": [{"sourceId": str(t), "sourceUrl": f"{site_origin}/?tag_id={t}", "data": {"name": studios[t]["name"], "slug": studios[t]["slug"]}} for t in used["studio"]],
    }
    rows = {k: v for k, v in rows.items() if v}
    for t, records in rows.items():
        if len(records) > 10000:
            raise SystemExit(f"foundation {t}: more than 10,000 records (CMS manifest type limit)")
    fdir = out / "foundation"
    fdir.mkdir(mode=0o700)
    foundation = dict(meta, sourceSystem=SOURCE_SYSTEM, sourceOrigins=[site_origin], comments={"mode": "excluded"},
                      types={t: [r["sourceId"] for r in records] for t, records in rows.items()}, files={},
                      taxonomy={"category": {c: {"accessLevel": categories[c]["accessLevel"]} for c in used["category"]},
                                "subCategory": {s: {"accessLevel": subs[s]["accessLevel"], "parentSourceKey": source_key(subs[s]["parent"])} for s in used["subCategory"]}})
    for t, records in rows.items():
        foundation["files"][t] = write_export(fdir, f"{t}.json", meta, records)
    with_publication(foundation, publication_approval)
    with_hash(foundation)
    (fdir / "manifest.json").write_bytes(canonical(foundation))
    chunks = selection_chunks(foundation["types"])
    (fdir / "selection.json").write_text(json.dumps({"manifest": "manifest.json", "manifestHash": foundation["manifestHash"],
                                                     "maxSourceIdsPerRun": MAX_RUN_IDS, "order": [c["type"] for c in chunks],
                                                     "chunks": chunks}, indent=1))
    return foundation, chunks


def load_user_map(paths):
    """wpId -> native reader id, first file wins. Admin IDs use a different namespace."""
    user_map = {}
    for path in paths:
        checkpoint = json.loads(Path(path).read_text())
        if checkpoint.get("identity", {}).get("phase") == "admin-users":
            raise ValueError("comment mapping must contain reader-user IDs, not admin-user IDs")
        for wp, uid in checkpoint.get("mapping", {}).items():
            user_map.setdefault(int(wp), int(uid))
    return user_map


def fetch_comments(client, api, post_id):
    """Full paginated /comments?post=ID exactly as the importer re-reads them (source.rs)."""
    rows, page_no, pages = [], 1, 1
    while page_no <= pages:
        url = api + "comments?" + urllib.parse.urlencode({"post": post_id, "per_page": 100, "page": page_no, "orderby": "id", "order": "asc"})
        headers, body = client.json(url)
        pages = int(headers.get("x-wp-totalpages", "0") or 0)
        rows += body
        page_no += 1
    return rows


def map_comments(rows, user_map, fallback_user_id, stats):
    """Old parity: mapped commenters keep their account; unmapped/anonymous ones use the explicit
    fallback (old importer: native user 3); without a fallback they are dropped and counted."""
    mapped = []
    for c in rows:
        stats["fetched"] += 1
        author = c.get("author") or 0
        if author == 0:
            stats["anonymous"] += 1
        if author and author in user_map:
            user, native = f"wp-user-{author}", user_map[author]
            stats["mapped"] += 1
        elif fallback_user_id:
            user, native = f"wp-user-{author}", fallback_user_id
            stats["fallback"] += 1
        else:
            stats["unmapped"] += 1
            continue
        mapped.append({"row": c, "user": user, "native": native})
    return mapped


def comment_record(c, site_origin, post_id):
    row = c["row"]
    return {"sourceId": str(row["id"]), "sourceUrl": f"{site_origin}/?p={post_id}&replytocom={row['id']}",
            "data": {"comment": plain(row["content"]["rendered"]) or "-", "commentedAt": gmt_millis(row["date_gmt"]),
                     "user": {"sourceKey": source_key(c["user"])}, "article": ref(str(post_id))},
            "wordpress": {"postId": str(post_id), "checksum": sha256(canonical(row))}}


def comment_too_long(c):
    return utf16_len(plain(c["row"]["content"]["rendered"])) > MAX_COMMENT_CHARS


def parse_post_ids(a, ap):
    raw = []
    if a.post_ids:
        raw += a.post_ids.split(",")
    if a.post_ids_file:
        raw += re.split(r"[\s,]+", a.post_ids_file.read_text())
    ids = []
    for v in (x.strip() for x in raw):
        if not v:
            continue
        if not v.isdigit() or int(v) < 1:
            ap.error(f"--post-ids: not a WordPress post id: {v[:20]}")
        if int(v) not in ids:
            ids.append(int(v))
    if not ids:
        ap.error("--post-ids/--post-ids-file selected no posts")
    return ids


def comments_for_existing(a, ap, client, api, site_origin, out, meta):
    """Comments-only manifests for articles that ALREADY exist on the target (imported earlier with
    any access level: posts are never fetched or filtered here, so gated/explicit articles keep their
    comments even though the article-import path skips gated subCategories;
    sourceKey ["wordpress","<postId>"]). Each comments-NN/ manifest has types.comment + files.comment,
    comments:{mode:"existing-users",approval,users}, dependencies.article for the referenced posts (the
    CMS and importer resolve the relation through dependencies; the article is not in types) and
    publication.releaseComments when --publication-approval is given."""
    post_ids = parse_post_ids(a, ap)
    user_map = load_user_map(a.user_mapping)
    stats = {"fetched": 0, "mapped": 0, "fallback": 0, "anonymous": 0, "unmapped": 0, "skippedOverLength": 0}
    per_post, over_length = {}, []
    for pid in post_ids:
        rows = fetch_comments(client, api, pid)
        if any(r.get("post") not in (None, pid) for r in rows):
            raise SystemExit(f"post {pid}: comments endpoint returned another post's comments")
        kept = []
        for c in map_comments(rows, user_map, a.fallback_user_id, stats):
            if comment_too_long(c):
                stats["skippedOverLength"] += 1
                over_length.append({"postId": pid, "commentId": c["row"]["id"]})
            else:
                kept.append(c)
        per_post[pid] = kept
        print(json.dumps({"progress": {"post": pid, "comments": len(kept), "requests": client.requests}}), file=sys.stderr)
    manifests, without = [], [pid for pid in post_ids if not per_post[pid]]
    with_comments = [pid for pid in post_ids if per_post[pid]]
    for n, start in enumerate(range(0, len(with_comments), a.batch_size), 1):
        group = with_comments[start:start + a.batch_size]
        comments = [comment_record(c, site_origin, pid) for pid in group for c in per_post[pid]]
        users = {}
        for pid in group:
            for c in per_post[pid]:
                users[source_key(c["user"])] = c["native"]
        if len(comments) > 10000:
            raise SystemExit(f"comments-{n:02d}: more than 10,000 comments; lower --batch-size")
        cdir = out / f"comments-{n:02d}"
        cdir.mkdir(mode=0o700)
        manifest = dict(meta, sourceSystem=SOURCE_SYSTEM, sourceOrigins=[site_origin],
                        types={"comment": [c["sourceId"] for c in comments]}, files={},
                        comments={"mode": "existing-users", "approval": a.comments_approval, "users": users},
                        dependencies={"article": [source_key(str(pid)) for pid in group]})
        manifest["files"]["comment"] = write_export(cdir, "comment.json", meta, comments)
        with_publication(manifest, a.publication_approval)
        with_hash(manifest)
        raw = canonical(manifest)
        if len(raw) > 2 * 1024 * 1024:
            raise SystemExit(f"comments-{n:02d}: manifest exceeds 2 MiB; lower --batch-size")
        (cdir / "manifest.json").write_bytes(raw)
        (cdir / "comment-source-ids.txt").write_text(",".join(manifest["types"]["comment"]) + "\n")
        manifests.append({"dir": cdir.name, "manifestHash": manifest["manifestHash"], "posts": len(group), "comments": len(comments)})
    summary = {"generatedAt": meta["approvedAt"], "mode": "comments-for-existing-articles", "wpApi": api,
               "requestedPosts": len(post_ids), "postsWithComments": len(with_comments), "postsWithoutComments": without,
               "comments": manifests, "commentStats": stats, "skippedOverLength": over_length,
               "network": {"requests": client.requests, "cacheHits": client.cache_hits},
               "notes": ["The referenced articles must already exist on the target (same sourceKey); the CMS rejects a missing one with migration_relation_missing.",
                         "Register each comments-NN/manifest.json verbatim, then import with --types comment and <= 1,000 ids of comment-source-ids.txt per run, then release-comments with the same run/checkpoint."]}
    (out / "SUMMARY.json").write_text(json.dumps(summary, indent=1, ensure_ascii=False))
    print(json.dumps({k: summary[k] for k in ("requestedPosts", "postsWithComments", "comments", "commentStats")}, indent=1))


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--count", type=int, default=None, help="newest published posts to include (e.g. 1000); article mode")
    ap.add_argument("--batch-size", type=int, default=None,
                    help="articles per article manifest (<=1000; required in article mode); posts per comments-NN manifest with --post-ids (default 100)")
    ap.add_argument("--out-dir", type=Path, required=True)
    ap.add_argument("--cache-dir", type=Path, required=True, help="raw response/image cache outside the repo")
    ap.add_argument("--wp-api", default=DEFAULT_API)
    ap.add_argument("--categories", type=Path, default=REPO / "data/source/categories/DS-categories.json")
    ap.add_argument("--performers", type=Path, default=REPO / "data/source/performers/DS-Performers.csv")
    ap.add_argument("--studios", type=Path, default=REPO / "data/source/studios/DS-studio.csv")
    ap.add_argument("--authors", type=Path, default=REPO / "data/source/users/author.json")
    ap.add_argument("--foundation-scope", choices=("all", "referenced"), default="all",
                    help="all (default, old Pre/Authors parity): every category, subCategory, author, performer and studio "
                         "from the source files; referenced: only records the selected posts use")
    ap.add_argument("--post-ids", default=None,
                    help="comments-for-existing-articles mode: comma-separated WordPress post IDs whose articles already exist on the target")
    ap.add_argument("--post-ids-file", type=Path, default=None, help="as --post-ids, IDs separated by commas/whitespace")
    ap.add_argument("--source-owner", default="Pink Triangle Press - Daily Squirt editorial")
    ap.add_argument("--max-scan", type=int, default=0, help="max candidate posts to scan (default count*3+100)")
    ap.add_argument("--refresh-listing", action="store_true", help="re-fetch cached post listing pages")
    ap.add_argument("--user-mapping", type=Path, action="append", default=[],
                    help="users-state.json from the users phase, then admin-users (old lookup order); repeatable")
    ap.add_argument("--fallback-user-id", type=int, default=None,
                    help="native user for unmapped/anonymous commenters (old importer used 3); no default")
    ap.add_argument("--comments-approval", default=None, help="approval reference; required to include comments")
    ap.add_argument("--publication-approval", default=None,
                    help="approval reference; adds publication.publish/unpublish lists for every selected (non-comment) record, "
                         "plus releaseComments when comments are included. Without it the manifests cannot be published.")
    ap.add_argument("--download-workers", type=int, default=4, help=f"parallel image downloads for checksums (1..{MAX_DOWNLOAD_WORKERS}, default 4)")
    a = ap.parse_args(argv)
    comments_mode = bool(a.post_ids or a.post_ids_file)
    if comments_mode:
        if a.count is not None:
            ap.error("--count does not apply with --post-ids/--post-ids-file")
        if not a.comments_approval:
            ap.error("--comments-approval is required with --post-ids/--post-ids-file")
        a.batch_size = a.batch_size or 100
        if not 1 <= a.batch_size <= 1000:
            ap.error("batch-size 1..1000")
    elif a.count is None or a.batch_size is None:
        ap.error("--count and --batch-size are required (or use --post-ids/--post-ids-file)")
    elif not (1 <= a.count <= 20000 and 1 <= a.batch_size <= 1000):
        ap.error("count 1..20000, batch-size 1..1000")
    if (a.user_mapping or a.fallback_user_id) and not a.comments_approval:
        ap.error("--comments-approval is required when comment authors are mapped")
    for label in ("comments_approval", "publication_approval"):
        value = getattr(a, label)
        if value is not None and not (0 < len(value) <= 255 and value.strip() == value):
            ap.error(f"--{label.replace('_', '-')} must be 1..255 characters without surrounding spaces")
    if a.fallback_user_id is not None and a.fallback_user_id < 1:
        ap.error("--fallback-user-id must be a positive native user id")
    if not 1 <= a.download_workers <= MAX_DOWNLOAD_WORKERS:
        ap.error(f"--download-workers must be 1..{MAX_DOWNLOAD_WORKERS}")
    api = a.wp_api if a.wp_api.endswith("/") else a.wp_api + "/"
    site = urllib.parse.urlsplit(api)
    if site.scheme != "https":
        ap.error("--wp-api must be https")
    site_origin = f"https://{site.netloc}"
    out = external_dir(a.out_dir, "--out-dir", True)
    cache = external_dir(a.cache_dir, "--cache-dir", False)
    client = Client(cache, a.refresh_listing)
    generated = now()
    meta = {"schemaVersion": "v1", "repositoryFixture": False, "sourceOwner": a.source_owner, "sourceAuthority": site.netloc,
            "sourceLocation": f"{api} (public REST snapshot {generated})", "approvedAt": generated}
    if comments_mode:
        return comments_for_existing(a, ap, client, api, site_origin, out, meta)

    categories, subs, wp_map = load_taxonomy(a.categories)
    performers, performer_stats = load_tags(a.performers, "performer")
    studios, studio_stats = load_tags(a.studios, "studio")
    authors = load_authors(a.authors)
    index = MediaIndex(client, api, site_origin)

    accepted, skipped, scanned = [], [], 0
    max_scan = a.max_scan or a.count * 3 + 100
    page, total_pages = 1, None
    while len(accepted) < a.count and scanned < max_scan and (total_pages is None or page <= total_pages):
        url = api + "posts?" + urllib.parse.urlencode({"status": "publish", "orderby": "date", "order": "desc", "per_page": 100, "page": page})
        headers, posts = client.json(url, listing=True)
        total_pages = int(headers.get("x-wp-totalpages", "0") or 0)
        attachment_ids = []
        for p in posts:
            parser = Sources()
            parser.feed(p["content"]["rendered"])
            attachment_ids += [p.get("featured_media") or 0] + parser.classes
        index.prefetch(attachment_ids)
        pending = []

        def finalize():
            # Download this chunk's images in parallel, then accept in newest-first order.
            index.download([m for x in pending for m in x["media"]], a.download_workers)
            for x in pending:
                errors = sorted({f"media:{index.error(m)}" for m in x["media"] if index.error(m)})
                if errors:
                    skipped.append({"postId": x["post"]["id"], "reasons": errors})
                elif len(accepted) < a.count:
                    accepted.append(x)
            pending.clear()

        for post in posts:
            if len(accepted) + len(pending) >= a.count or scanned >= max_scan:
                break
            scanned += 1
            if any(x["post"]["id"] == post["id"] for x in accepted + pending):
                continue
            reasons = []
            if post.get("status") != "publish":
                reasons.append("status_not_publish")
            primary = next((c for c in post.get("categories", []) if c in wp_map), None)
            if primary is None:
                reasons.append("category_unmapped:" + ",".join(map(str, post.get("categories", []))))
            if post.get("author") not in authors:
                reasons.append("author_unmapped")
            if not plain(post["title"]["rendered"]):
                reasons.append("title_empty")
            if not post.get("featured_media"):
                reasons.append("no_featured_image")
            elif not index.attachments.get(post["featured_media"]):
                reasons.append("featured_image_missing")
            gated = primary is not None and subs[wp_map[primary]]["accessLevel"] == "gated"
            if gated:
                # The CMS cover contract requires a public cover; no approved public variant exists.
                reasons.append("gated_article_requires_public_cover_decision")
            body = stable_wordpress_html(post["content"]["rendered"])
            parser = Sources()
            parser.feed(body)
            reasons += sorted(set(parser.problems))
            for tag, _ in parser.required:
                if tag in ("script", "audio", "embed", "track", "input"):
                    reasons.append(f"unsupported_embedded_source:{tag}")
            frames = []
            for f in parser.frames:
                if urllib.parse.urlsplit(f).scheme != "https":
                    reasons.append("iframe_not_https")
                else:
                    frames.append(rust_url(f))
            videos = []
            for v in parser.videos:
                u = urllib.parse.urlsplit(v)
                if u.scheme != "https" or not u.hostname:
                    reasons.append("video_source_not_https")
                else:
                    videos.append(f"https://{u.netloc.lower()}")
            if reasons:
                skipped.append({"postId": post["id"], "reasons": sorted(set(reasons))})
                continue
            media_ids, media_errors = [], []
            cover, err = index.resolve(index.attachments[post["featured_media"]]["source_url"], post["link"], [post["featured_media"]])
            if err:
                media_errors.append(f"featured:{err}")
            for _, raw in parser.required:
                mid, err = index.resolve(raw, post["link"], parser.classes)
                if err:
                    media_errors.append(err)
                elif mid not in media_ids:
                    media_ids.append(mid)
            if media_errors:
                skipped.append({"postId": post["id"], "reasons": sorted(set(media_errors))})
                continue
            pending.append({"post": post, "primary": primary, "cover": cover, "media": [cover] + [m for m in media_ids if m != cover], "frames": frames,
                            "videos": videos, "ctaLinks": count_cta_links(body)})
            if len(pending) >= a.count - len(accepted):
                finalize()
        finalize()
        page += 1
        print(json.dumps({"progress": {"page": page - 1, "accepted": len(accepted), "skipped": len(skipped), "requests": client.requests}}), file=sys.stderr)

    if not accepted:
        raise SystemExit("no posts accepted; see skipped reasons")

    # ------------------------------------------------ comments (old importer migrated them)
    # The public API returns approved comments only. Checksum = whole row (source.rs).
    user_map = load_user_map(a.user_mapping)
    include_comments = bool(a.comments_approval)
    comment_stats = {"fetched": 0, "mapped": 0, "fallback": 0, "anonymous": 0, "unmapped": 0, "skippedOverLength": 0}
    over_length = []
    for x in accepted:
        x["comments"] = []
        for c in map_comments(fetch_comments(client, api, x["post"]["id"]), user_map, a.fallback_user_id, comment_stats):
            if comment_too_long(c):
                # ds-comment.comment is max 5,000 characters for migrated comments; the CMS would reject (and fail) the run.
                comment_stats["skippedOverLength"] += 1
                over_length.append({"postId": x["post"]["id"], "commentId": c["row"]["id"]})
            else:
                x["comments"].append(c)

    # ------------------------------------------------ foundation
    if a.foundation_scope == "referenced":
        used_subs = sorted({wp_map[x["primary"]] for x in accepted})
        used = {"category": sorted({subs[s]["parent"] for s in used_subs}), "subCategory": used_subs,
                "author": sorted({x["post"]["author"] for x in accepted}),
                "performer": sorted({t for x in accepted for t in x["post"].get("tags", []) if t in performers}),
                "studio": sorted({t for x in accepted for t in x["post"].get("tags", []) if t in studios})}
    else:
        used = None
    foundation, foundation_chunks = build_foundation(out, meta, site_origin, (categories, subs, wp_map), authors, performers, studios,
                                                     used, a.publication_approval)
    category_map = {str(wp): source_key(sid) for wp, sid in sorted(wp_map.items())}

    # ------------------------------------------------ articles
    manifests, classification, all_media, alias_conflicts = [], {"public": 0, "explicit": 0}, {}, 0
    for n, start in enumerate(range(0, len(accepted), a.batch_size), 1):
        batch = accepted[start:start + a.batch_size]
        adir = out / f"articles-{n:02d}"
        adir.mkdir(mode=0o700)
        media_defs, media_rows, articles, origins, frames = {}, [], [], {site_origin}, []
        comments, comment_users, video_origins = [], {}, {}
        seen_alias = {}
        for x in batch:
            p, sid = x["post"], str(x["post"]["id"])
            level = "explicit" if subs[wp_map[x["primary"]]]["accessLevel"] == "gated" else "public"
            for mid in x["media"]:
                d = media_defs.setdefault(mid, {"sourceKey": source_key(mid), "checksum": index.media[mid]["checksum"], "accessLevel": level,
                                                "mimeType": index.media[mid]["mimeType"], "size": index.media[mid]["size"], "transformVersion": "v1",
                                                "requiredFor": {"article": []}})
                if level == "explicit":
                    d["accessLevel"] = "explicit"
                if sid not in d["requiredFor"]["article"]:
                    d["requiredFor"]["article"].append(sid)
            frames += [f for f in x["frames"] if f not in frames]
            att = index.attachments[p["featured_media"]]
            alt = (att.get("alt_text") or "").strip() or plain(p["title"]["rendered"])
            data = {"title": plain(p["title"]["rendered"])[:255], "slug": sanitize_slug(p["slug"], p["id"]), "excerpt": plain(p["excerpt"]["rendered"])[:300],
                    "publishDate": gmt_millis(p["date_gmt"]), "body": stable_wordpress_html(p["content"]["rendered"]),
                    "coverImage": {"publicImage": ref(x["cover"]), "altText": alt[:255]},
                    "author": ref(str(p["author"])), "subCategory": ref(wp_map[x["primary"]]), "allowComments": p.get("comment_status") == "open"}
            perf = [ref(str(t)) for t in p.get("tags", []) if t in performers]
            stud = [ref(str(t)) for t in p.get("tags", []) if t in studios]
            if perf:
                data["performers"] = perf
            if stud:
                data["studios"] = stud
            for v in x["videos"]:
                video_origins[v] = video_origins.get(v, 0) + 1
            if include_comments:
                for c in x["comments"]:
                    comment_users[source_key(c["user"])] = c["native"]
                    comments.append(comment_record(c, site_origin, p["id"]))
            articles.append({"sourceId": sid, "sourceUrl": p["link"], "data": data,
                             "wordpress": {"status": p["status"], "modifiedGmt": p["modified_gmt"], "primaryCategoryId": x["primary"], "checksum": snapshot_checksum(p)}})
        for mid in media_defs:
            m = index.media[mid]
            aliases = sorted(al for al in m["aliases"] if al != m["url"])
            for al in [m["url"]] + aliases:
                if seen_alias.setdefault(al, mid) != mid:
                    alias_conflicts += 1
            aliases = [al for al in aliases if seen_alias[al] == mid]
            media_rows.append({"sourceId": mid, "url": m["url"], "aliases": aliases})
            origins.add("{0.scheme}://{0.netloc}".format(urllib.parse.urlsplit(m["url"])))
            all_media[mid] = m["size"]
            classification[media_defs[mid]["accessLevel"]] += 1
        manifest = dict(meta, sourceSystem=SOURCE_SYSTEM, sourceOrigins=sorted(origins), comments={"mode": "excluded"},
                        types={"article": [r["sourceId"] for r in articles]}, files={},
                        media=media_defs, wordpressCategoryMap=category_map)
        # Dependencies: exactly the foundation keys this batch references.
        used = {"author": set(), "subCategory": set(), "performer": set(), "studio": set()}
        for r in articles:
            used["author"].add(r["data"]["author"]["sourceKey"])
            used["subCategory"].add(r["data"]["subCategory"]["sourceKey"])
            used["performer"].update(v["sourceKey"] for v in r["data"].get("performers", []))
            used["studio"].update(v["sourceKey"] for v in r["data"].get("studios", []))
        manifest["dependencies"] = {k: sorted(v) for k, v in used.items() if v}
        if frames:
            manifest["frameSources"] = frames
        if len(manifest["sourceOrigins"]) > 20 or len(frames) > 50:
            raise SystemExit(f"articles-{n:02d}: too many origins/frames; lower --batch-size")
        if comments:
            manifest["types"]["comment"] = [c["sourceId"] for c in comments]
            manifest["comments"] = {"mode": "existing-users", "approval": a.comments_approval, "users": comment_users}
            manifest["files"]["comment"] = write_export(adir, "comment.json", meta, comments)
            (adir / "comment-source-ids.txt").write_text(",".join(manifest["types"]["comment"]) + "\n")
        manifest["files"]["article"] = write_export(adir, "article.json", meta, articles)
        manifest["files"]["media"] = write_export(adir, "media.json", meta, media_rows)
        with_publication(manifest, a.publication_approval)
        with_hash(manifest)
        raw = canonical(manifest)
        if len(raw) > 2 * 1024 * 1024:
            raise SystemExit(f"articles-{n:02d}: manifest exceeds 2 MiB; lower --batch-size")
        (adir / "manifest.json").write_bytes(raw)
        (adir / "source-ids.txt").write_text(",".join(manifest["types"]["article"]) + "\n")
        manifests.append({"dir": adir.name, "manifestHash": manifest["manifestHash"], "articles": len(articles), "media": len(media_defs),
                          "mediaBytes": sum(d["size"] for d in media_defs.values()), "frames": len(frames), "sourceOrigins": manifest["sourceOrigins"],
                          "comments": len(comments), "videoOrigins": video_origins})

    slugs = {}
    for x in accepted:
        slugs.setdefault(sanitize_slug(x["post"]["slug"], x["post"]["id"]), []).append(x["post"]["id"])
    video_totals = {}
    for m in manifests:
        for o, n in m["videoOrigins"].items():
            video_totals[o] = video_totals.get(o, 0) + n
    reason_counts = {}
    for sk in skipped:
        for r in sk["reasons"]:
            key = r.split(":")[0] if r.startswith("category_unmapped") else r
            reason_counts[key] = reason_counts.get(key, 0) + 1
    n_acc = len(accepted)
    summary = {
        "generatedAt": generated, "wpApi": api, "requestedCount": a.count, "batchSize": a.batch_size,
        "scannedPosts": scanned, "acceptedPosts": len(accepted), "skippedPosts": len(skipped),
        "foundation": {"scope": a.foundation_scope, "manifestHash": foundation["manifestHash"],
                       "counts": {t: len(v) for t, v in foundation["types"].items()},
                       "selectionIds": sum(len(v) for v in foundation["types"].values()),
                       "runChunks": [{k: c[k] for k in ("index", "type", "part", "parts", "count")} for c in foundation_chunks],
                       "sourceFiles": {"performer": performer_stats, "studio": studio_stats, "authorsLoaded": len(authors),
                                       "categories": len(categories), "subCategories": len(subs)}},
        "articles": manifests,
        "counts": dict({t: len(v) for t, v in foundation["types"].items()}, article=len(accepted)),
        "media": {"unique": len(all_media), "bytes": sum(all_media.values()), "classificationPerManifest": classification,
                  "aliasConflictsDropped": alias_conflicts},
        "coverAltTextFromTitle": sum(1 for x in accepted if not (index.attachments[x["post"]["featured_media"]].get("alt_text") or "").strip()),
        "perPost": {"images": round(len(all_media) / n_acc, 2), "megabytes": round(sum(all_media.values()) / n_acc / 1e6, 3),
                    "comments": round(comment_stats["fetched"] / n_acc, 2),
                    "videoShare": round(sum(1 for x in accepted if x["videos"]) / n_acc, 3),
                    "ctaLinkShare": round(sum(1 for x in accepted if x["ctaLinks"]) / n_acc, 3)},
        "videoOrigins": dict(sorted(video_totals.items(), key=lambda kv: -kv[1])),
        "comments": dict(comment_stats, includedInManifests=include_comments, fallbackUserId=a.fallback_user_id,
                         skippedOverLengthIds=over_length),
        "slugs": {"changedByNormalisation": sum(1 for x in accepted if sanitize_slug(x["post"]["slug"], x["post"]["id"]) != x["post"]["slug"]),
                  "collisions": [{"slug": k, "postIds": v} for k, v in slugs.items() if len(v) > 1]},
        "skipReasonCounts": reason_counts,
        "skipped": skipped,
        "network": {"requests": client.requests, "cacheHits": client.cache_hits, "downloadWorkers": a.download_workers},
        "notes": [
            "Register each manifest.json object verbatim in daily-squirt-migration.approvedManifests before running.",
            "Run foundation first as the ordered per-type chunks of foundation/selection.json (<= 1,000 IDs each), then articles-NN with --types article --source-ids from source-ids.txt, then --types comment with <= 1,000 ids of comment-source-ids.txt per run.",
            "Article title/excerpt/body/slug/publishDate are refreshed from WordPress at import and checked against wordpress.checksum/modifiedGmt.",
        ],
    }
    (out / "SUMMARY.json").write_text(json.dumps(summary, indent=1, ensure_ascii=False))
    print(json.dumps({k: summary[k] for k in ("acceptedPosts", "skippedPosts", "counts", "media", "perPost", "videoOrigins", "comments", "slugs", "skipReasonCounts")}, indent=1))


if __name__ == "__main__":
    main()
