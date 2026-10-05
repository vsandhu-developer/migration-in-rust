use clap::Parser;
use migration_system::{
    canonical::{checksum, manifest_hash, sha256, source_key, stringify},
    checkpoint::Checkpoint,
    cli::Cli,
    error::Error,
    http::{public_address, retry_after, Boundary},
    manifest::{external_path, Manifest},
    source::{apply_wordpress, gmt, rewrite_html, verify_media},
};
use serde_json::json;
use std::{collections::BTreeMap, fs, net::IpAddr};

#[test]
fn checksum_matches_actual_ds_source_js_golden() {
    let data = json!({"title":"Cafe\u{301}\r\nTitle","documentId":"ignored","body":"<p>Hi</p>","coverImage":{"publicImage":{"sourceKey":"[\"wordpress\",\"9\"]"}},"author":{"sourceKey":"[\"wordpress\",\"1\"]"},"rating":1.25});
    let relations =
        BTreeMap::from([("author".into(), vec![source_key("wordpress", "1").unwrap()])]);
    let media = vec![
        json!({"sourceKey":source_key("wordpress","9").unwrap(),"checksum":"a".repeat(64),"transformVersion":"v1"}),
    ];
    assert_eq!(
        checksum(&data, relations, media).unwrap(),
        "96ff7de5e123a1ac3a4408f597c1da6f3a7c0c5b9eb10b224313b16693a889db"
    );
}
#[test]
fn canonical_numbers_and_keys_follow_javascript() {
    assert_eq!(
        stringify(&json!({"10":1.0,"2":1e-7,"a":-0.0,"b":0.000001}), false).unwrap(),
        "{\"2\":1e-7,\"10\":1,\"a\":0,\"b\":0.000001}"
    );
    assert!(stringify(&json!({"n":9007199254740992_u64}), false).is_err());
    assert!(stringify(&json!({"__proto__":null}), false).is_err());
}
#[test]
fn source_keys_are_collision_free_and_strict() {
    assert_ne!(
        source_key("a", "b:c").unwrap(),
        source_key("a.b", "c").unwrap()
    );
    for id in ["", " 1", "1\n"] {
        assert!(source_key("wordpress", id).is_err());
    }
    assert!(source_key("Wordpress", "1").is_err());
}
#[test]
fn public_address_policy_denies_private_metadata_mapped_and_documentation() {
    for addr in [
        "127.0.0.1",
        "10.1.1.1",
        "172.16.0.1",
        "192.168.0.1",
        "169.254.169.254",
        "100.64.0.1",
        "198.18.0.1",
        "192.0.2.1",
        "::1",
        "::ffff:8.8.8.8",
        "fe80::1",
        "fd00::1",
        "2001:db8::1",
    ] {
        assert!(!public_address(addr.parse::<IpAddr>().unwrap()), "{addr}");
    }
    assert!(public_address("8.8.8.8".parse().unwrap()));
    assert!(public_address("2606:4700:4700::1111".parse().unwrap()));
    assert!(Boundary::new(vec!["http://127.0.0.1:9999".into()], None).is_err());
}
#[test]
fn retry_after_never_retries_early_when_outside_budget() {
    assert_eq!(retry_after("3").unwrap().as_secs(), 3);
    assert!(retry_after("61").is_err());
    assert!(retry_after("nonsense").is_err());
}
#[test]
fn media_checks_content_not_extension_and_never_accepts_changed_bytes() {
    let bytes = b"\x89PNG\r\n\x1a\nsynthetic";
    let def = json!({"size":bytes.len(),"checksum":sha256(bytes),"mimeType":"image/png"});
    verify_media(bytes, &def).unwrap();
    assert_eq!(
        verify_media(b"<html>bot</html>", &def).unwrap_err().code,
        "media_size_mismatch"
    );
    let mut bad = bytes.to_vec();
    bad[10] = b'X';
    assert_eq!(
        verify_media(&bad, &def).unwrap_err().code,
        "media_checksum_mismatch"
    );
    let mut wrong = def.clone();
    wrong["mimeType"] = json!("image/jpeg");
    assert!(verify_media(bytes, &wrong).is_err());
}
#[test]
fn html_preserves_gallery_caption_cta_order_and_rewrites_srcset() {
    let html="<p>Before</p><figure><a href=\"https://source.invalid/a.jpg\"><img alt=\"A &amp; B\" src=\"https://source.invalid/a.jpg\" srcset=\"https://source.invalid/a.jpg 1x, https://source.invalid/b.jpg 2x\"></a><figcaption>Caption <em>kept</em></figcaption></figure><div class=\"wp-block-gallery\"><img src=\"https://source.invalid/b.jpg\"></div><a class=\"wp-block-button__link\" href=\"/join\">Join</a><p>After</p>";
    let maps = BTreeMap::from([
        (
            "https://source.invalid/a.jpg".into(),
            "https://cdn.invalid/a.jpg".into(),
        ),
        (
            "https://source.invalid/b.jpg".into(),
            "/api/ds-media/asset-protected/original".into(),
        ),
    ]);
    let out = rewrite_html(html, &maps).unwrap();
    assert!(!out.contains("source.invalid"));
    assert!(out.contains("figcaption>Caption <em>kept</em>"));
    assert!(out.contains("1x, /api/ds-media/asset-protected/original 2x"));
    assert!(out.contains("href=\"/join\""));
    assert!(out.find("Before").unwrap() < out.find("After").unwrap());
    assert_eq!(
        rewrite_html(html, &BTreeMap::new()).unwrap_err().code,
        "required_inline_media_unmapped"
    );
}
#[test]
fn canonical_manifest_hash_is_order_independent_and_detects_tamper() {
    let a = json!({"schemaVersion":"v1","manifestHash":"ignored","n":2});
    let b = json!({"n":2,"schemaVersion":"v1"});
    assert_eq!(manifest_hash(&a).unwrap(), manifest_hash(&b).unwrap());
    let mut changed = b;
    changed["n"] = json!(3);
    assert_ne!(manifest_hash(&a).unwrap(), manifest_hash(&changed).unwrap());
}
#[test]
fn repository_paths_and_symlink_bypasses_are_rejected_before_read() {
    assert!(external_path(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).is_err());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("link");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(env!("CARGO_MANIFEST_DIR"), &path).unwrap();
        assert!(external_path(&path).is_err());
    }
}
#[test]
fn checkpoint_is_batched_atomic_locked_and_requires_same_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checkpoint");
    let identity = json!({"run":"test","manifest":"hash"});
    {
        let mut cp = Checkpoint::open(&path, &identity, false).unwrap();
        cp.set_run(&identity, &format!("dsrun-{}", "a".repeat(48)))
            .unwrap();
        for i in 0..26 {
            cp.record(&json!({"type":"article","sourceKey":source_key("wordpress",&i.to_string()).unwrap(),"targetDocumentId":format!("d{i}"),"operation":"created","ingressUrl":"private-sentinel"})).unwrap();
        }
        assert!(path.join("batch-000001.json").exists());
        assert!(!path.join("batch-000002.json").exists());
        assert!(Checkpoint::open(&path, &identity, true).is_err());
        cp.flush().unwrap();
    }
    assert!(!fs::read_to_string(path.join("batch-000001.json"))
        .unwrap()
        .contains("private-sentinel"));
    assert!(Checkpoint::open(&path, &json!({"run":"changed"}), true).is_err());
    {
        let cp = Checkpoint::open(&path, &identity, true).unwrap();
        assert!(cp.run_id.is_some());
    }
    fs::write(
        path.join("batch-000002.json"),
        "{\"entries\":[],\"checksum\":\"tampered\"}",
    )
    .unwrap();
    assert!(Checkpoint::open(&path, &identity, true).is_err());
}
#[test]
fn cli_disallows_legacy_modes_and_requires_explicit_selection() {
    assert!(Cli::try_parse_from(["ds", "--phase", "users"]).is_err());
    assert!(Cli::try_parse_from(["ds", "--phase", "all"]).is_err());
    assert!(Cli::try_parse_from(["ds"]).is_err());
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_migration-system"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    for flag in [
        "--config",
        "--manifest",
        "--phase",
        "--run-id",
        "--source-ids",
        "--types",
        "--checkpoint",
        "--dry-run",
        "--skip-images",
        "--resume",
        "--wp-per-page",
    ] {
        assert!(help.contains(flag));
    }
}
#[test]
fn invalid_configuration_exits_nonzero_without_creating_checkpoint_or_leaking_path() {
    let dir = tempfile::tempdir().unwrap();
    let cp = dir.path().join("checkpoint");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_migration-system"))
        .args([
            "--config",
            "/not-found/private-sentinel",
            "--manifest",
            "/not-found",
            "--phase",
            "import",
            "--run-id",
            "test",
            "--source-ids",
            "1",
            "--types",
            "article",
            "--checkpoint",
        ])
        .arg(&cp)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!cp.exists());
    let err = String::from_utf8(output.stderr).unwrap();
    assert!(!err.contains("private-sentinel"));
    assert!(err.contains("input_unreadable"));
}
#[test]
fn error_json_cannot_contain_response_body_or_credentials() {
    let e = Error::http(401);
    assert_eq!(
        serde_json::to_value(e).unwrap(),
        json!({"code":"http_rejected","status":401})
    );
}
#[test]
fn wordpress_uses_original_gmt_and_requires_manifest_primary_leaf() {
    let mut post = json!({"id":7,"title":{"rendered":"A &amp; B"},"content":{"rendered":"<figure class=\"wp-block-gallery-4\"><img src=\"x\"><figcaption>keep</figcaption></figure>"},"excerpt":{"rendered":"<p>Excerpt</p>"},"date_gmt":"2020-01-01T00:00:00","modified_gmt":"2026-09-01T12:00:00","status":"publish","categories":[4,9],"author":3,"slug":"seven","link":"https://source.invalid/seven"});
    let stable_post = json!({"id":7,"title":{"rendered":"A &amp; B"},"content":{"rendered":"<figure class=\"wp-block-gallery-instance\"><img src=\"x\"><figcaption>keep</figcaption></figure>"},"excerpt":{"rendered":"<p>Excerpt</p>"},"date_gmt":"2020-01-01T00:00:00","modified_gmt":"2026-09-01T12:00:00","status":"publish","categories":[4,9],"author":3,"slug":"seven","link":"https://source.invalid/seven"});
    let sum = sha256(stringify(&stable_post, false).unwrap().as_bytes());
    let mut row = migration_system::manifest::Record {
        source_id: "7".into(),
        source_url: "https://source.invalid/seven".parse().unwrap(),
        data: json!({"subCategory":{"sourceKey":source_key("wordpress","9").unwrap()},"author":{"sourceKey":source_key("wordpress","3").unwrap()}}),
        wordpress: Some(
            json!({"status":"publish","modifiedGmt":"2026-09-01T12:00:00","primaryCategoryId":9,"checksum":sum}),
        ),
    };
    apply_wordpress(
        &mut row,
        &post,
        &json!({"9":source_key("wordpress","9").unwrap()}),
    )
    .unwrap();
    assert_eq!(row.data["publishDate"], "2020-01-01T00:00:00.000Z");
    assert_eq!(row.data["title"], "A & B");
    assert!(row.data["body"].as_str().unwrap().contains("figcaption"));
    assert!(row.data["body"]
        .as_str()
        .unwrap()
        .contains("wp-block-gallery-instance"));
    post["content"]["rendered"] = json!("<figure class=\"wp-block-gallery-99\"><img src=\"x\"><figcaption>keep</figcaption></figure>");
    apply_wordpress(
        &mut row,
        &post,
        &json!({"9":source_key("wordpress","9").unwrap()}),
    )
    .unwrap();
    post["categories"] = json!([4]);
    assert_eq!(
        apply_wordpress(&mut row, &post, &json!({}))
            .unwrap_err()
            .code,
        "wordpress_primary_category_absent"
    );
    assert!(gmt("unknown").is_err());
}

#[test]
fn independent_manifests_reject_gating_defaults_hash_changes_and_duplicate_ids() {
    let dir = tempfile::tempdir().unwrap();
    let origin = "https://source.invalid";
    let export = json!({"schemaVersion":"v1","repositoryFixture":false,"sourceOwner":"QA","sourceAuthority":"synthetic.local","approvedAt":"2026-09-01T00:00:00Z","sourceLocation":"synthetic-generated","records":[{"sourceId":"1","sourceUrl":format!("{origin}/category/1"),"data":{"name":"Restricted","slug":"restricted","accessLevel":"public"}}]});
    let bytes = serde_json::to_vec(&export).unwrap();
    fs::write(dir.path().join("taxonomy.json"), &bytes).unwrap();
    let mut m = json!({"schemaVersion":"v1","repositoryFixture":false,"sourceOwner":"QA","sourceAuthority":"synthetic.local","sourceSystem":"wordpress","approvedAt":"2026-09-01T00:00:00Z","sourceLocation":"synthetic-generated","sourceOrigins":[origin],"types":{"category":["1"]},"taxonomy":{"category":{"1":{"accessLevel":"gated"}}},"files":{"category":{"path":"taxonomy.json","sha256":sha256(&bytes)}}});
    m["manifestHash"] = json!(manifest_hash(&m).unwrap());
    let path = dir.path().join("manifest.json");
    fs::write(&path, m.to_string()).unwrap();
    assert_eq!(
        Manifest::load(&path).err().unwrap().code,
        "taxonomy_access_invalid"
    );
    m["taxonomy"]["category"]["1"]["accessLevel"] = json!("public");
    m["manifestHash"] = json!(manifest_hash(&m).unwrap());
    fs::write(&path, m.to_string()).unwrap();
    Manifest::load(&path).unwrap();
    fs::write(dir.path().join("taxonomy.json"), b"tampered").unwrap();
    assert_eq!(
        Manifest::load(&path).err().unwrap().code,
        "export_hash_mismatch"
    );
}

#[test]
fn typed_relations_keep_performer_studio_namespaces_and_reject_numeric_fallbacks() {
    let key = source_key("wordpress", "17").unwrap();
    let mut manifest = Manifest {
        raw: json!({"sourceSystem":"wordpress","types":{"performer":["17"],"studio":["17"],"author":["1"],"subCategory":["2"]},"media":{"9":{"sourceKey":source_key("wordpress","9").unwrap(),"checksum":"a".repeat(64)}}}),
        records: BTreeMap::new(),
        media: BTreeMap::new(),
    };
    let mut record = migration_system::manifest::Record {
        source_id: "100".into(),
        source_url: "https://source.invalid/a".parse().unwrap(),
        wordpress: None,
        data: json!({"title":"Synthetic","slug":"synthetic","publishDate":"2020-01-01T00:00:00Z","body":"<p>Body</p>","author":{"sourceKey":source_key("wordpress","1").unwrap()},"subCategory":{"sourceKey":source_key("wordpress","2").unwrap()},"coverImage":{"publicImage":{"sourceKey":source_key("wordpress","9").unwrap()}},"performers":[{"sourceKey":key}],"studios":[{"sourceKey":key}]}),
    };
    migration_system::protocol::record_body("article", &record, &manifest).unwrap();
    manifest.raw["types"]["studio"] = json!([]);
    assert_eq!(
        migration_system::protocol::record_body("article", &record, &manifest)
            .unwrap_err()
            .code,
        "relation_unapproved"
    );
    manifest.raw["types"]["studio"] = json!(["17"]);
    record.data["author"] = json!(1);
    assert_eq!(
        migration_system::protocol::record_body("article", &record, &manifest)
            .unwrap_err()
            .code,
        "relation_must_use_source_key"
    );
    record.data["documentId"] = json!("spoofed");
    assert_eq!(
        migration_system::protocol::record_body("article", &record, &manifest)
            .unwrap_err()
            .code,
        "record_fields_invalid"
    );
}

#[test]
fn generator_produces_fresh_external_1000_article_manifests_without_fixture_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("generated");
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/generate_synthetic_manifest.py"
        ))
        .arg("--output")
        .arg(&output)
        .args([
            "--source-origin",
            "http://127.0.0.1:19080",
            "--count",
            "1000",
            "--prefix",
            "testsynthetic",
        ])
        .output()
        .unwrap();
    assert!(result.status.success());
    let manifest = Manifest::load(&output.join("manifest.json")).unwrap();
    assert_eq!(manifest.records["article"].len(), 1000);
    assert!(!manifest.records.contains_key("comment"));
    assert_eq!(manifest.records.len(), 11);
    for (typ, records) in &manifest.records {
        for record in records {
            migration_system::protocol::record_body(typ, record, &manifest).unwrap();
        }
    }
    migration_system::source::verify_media(
        &fs::read(output.join("synthetic.png")).unwrap(),
        &manifest.media.values().next().unwrap().definition,
    )
    .unwrap();
}

#[test]
fn approved_iframe_is_preserved_separately_from_stored_image_mappings() {
    let html="<p>Before</p><iframe src=\"https://video.invalid/embed/approved\" title=\"Synthetic video\"></iframe><p>After</p>";
    let frames = vec!["https://video.invalid/embed/approved".into()];
    let result =
        migration_system::source::rewrite_html_with_frames(html, &BTreeMap::new(), &frames)
            .unwrap();
    assert!(result.contains("https://video.invalid/embed/approved"));
    assert!(result.contains("Synthetic video"));
    assert_eq!(
        rewrite_html(html, &BTreeMap::new()).unwrap_err().code,
        "iframe_source_unapproved"
    );
}

#[test]
fn slug_normalisation_matches_old_article_transformer() {
    use migration_system::source::sanitize_slug;
    for (raw, want) in [
        ("already-clean", "already-clean"),
        ("Keeps_Case.and~tilde", "Keeps_Case.and~tilde"),
        ("wait%e2%80%a6-what", "wait-e2-80-a6-what"),
        ("caf\u{e9} au lait", "caf-au-lait"),
        ("--a!!b--", "a-b"),
        // Old quirk kept: a literal "-" after a replaced char is not collapsed.
        ("a - b", "a--b"),
        ("%%%", "post-42"),
        ("", "post-42"),
    ] {
        assert_eq!(sanitize_slug(raw, 42), want, "{raw}");
    }
}

#[test]
fn video_sources_pass_through_but_posters_and_picture_sources_are_images() {
    use migration_system::source::rewrite_html_with_frames;
    let html = "<video poster=\"https://source.invalid/p.jpg\" controls><source src=\"https://videos.invalid/a/b.mp4\" type=\"video/mp4\"></video><picture><source srcset=\"https://source.invalid/p.jpg 1x\"><img src=\"https://source.invalid/p.jpg\"></picture>";
    let maps = BTreeMap::from([(
        "https://source.invalid/p.jpg".to_string(),
        "https://cdn.invalid/p.jpg".to_string(),
    )]);
    let out = rewrite_html_with_frames(html, &maps, &[]).unwrap();
    assert!(out.contains("src=\"https://videos.invalid/a/b.mp4\""));
    assert!(out.contains("poster=\"https://cdn.invalid/p.jpg\""));
    assert!(out.contains("srcset=\"https://cdn.invalid/p.jpg 1x\""));
    assert!(!out.contains("source.invalid"));
    // The poster is an image and still has to be mapped (rehosted).
    assert_eq!(
        rewrite_html_with_frames(html, &BTreeMap::new(), &[])
            .unwrap_err()
            .code,
        "required_inline_media_unmapped"
    );
    let insecure = "<video src=\"http://videos.invalid/a.mp4\"></video>";
    assert_eq!(
        rewrite_html_with_frames(insecure, &BTreeMap::new(), &[])
            .unwrap_err()
            .code,
        "video_source_not_https"
    );
}

#[test]
fn cta_blocks_map_to_registration_ctas_and_leave_the_body_like_the_old_importer() {
    use migration_system::content_parser::ContentParser;
    let cta = "class=\"wp-block-button__link has-white-color has-text-color has-background\"";
    let html = format!(
        "<a {cta} href=\"https://join.invalid/first\">First</a><p>One</p><div class=\"wp-block-button\"><a {cta} href=\"https://join.invalid/x\">Join   <strong>now</strong></a></div><p>Two</p><div class=\"wp-block-buttons\"><div class=\"wp-block-button\"><a {cta} href=\"https://join.invalid/nested\">Nested</a></div></div><a class=\"wp-block-button__link\" href=\"/plain\">Plain</a>"
    );
    let old_indices = ContentParser::parse(&html)
        .into_iter()
        .filter(|b| b.is_cta())
        .map(|b| b.index)
        .collect::<Vec<_>>();
    let (body, ctas) = ContentParser::split_ctas(&html);
    assert_eq!(
        ctas.iter().map(|b| b.index).collect::<Vec<_>>(),
        old_indices
    );
    assert_eq!(old_indices, vec![0, 2]);
    assert!(!body.contains("join.invalid/first") && !body.contains("join.invalid/x"));
    // Old parser only treated top-level buttons as CTAs; nested groups and
    // incomplete button classes stay in the body verbatim.
    assert!(body.contains("join.invalid/nested") && body.contains("/plain"));
    assert!(body.find("One").unwrap() < body.find("Two").unwrap());
    assert_eq!(
        migration_system::source::registration_ctas(&ctas),
        json!([
            {"enabled":true,"afterParagraph":1,"label":"First","url":"https://join.invalid/first"},
            {"enabled":true,"afterParagraph":2,"label":"Join now","url":"https://join.invalid/x"}
        ])
    );
    let (unchanged, none) = ContentParser::split_ctas("<p>No buttons</p>");
    assert_eq!((unchanged.as_str(), none.len()), ("<p>No buttons</p>", 0));
}

#[test]
fn video_shortcode_request_artifacts_are_stabilised() {
    use migration_system::source::stable_wordpress_html;
    let render = |n: u32, shim: bool| {
        format!(
            "<div class=\"wp-video\">{}<video class=\"wp-video-shortcode\" id=\"video-665842-{n}\" preload=\"metadata\"><source type=\"video/mp4\" src=\"https://videos.invalid/a.mp4?_={n}\" /><a href=\"https://videos.invalid/a.mp4\">https://videos.invalid/a.mp4</a></video></div><p>id=\"video-x-1\" ?_=abc</p>",
            if shim { "<!--[if lt IE 9]><script>document.createElement('video');</script><![endif]-->\n" } else { "" }
        )
    };
    let first = stable_wordpress_html(&render(1, true));
    assert_eq!(first, stable_wordpress_html(&render(9, false)));
    assert!(first.contains("id=\"video-665842-instance\""));
    assert!(first.contains("src=\"https://videos.invalid/a.mp4\" />"));
    assert!(!first.contains("[if lt IE 9]"));
    // Non-matching look-alikes are untouched.
    assert!(first.contains("<p>id=\"video-x-1\" ?_=abc</p>"));
    assert_eq!(
        stable_wordpress_html("<a href=\"/x?a=1&#038;_=7\">"),
        "<a href=\"/x?a=1\">"
    );
}

/// Runs scripts/generate_wordpress_manifest.py against the offline WordPress double in
/// scripts/tests/fake_wordpress.py (synthetic posts/comments, real public taxonomy/performer/studio
/// source files, synthetic authors and user mappings).
fn generate_offline(out: &std::path::Path, args: &[&str]) {
    let script = format!(
        "import sys; sys.path.insert(0, {:?}); from pathlib import Path; from fake_wordpress import run_generator, write_inputs; \
         tmp = Path(sys.argv[1]); tmp.mkdir(parents=True, exist_ok=True); inp = write_inputs(tmp); \
         run_generator(['--cache-dir', str(tmp / 'cache'), '--authors', str(inp / 'authors.json'), '--download-workers', '1', \
         '--user-mapping', str(inp / 'users-state.json'), '--user-mapping', str(inp / 'admin-state.json'), '--fallback-user-id', '3', \
         '--comments-approval', 'CMT-1', '--publication-approval', 'PUB-1'] + sys.argv[2:])",
        concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/tests")
    );
    let result = std::process::Command::new("python3")
        .args(["-c", &script])
        .arg(out)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn offline_cli(
    config: &std::path::Path,
    manifest: &std::path::Path,
    typ: &str,
    ids: &[String],
) -> Cli {
    Cli::try_parse_from([
        "migration-system",
        "--config",
        config.to_str().unwrap(),
        "--manifest",
        manifest.to_str().unwrap(),
        "--phase",
        "inventory",
        "--run-id",
        "offline-check",
        "--source-ids",
        &ids.join(","),
        "--types",
        typ,
        "--dry-run",
    ])
    .unwrap()
}

#[test]
fn full_foundation_and_comments_for_existing_articles_pass_importer_validation() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.json");
    fs::write(
        &config,
        json!({"target":"https://cms.example.invalid/","targetTokenEnv":"DS_OFFLINE_UNUSED","wordpress":"https://daily.squirt.org/wp-json/wp/v2/"}).to_string(),
    )
    .unwrap();

    // Full-parity foundation: every source record, imported as ordered per-type chunks <= 1,000.
    let full = dir.path().join("full");
    generate_offline(
        &full,
        &[
            "--out-dir",
            full.join("out").to_str().unwrap(),
            "--count",
            "2",
            "--batch-size",
            "2",
        ],
    );
    let foundation = full.join("out/foundation/manifest.json");
    let manifest = Manifest::load(&foundation).unwrap();
    assert_eq!(manifest.records["performer"].len(), 1838);
    assert_eq!(manifest.records["studio"].len(), 62);
    let selection: serde_json::Value =
        serde_json::from_slice(&fs::read(full.join("out/foundation/selection.json")).unwrap())
            .unwrap();
    let mut covered = 0;
    for chunk in selection["chunks"].as_array().unwrap() {
        let typ = chunk["type"].as_str().unwrap();
        let ids: Vec<String> = serde_json::from_value(chunk["sourceIds"].clone()).unwrap();
        assert!(ids.len() <= 1000);
        let selected = manifest.selected(&[typ.to_owned()], &ids).unwrap();
        for (t, record) in &selected {
            migration_system::protocol::record_body(t, record, &manifest).unwrap();
        }
        migration_system::runner::preflight(&offline_cli(&config, &foundation, typ, &ids)).unwrap();
        covered += selected.len();
    }
    assert_eq!(
        covered,
        manifest.records.values().map(Vec::len).sum::<usize>()
    );

    // Comments-only manifest: the article relation resolves through dependencies.article.
    let cmts = dir.path().join("cmts");
    generate_offline(
        &cmts,
        &[
            "--out-dir",
            cmts.join("out").to_str().unwrap(),
            "--post-ids",
            "101,102,103",
        ],
    );
    let path = cmts.join("out/comments-01/manifest.json");
    let manifest = Manifest::load(&path).unwrap();
    assert!(manifest.raw["types"].get("article").is_none());
    let ids: Vec<String> = manifest.records["comment"]
        .iter()
        .map(|r| r.source_id.clone())
        .collect();
    for (t, record) in manifest.selected(&["comment".into()], &ids).unwrap() {
        migration_system::protocol::record_body(&t, &record, &manifest).unwrap();
    }
    migration_system::runner::preflight(&offline_cli(&config, &path, "comment", &ids)).unwrap();
    let mut without = manifest.clone();
    without.raw["dependencies"] = json!({});
    assert_eq!(
        migration_system::protocol::record_body(
            "comment",
            &manifest.records["comment"][0],
            &without
        )
        .unwrap_err()
        .code,
        "relation_unapproved"
    );
}
