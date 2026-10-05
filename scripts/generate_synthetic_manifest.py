#!/usr/bin/env python3
"""Generate new local DS exports; never reads legacy fixtures or uses a network."""
import argparse
import hashlib
import json
from pathlib import Path
from urllib.parse import urlparse
import struct
import zlib


def encode(value):
    # Generated keys/text are ASCII; mirror ECMAScript integer-key enumeration.
    if isinstance(value, dict):
        def order(k):
            numeric = k.isdigit() and str(int(k)) == k and int(k) < 4294967295
            return (0, int(k)) if numeric else (1, k)
        value = {k: encode(value[k]) for k in sorted(value, key=order)}
    elif isinstance(value, list):
        value = [encode(v) for v in value]
    return value


def raw(value):
    return json.dumps(encode(value), ensure_ascii=False, separators=(",", ":")).encode()


def digest(value):
    return hashlib.sha256(value).hexdigest()


def png():
    def chunk(kind, payload):
        return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", zlib.crc32(kind + payload))
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 8, 8, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress((b"\0" + b"\x40\x80\xc0" * 8) * 8)) + chunk(b"IEND", b"")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--source-origin", required=True)
    parser.add_argument("--count", type=int, choices=(25, 1000), required=True)
    parser.add_argument("--prefix", required=True)
    args = parser.parse_args()
    if not args.prefix.isascii() or not args.prefix.replace("-", "").isalnum():
        parser.error("prefix must be an ASCII letters/digits/hyphen label")
    u = urlparse(args.source_origin)
    if u.scheme not in ("http", "https") or not u.netloc or u.path or u.query or u.fragment or u.username:
        parser.error("source-origin must be an exact approved local mock origin")
    output = args.output.resolve()
    if output.exists() or "daily-squirt-code" in str(output).lower() or "migration-in-rust" in str(output).lower():
        parser.error("output must be a new external directory")
    output.mkdir(mode=0o700, parents=False)
    origin = args.source_origin
    meta = {"schemaVersion": "v1", "repositoryFixture": False, "sourceOwner": "Local QA", "sourceAuthority": "synthetic.local", "sourceLocation": "new synthetic generator", "approvedAt": "2026-09-10T00:00:00Z"}
    key = lambda sid: json.dumps(["wordpress", sid], separators=(",", ":"))
    ref = lambda sid: {"sourceKey": key(sid)}
    sid = lambda typ, n=1: f"{args.prefix}-{typ}-{n}"
    article_ids = [sid("article", n) for n in range(1, args.count + 1)]
    media_id = sid("media")
    rows = {
        "category": [{"sourceId": sid("category"), "data": {"name": "Synthetic Public", "slug": sid("category"), "accessLevel": "public"}}],
        "subCategory": [{"sourceId": sid("subcategory"), "data": {"name": "Synthetic Child", "slug": sid("subcategory"), "accessLevel": "public", "category": ref(sid("category"))}}],
        "author": [{"sourceId": sid("author"), "data": {"name": "Synthetic Author"}}],
        "performer": [{"sourceId": sid("performer"), "data": {"name": "Synthetic Performer", "slug": sid("performer")}}],
        "studio": [{"sourceId": sid("studio"), "data": {"name": "Synthetic Studio", "slug": sid("studio"), "description": "<p>Synthetic studio.</p>"}}],
        "article": [{"sourceId": i, "data": {"title": f"Synthetic article {n}", "slug": i, "excerpt": "New synthetic content", "publishDate": "2026-01-01T00:00:00.000Z", "body": f'<p>Article {n}</p><figure><img src="{origin}/synthetic.png"><figcaption>Generated illustration</figcaption></figure>', "coverImage": {"publicImage": ref(media_id), "altText": "Synthetic illustration"}, "author": ref(sid("author")), "subCategory": ref(sid("subcategory")), "performers": [ref(sid("performer"))], "studios": [ref(sid("studio"))], "allowComments": True, "commentExpiryDays": 30}} for n, i in enumerate(article_ids, 1)],
        "header": [{"sourceId": sid("header"), "data": {"logo": ref(media_id), "ctaLabel": "Synthetic", "navLinks": []}}],
        "footer": [{"sourceId": sid("footer"), "data": {"logo": ref(media_id), "description": "Synthetic footer"}}],
        "homepage": [{"sourceId": sid("homepage"), "data": {"featuredArticles": [ref(article_ids[0])], "latestArticleLimit": 10, "discoverMoreLimit": 10}}],
        "popup": [{"sourceId": sid("popup"), "data": {"key": sid("popup"), "title": "Synthetic popup", "label": "Continue", "url": "/"}}],
        "adConfig": [{"sourceId": sid("ad"), "data": {"key": sid("ad"), "page": "home", "type": "responsive", "responsiveAd": {"placement": "top"}}}],
    }
    manifest = dict(meta, sourceSystem="wordpress", sourceOrigins=[origin], types={t: [r["sourceId"] for r in records] for t, records in rows.items()}, files={}, comments={"mode": "excluded"})
    manifest["taxonomy"] = {"category": {sid("category"): {"accessLevel": "public"}}, "subCategory": {sid("subcategory"): {"accessLevel": "public", "parentSourceKey": key(sid("category"))}}}
    image = png()
    (output / "synthetic.png").write_bytes(image)
    manifest["media"] = {media_id: {"sourceKey": key(media_id), "checksum": digest(image), "accessLevel": "public", "mimeType": "image/png", "size": len(image), "transformVersion": "v1", "requiredFor": {"article": article_ids, "header": [sid("header")], "footer": [sid("footer")]}}}
    rows["media"] = [{"sourceId": media_id, "url": f"{origin}/synthetic.png", "aliases": []}]
    for typ, records in rows.items():
        if typ != "media":
            for record in records:
                record["sourceUrl"] = f'{origin}/{typ}/{record["sourceId"]}'
        body = raw(dict(meta, records=records))
        filename = f"{typ}.json"
        (output / filename).write_bytes(body)
        manifest["files"][typ] = {"path": filename, "sha256": digest(body)}
    # Publication is intentionally absent: this artifact approves draft migration only.
    manifest["manifestHash"] = digest(raw(manifest))
    (output / "manifest.json").write_bytes(raw(manifest))
    (output / "article-selection.txt").write_text(",".join(article_ids) + "\n")
    print(json.dumps({"manifest": str(output / "manifest.json"), "manifestHash": manifest["manifestHash"], "articles": args.count}))


if __name__ == "__main__":
    main()
