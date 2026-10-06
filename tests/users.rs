//! Loopback protocol tests for the admin-users/users phases. Synthetic users only.
use migration_system::{
    cli::{Cli, Phase},
    users::{execute, load_users},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

const TOKEN_ENV: &str = "DS_SYNTHETIC_USERS_TEST_TOKEN";
const TOKEN: &str = "synthetic-local-users-token";

struct Request {
    path: String,
    headers: String,
    body: Value,
}
struct Reply {
    status: u16,
    body: Value,
    headers: Vec<(String, String)>,
}
impl Reply {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            body,
            headers: vec![],
        }
    }
}
struct Server {
    origin: Url,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(f: impl Fn(Request) -> Reply + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let f = Arc::new(f);
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let f = f.clone();
            tokio::spawn(async move {
                let mut input = Vec::new();
                let mut part = [0u8; 8192];
                let header_end = loop {
                    let n = socket.read(&mut part).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    input.extend_from_slice(&part[..n]);
                    if let Some(i) = input.windows(4).position(|s| s == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let headers = String::from_utf8(input[..header_end].to_vec()).unwrap();
                let length = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|s| s.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while input.len() < header_end + length {
                    let n = socket.read(&mut part).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    input.extend_from_slice(&part[..n]);
                }
                let path = headers
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                let body = serde_json::from_slice(&input[header_end..header_end + length])
                    .unwrap_or(Value::Null);
                let reply = f(Request {
                    path,
                    headers,
                    body,
                });
                let bytes = reply.body.to_string();
                let mut response = format!(
                    "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    bytes.len()
                );
                for (k, v) in reply.headers {
                    response.push_str(&format!("{k}: {v}\r\n"));
                }
                response.push_str("\r\n");
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(bytes.as_bytes()).await;
            });
        }
    });
    Server {
        origin: format!("http://{addr}/").parse().unwrap(),
        task,
    }
}

/// Synthetic accounts: `.invalid` domains, obviously fake names.
fn synthetic_users(n: u64) -> Value {
    json!((1..=n)
        .map(|i| json!({
            "wpId": 1000 + i,
            "username": format!("synthetic-user-{i}"),
            // Every third synthetic user has no email to exercise the placeholder.
            "email": if i % 3 == 0 { String::new() } else { format!("synthetic{i}@qa.invalid") },
            "slug": format!("synthetic-user-{i}"),
            "image": format!("https://qa.invalid/avatar-{i}.png"),
            "roles": ["subscriber"],
        }))
        .collect::<Vec<_>>())
}
fn fixture(dir: &Path, origin: &Url, phase: Phase, users: &Value) -> Cli {
    std::fs::write(dir.join("users.json"), users.to_string()).unwrap();
    let config = json!({"target":origin,"targetTokenEnv":TOKEN_ENV,"local":true,"localTargetOrigin":origin,"localTargetAddress":"127.0.0.1","maxAttempts":3,"timeoutSeconds":5});
    std::fs::write(dir.join("config.json"), config.to_string()).unwrap();
    std::env::set_var(TOKEN_ENV, TOKEN);
    Cli {
        config: dir.join("config.json"),
        manifest: None,
        phase,
        run_id: "synthetic-users".into(),
        source_ids: vec![],
        types: vec![],
        checkpoint: Some(dir.join("checkpoint")),
        dry_run: false,
        skip_images: false,
        resume: false,
        wp_per_page: 50,
        users_file: Some(dir.join("users.json")),
        batch_size: None,
        media_concurrency: None,
        record_batch_size: None,
    }
}

#[derive(Default)]
struct Cms {
    /// email -> userId for accounts that already exist on the CMS.
    accounts: BTreeMap<String, u64>,
    posts: Vec<(String, usize)>,
    lookups: Vec<Vec<String>>,
    /// wpIds rejected with a code (once each).
    reject: BTreeMap<u64, String>,
    fail_batches_with: Option<u16>,
    throttle_once: bool,
}
async fn mock(state: Arc<Mutex<Cms>>) -> Server {
    server(move |r| {
        assert!(r
            .headers
            .contains(&format!("authorization: Bearer {TOKEN}")));
        let mut s = state.lock().unwrap();
        if r.path == "/api/ds-migration/users/lookup" {
            let ids: Vec<String> = serde_json::from_value(r.body["identifiers"].clone()).unwrap();
            assert!(ids.len() <= 200);
            s.lookups.push(ids.clone());
            let (found, missing): (Vec<_>, Vec<_>) =
                ids.iter().partition(|i| s.accounts.contains_key(*i));
            return Reply::json(json!({"data":{
                "found": found.iter().map(|i| json!({"identifier":i,"userId":s.accounts[*i],"foundBy":"email"})).collect::<Vec<_>>(),
                "notFound": missing.iter().map(|i| json!({"identifier":i})).collect::<Vec<_>>(),
            }}));
        }
        assert!(matches!(
            r.path.as_str(),
            "/api/ds-migration/users" | "/api/ds-migration/admin-users"
        ));
        let rows = r.body["data"].as_array().unwrap().clone();
        let max = if r.path.ends_with("admin-users") { 100 } else { 200 };
        assert!(!rows.is_empty() && rows.len() <= max);
        if s.throttle_once {
            s.throttle_once = false;
            return Reply {
                status: 429,
                body: json!({"error":"slow down"}),
                headers: vec![("Retry-After".into(), "0".into())],
            };
        }
        s.posts.push((r.path.clone(), rows.len()));
        if let Some(status) = s.fail_batches_with {
            return Reply {
                status,
                body: json!({"error":{"message":"private upstream detail"}}),
                headers: vec![],
            };
        }
        let mut results = Vec::new();
        for row in rows {
            // Exact contract keys; the source `image` is never forwarded.
            let mut keys = row.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
            keys.sort();
            assert_eq!(keys, ["email", "roles", "slug", "username", "wpId"]);
            let wp = row["wpId"].as_u64().unwrap();
            let email = row["email"].as_str().unwrap().to_owned();
            assert!(!email.is_empty());
            if let Some(code) = s.reject.remove(&wp) {
                results.push(json!({"wpId":wp,"status":"failed","code":code}));
            } else if let Some(id) = s.accounts.get(&email) {
                results.push(json!({"wpId":wp,"status":"existing","userId":id}));
            } else {
                let id = 5000 + wp;
                s.accounts.insert(email, id);
                results.push(json!({"wpId":wp,"status":"created","userId":id}));
            }
        }
        Reply::json(json!({"data":{"results":results},"meta":{"contractVersion":"v1"}}))
    })
    .await
}

fn assert_private(report: &impl serde::Serialize, dir: &Path) {
    let out = serde_json::to_string(report).unwrap();
    assert!(!out.contains("@"), "stdout report must not contain emails");
    assert!(!out.contains("synthetic-user-"), "no usernames in report");
    assert!(!out.contains("private upstream detail"));
    let state = std::fs::read_to_string(dir.join("checkpoint/users-state.json")).unwrap();
    assert!(!state.contains("@") && !state.contains("synthetic-user-"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in ["users-state.json", "writer.lock"] {
            let mode = std::fs::metadata(dir.join("checkpoint").join(f))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{f}");
        }
    }
}

#[tokio::test]
async fn users_batches_placeholders_mapping_and_resume_skips_completed() {
    let state = Arc::new(Mutex::new(Cms::default()));
    // wpId 1004 is rejected with a code that embeds an email; it must be sanitized.
    state
        .lock()
        .unwrap()
        .reject
        .insert(1004, "duplicate synthetic4@qa.invalid".into());
    // wpId 1005's email already exists on the CMS.
    state
        .lock()
        .unwrap()
        .accounts
        .insert("synthetic5@qa.invalid".into(), 777);
    let cms = mock(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cli = fixture(dir.path(), &cms.origin, Phase::Users, &synthetic_users(7));
    cli.batch_size = Some(3);

    cli.dry_run = true;
    let dry = execute(&cli).await.unwrap();
    assert_eq!(
        (dry.selected, dry.placeholder_email, dry.batches),
        (7, 2, 3)
    );
    assert!(!dir.path().join("checkpoint").exists());
    assert!(state.lock().unwrap().posts.is_empty());
    cli.dry_run = false;

    let first = execute(&cli).await.unwrap();
    assert_eq!(
        (first.selected, first.created, first.existing, first.failed),
        (7, 5, 1, 1)
    );
    assert_eq!(first.placeholder_email, 2);
    assert_eq!(first.failures.len(), 1);
    assert_eq!(first.failures[0].wp_id, 1004);
    assert_eq!(first.failures[0].code, "cms_failed");
    assert!(!first.passed());
    // 3 batches of <=3, then exactly one lookup for the one failure.
    assert_eq!(
        state
            .lock()
            .unwrap()
            .posts
            .iter()
            .map(|p| p.1)
            .collect::<Vec<_>>(),
        vec![3, 3, 1]
    );
    assert_eq!(state.lock().unwrap().lookups.len(), 1);
    assert_private(&first, dir.path());
    let saved: Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("checkpoint/users-state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["mapping"]["1005"], 777);
    assert_eq!(saved["mapping"]["1003"], 6003);
    assert_eq!(saved["entries"]["1004"]["status"], "failed");

    // Existing checkpoint without --resume is refused.
    assert_eq!(
        execute(&cli).await.err().unwrap().code,
        "checkpoint_exists_use_resume"
    );
    cli.resume = true;
    let resumed = execute(&cli).await.unwrap();
    assert!(resumed.passed());
    assert_eq!(
        (resumed.already_complete, resumed.created, resumed.failed),
        (6, 1, 0)
    );
    let posts = state.lock().unwrap().posts.clone();
    assert_eq!(posts.last().unwrap().1, 1, "only the failed wpId is resent");
    // Placeholder uses the wpId, never the username.
    assert!(state
        .lock()
        .unwrap()
        .accounts
        .contains_key("migrated+1003@example.invalid"));
    let again = execute(&cli).await.unwrap();
    assert_eq!((again.already_complete, again.batches), (7, 0));
}

#[tokio::test]
async fn failed_batch_is_recovered_by_lookup_and_429_is_retried() {
    let state = Arc::new(Mutex::new(Cms {
        throttle_once: true,
        ..Default::default()
    }));
    let cms = mock(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let cli = fixture(dir.path(), &cms.origin, Phase::Users, &synthetic_users(4));
    let ok = execute(&cli).await.unwrap();
    assert!(ok.passed());
    assert_eq!(ok.created, 4);
    assert_eq!(state.lock().unwrap().posts.len(), 1, "429 then one success");

    // Second import (new checkpoint): the CMS rejects the whole batch, but every
    // account already exists, so the lookup recovers each mapping.
    state.lock().unwrap().fail_batches_with = Some(400);
    let dir2 = tempfile::tempdir().unwrap();
    let cli2 = fixture(dir2.path(), &cms.origin, Phase::Users, &synthetic_users(4));
    let recovered = execute(&cli2).await.unwrap();
    assert!(recovered.passed());
    assert_eq!((recovered.existing, recovered.recovered), (4, 4));
    assert_private(&recovered, dir2.path());

    // Unknown accounts stay failed with the HTTP status and a safe code.
    let dir3 = tempfile::tempdir().unwrap();
    let mut users = synthetic_users(2);
    users[0]["wpId"] = json!(9001);
    users[0]["email"] = json!("fresh@qa.invalid");
    let cli3 = fixture(dir3.path(), &cms.origin, Phase::Users, &users);
    let failed = execute(&cli3).await.unwrap();
    assert_eq!((failed.failed, failed.existing), (1, 1));
    assert_eq!(failed.failures[0].wp_id, 9001);
    assert_eq!(failed.failures[0].code, "http_rejected");
    assert_eq!(failed.failures[0].status, Some(400));
    assert_private(&failed, dir3.path());
}

#[tokio::test]
async fn admin_users_use_admin_route_limit_and_no_lookup() {
    let state = Arc::new(Mutex::new(Cms::default()));
    state
        .lock()
        .unwrap()
        .reject
        .insert(1002, "admin_role_unmapped".into());
    let cms = mock(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut users = synthetic_users(150);
    users[0]["roles"] = json!(["administrator"]);
    let mut cli = fixture(dir.path(), &cms.origin, Phase::AdminUsers, &users);
    cli.batch_size = Some(101);
    assert_eq!(
        execute(&cli).await.err().unwrap().code,
        "users_batch_size_invalid"
    );
    cli.batch_size = None;
    let report = execute(&cli).await.unwrap();
    assert_eq!((report.created, report.failed, report.batches), (149, 1, 2));
    assert_eq!(report.failures[0].code, "admin_role_unmapped");
    let s = state.lock().unwrap();
    assert!(s.lookups.is_empty());
    assert!(s
        .posts
        .iter()
        .all(|(p, n)| p == "/api/ds-migration/admin-users" && *n <= 100));
    assert_private(&report, dir.path());
}

#[tokio::test]
async fn strict_arguments_and_input_validation_before_network() {
    let dir = tempfile::tempdir().unwrap();
    let origin: Url = "http://127.0.0.1:9/".parse().unwrap();
    let mut cli = fixture(dir.path(), &origin, Phase::Users, &synthetic_users(2));
    cli.batch_size = Some(201);
    assert_eq!(
        execute(&cli).await.err().unwrap().code,
        "users_batch_size_invalid"
    );
    cli.batch_size = None;
    cli.types = vec!["article".into()];
    assert_eq!(
        execute(&cli).await.err().unwrap().code,
        "manifest_arguments_forbidden"
    );
    cli.types.clear();
    cli.checkpoint = None;
    assert_eq!(
        execute(&cli).await.err().unwrap().code,
        "checkpoint_required"
    );
    let mut dup = synthetic_users(2);
    dup[1]["wpId"] = dup[0]["wpId"].clone();
    std::fs::write(dir.path().join("dup.json"), dup.to_string()).unwrap();
    assert_eq!(
        load_users(&dir.path().join("dup.json")).err().unwrap().code,
        "users_wp_id_duplicate"
    );
    std::fs::write(
        dir.path().join("extra.json"),
        json!([{"wpId":1,"email":"x@qa.invalid","password":"nope"}]).to_string(),
    )
    .unwrap();
    let err = load_users(&dir.path().join("extra.json")).err().unwrap();
    assert_eq!(err.code, "users_file_invalid");
    assert!(!serde_json::to_string(&err).unwrap().contains("qa.invalid"));
    // Content phases reject account arguments.
    let mut content = fixture(dir.path(), &origin, Phase::Import, &synthetic_users(1));
    content.manifest = Some(dir.path().join("manifest.json"));
    content.types = vec!["article".into()];
    content.source_ids = vec!["1".into()];
    assert_eq!(
        migration_system::runner::execute(&content)
            .await
            .err()
            .unwrap()
            .code,
        "users_arguments_forbidden"
    );
}
