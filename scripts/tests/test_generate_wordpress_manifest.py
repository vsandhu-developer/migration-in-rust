"""Offline tests for scripts/generate_wordpress_manifest.py (python3 -m unittest discover -s scripts/tests)."""
import csv
import json
import re
import tempfile
import unittest
from pathlib import Path

from fake_wordpress import ORIGIN, FakeClient, load_generator, run_generator, write_gated_taxonomy, write_inputs

REPO = Path(__file__).resolve().parents[2]
SLUG = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
gen = load_generator()


def key(i):
    return json.dumps(["wordpress", str(i)], separators=(",", ":"))


def csv_rows(path):
    with open(path, newline="", encoding="utf-8-sig") as f:
        return list(csv.reader(f))[1:]


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-gen-test-"))
        self.inputs = write_inputs(self.tmp)
        self.out = self.tmp / "out"

    def generate(self, *extra):
        argv = ["--out-dir", str(self.out), "--cache-dir", str(self.tmp / "cache"), "--authors", str(self.inputs / "authors.json"),
                "--download-workers", "1"] + list(extra)
        run_generator(argv, gen)
        return self.out

    def manifest(self, rel):
        m = json.loads((self.out / rel / "manifest.json").read_text())
        self.assertEqual(m["manifestHash"], gen.sha256(gen.canonical({k: v for k, v in m.items() if k != "manifestHash"})))
        for t, f in m["files"].items():
            self.assertEqual(gen.sha256((self.out / rel / f["path"]).read_bytes()), f["sha256"])
        return m

    def export(self, rel, typ):
        return json.loads((self.out / rel / f"{typ}.json").read_text())["records"]


class LoaderParity(unittest.TestCase):
    def test_tag_loader_matches_old_positional_rules(self):
        tmp = Path(tempfile.mkdtemp(prefix="ds-gen-csv-"))
        path = tmp / "tags.csv"
        path.write_text("whatever,header,names\n"
                        "5,Good Name,good-name\n"
                        "0,Zero Id,zero\n"
                        "abc,Bad Id,bad\n"
                        "6,,no-name\n"
                        " 7 , Spaced Name ,\n"
                        "8,Under_Score,Under_Score\n"
                        "9,Dup,good-name\n"
                        "5,Again,again\n"
                        "10,!!!,\n"
                        "11,Short\n", encoding="utf-8")
        tags, stats = gen.load_tags(path, "performer")
        self.assertEqual(sorted(tags), [5, 7, 8, 9, 10, 11])
        self.assertEqual(tags[7], {"name": "Spaced Name", "slug": "spaced-name"})
        self.assertEqual(tags[8]["slug"], "under-score")
        self.assertEqual(tags[9]["slug"], "good-name-9")
        self.assertEqual(tags[10]["slug"], "performer-10")
        self.assertEqual(tags[11]["slug"], "short")
        self.assertEqual(stats["skippedInvalidIdOrName"], 3)
        self.assertEqual(stats["duplicateIds"], 1)
        self.assertEqual(stats["slugNormalised"], 1)
        self.assertEqual(stats["slugDerivedFromName"], 3)
        self.assertEqual(stats["slugDeduplicated"], 1)

    def test_real_source_files_load_every_old_record(self):
        for name, kind in (("performers/DS-Performers.csv", "performer"), ("studios/DS-studio.csv", "studio")):
            path = REPO / "data/source" / name
            rows = csv_rows(path)
            expected = {int(r[0]) for r in rows if r and r[0].strip().isdigit() and int(r[0]) > 0 and len(r) > 1 and r[1].strip()}
            tags, stats = gen.load_tags(path, kind)
            self.assertEqual(set(tags), expected, name)
            slugs = [t["slug"] for t in tags.values()]
            self.assertTrue(all(SLUG.match(s) and len(s) <= 128 for s in slugs), name)
            self.assertEqual(len(slugs), len(set(slugs)), name)
            self.assertEqual(stats["loaded"], len(rows), name)


class FullFoundation(Base):
    def test_full_scope_includes_every_source_record_in_ordered_chunks(self):
        self.generate("--count", "2", "--batch-size", "1", "--publication-approval", "PUB-1")
        m = self.manifest("foundation")
        performers = csv_rows(REPO / "data/source/performers/DS-Performers.csv")
        studios = csv_rows(REPO / "data/source/studios/DS-studio.csv")
        categories = json.loads((REPO / "data/source/categories/DS-categories.json").read_text())
        subs = {f"{c['dsSlug']}/{s['dsSlug']}" for c in categories.values() for s in c["SubCategories"].values()}
        self.assertEqual(len(m["types"]["performer"]), len(performers))
        self.assertEqual(len(m["types"]["studio"]), len(studios))
        self.assertEqual(set(m["types"]["category"]), {c["dsSlug"] for c in categories.values()})
        self.assertEqual(set(m["types"]["subCategory"]), subs)
        self.assertEqual(m["types"]["author"], ["7", "8", "9"])
        for t in ("category", "subCategory"):
            self.assertEqual(set(m["taxonomy"][t]), set(m["types"][t]))
        for sid, tax in m["taxonomy"]["subCategory"].items():
            parent = json.loads(tax["parentSourceKey"])[1]
            self.assertFalse(m["taxonomy"]["category"][parent]["accessLevel"] == "gated" and tax["accessLevel"] != "gated")
        self.assertEqual(m["publication"]["approval"], "PUB-1")
        for t, ids in m["types"].items():
            self.assertEqual(m["publication"]["publish"][t], [key(i) for i in ids])
        self.assertNotIn("releaseComments", m["publication"])
        for rec in self.export("foundation", "performer") + self.export("foundation", "studio"):
            self.assertTrue(SLUG.match(rec["data"]["slug"]))
            self.assertEqual(set(rec["data"]), {"name", "slug"})
        sel = json.loads((self.out / "foundation/selection.json").read_text())
        self.assertEqual(sel["manifestHash"], m["manifestHash"])
        order = [c["type"] for c in sel["chunks"]]
        self.assertEqual(order, ["category", "subCategory", "author", "performer", "performer", "studio"])
        self.assertTrue(all(0 < len(c["sourceIds"]) <= 1000 for c in sel["chunks"]))
        for t, ids in m["types"].items():
            self.assertEqual([i for c in sel["chunks"] if c["type"] == t for i in c["sourceIds"]], ids)
        summary = json.loads((self.out / "SUMMARY.json").read_text())
        self.assertEqual(summary["foundation"]["scope"], "all")
        self.assertEqual(summary["foundation"]["sourceFiles"]["performer"]["loaded"], len(performers))

    def test_referenced_scope_keeps_old_narrow_behaviour(self):
        self.generate("--count", "2", "--batch-size", "2", "--foundation-scope", "referenced")
        m = self.manifest("foundation")
        self.assertEqual(m["types"], {"category": ["porn-and-play"], "subCategory": ["porn-and-play/erotic-stories", "porn-and-play/latest-studio-releases"],
                                      "author": ["7", "8"], "performer": ["6881"], "studio": ["3583"]})
        self.assertNotIn("publication", m)


class ArticleComments(Base):
    def test_article_manifest_comments_use_mapping_order_fallback_and_length_limit(self):
        self.generate("--count", "2", "--batch-size", "2", "--comments-approval", "CMT-1", "--publication-approval", "PUB-1",
                      "--user-mapping", str(self.inputs / "users-state.json"), "--user-mapping", str(self.inputs / "admin-state.json"),
                      "--fallback-user-id", "3")
        m = self.manifest("articles-01")
        self.assertEqual(m["types"]["comment"], ["9001", "9002", "9003", "9005"])  # 9004 > 5,000 chars: skipped; 9005 (1,667) kept
        self.assertEqual(m["comments"], {"mode": "existing-users", "approval": "CMT-1",
                                         "users": {key("wp-user-55"): 900, key("wp-user-0"): 3, key("wp-user-77"): 401, key("wp-user-66"): 3}})
        self.assertEqual(m["publication"]["releaseComments"], {"comment": [key(i) for i in ("9001", "9002", "9003", "9005")]})
        rec = self.export("articles-01", "comment")[0]
        self.assertEqual(rec["data"]["article"], {"sourceKey": key(101)})
        self.assertEqual(rec["wordpress"]["postId"], "101")
        self.assertEqual((self.out / "articles-01/comment-source-ids.txt").read_text().strip(), "9001,9002,9003,9005")
        summary = json.loads((self.out / "SUMMARY.json").read_text())
        self.assertEqual(summary["comments"]["skippedOverLength"], 1)
        self.assertEqual(summary["comments"]["fallback"], 3)  # anonymous + two comments of the unmapped author 66


class CommentsForExistingArticles(Base):
    def test_comments_only_manifest_depends_on_existing_articles(self):
        ids = self.tmp / "ids.txt"
        ids.write_text("101\n102, 103\n101\n")
        self.generate("--post-ids-file", str(ids), "--comments-approval", "CMT-2", "--publication-approval", "PUB-2",
                      "--user-mapping", str(self.inputs / "users-state.json"), "--user-mapping", str(self.inputs / "admin-state.json"),
                      "--fallback-user-id", "3")
        self.assertFalse((self.out / "foundation").exists())
        m = self.manifest("comments-01")
        self.assertEqual(list(m["types"]), ["comment"])
        self.assertEqual(list(m["files"]), ["comment"])
        self.assertEqual(m["types"]["comment"], ["9001", "9002", "9003", "9005", "9101"])
        self.assertEqual(m["dependencies"], {"article": [key(101), key(103)]})
        self.assertEqual(m["comments"]["users"][key("wp-user-12345")], 3)
        self.assertEqual(m["publication"], {"approval": "PUB-2", "releaseComments": {"comment": [key(i) for i in m["types"]["comment"]]}})
        for rec in self.export("comments-01", "comment"):
            self.assertIn(rec["data"]["article"]["sourceKey"], m["dependencies"]["article"])
            self.assertTrue(rec["sourceUrl"].startswith(ORIGIN + "/?p="))
            self.assertEqual(set(rec["wordpress"]), {"postId", "checksum"})
        summary = json.loads((self.out / "SUMMARY.json").read_text())
        self.assertEqual(summary["postsWithoutComments"], [102])

    def test_gated_article_is_migrated_with_public_media_and_comments_are_kept(self):
        gated = write_gated_taxonomy(self.tmp, REPO / "data/source/categories/DS-categories.json")
        # The article path migrates the gated post: it keeps its gated subCategory, its featured
        # image becomes the public cover and every media record is public.
        self.generate("--count", "3", "--batch-size", "3", "--categories", str(gated))
        summary = json.loads((self.out / "SUMMARY.json").read_text())
        self.assertEqual(summary["acceptedPosts"], 3)
        self.assertNotIn(104, [s["postId"] for s in summary["skipped"]])
        self.assertEqual(summary["articlesByAccessLevel"]["gated"], 1)
        self.assertEqual(summary["media"]["classificationPerManifest"]["explicit"], 0)
        m = self.manifest("articles-01")
        self.assertIn("104", m["types"]["article"])
        self.assertEqual({d["accessLevel"] for d in m["media"].values()}, {"public"})
        rec = next(r for r in self.export("articles-01", "article") if r["sourceId"] == "104")
        self.assertEqual(rec["data"]["subCategory"], {"sourceKey": key("gated-cat/gated-sub")})
        self.assertEqual(rec["data"]["coverImage"]["publicImage"], {"sourceKey": key("wp-attachment-503")})
        self.assertNotIn("explicitImage", rec["data"]["coverImage"])
        self.assertIn("104", m["media"]["wp-attachment-503"]["requiredFor"]["article"])
        self.assertEqual(self.manifest("foundation")["taxonomy"]["subCategory"]["gated-cat/gated-sub"]["accessLevel"], "gated")
        # The comments path for an existing gated article keeps its comments.
        self.out = self.tmp / "out-comments"
        FakeClient.calls.clear()
        self.generate("--post-ids", "104", "--categories", str(gated), "--comments-approval", "CMT-3",
                      "--user-mapping", str(self.inputs / "users-state.json"))
        self.assertFalse(any("/posts" in u for u in FakeClient.calls))
        m = self.manifest("comments-01")
        self.assertEqual(m["types"]["comment"], ["9201"])
        self.assertEqual(m["dependencies"], {"article": [key(104)]})
        self.assertEqual(m["comments"]["users"], {key("wp-user-55"): 900})

    def test_post_ids_require_comments_approval(self):
        with self.assertRaises(SystemExit):
            self.generate("--post-ids", "101")

    def test_post_ids_reject_count(self):
        with self.assertRaises(SystemExit):
            self.generate("--post-ids", "101", "--count", "1", "--comments-approval", "C")


if __name__ == "__main__":
    unittest.main()
