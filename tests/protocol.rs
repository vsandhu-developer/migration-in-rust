use migration_system::{
    canonical::{manifest_hash, sha256, source_key},
    cli::{Cli, Phase},
    http::{Boundary, Http},
    manifest::Config,
    protocol::Cms,
    runner::execute,
    source::Wordpress,
};
use reqwest::Method;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

#[derive(Clone)]
struct Request {
    method: String,
    path: String,
    headers: String,
    body: Vec<u8>,
}
struct Reply {
    status: u16,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}
impl Reply {
    fn json(value: Value) -> Self {
        Self {
            status: 200,
            body: value.to_string().into_bytes(),
            headers: vec![("Content-Type".into(), "application/json".into())],
        }
    }
    fn status(status: u16) -> Self {
        Self {
            status,
            body: b"private-error-sentinel".to_vec(),
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
                    assert!(input.len() < 65536);
                };
                let headers = String::from_utf8(input[..header_end].to_vec()).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
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
                let mut line = headers.lines().next().unwrap().split_whitespace();
                let method = line.next().unwrap().into();
                let path = line.next().unwrap().into();
                let reply = f(Request {
                    method,
                    path,
                    headers,
                    body: input[header_end..header_end + length].to_vec(),
                });
                let mut response = format!(
                    "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (k, v) in reply.headers {
                    response.push_str(&format!("{k}: {v}\r\n"));
                }
                response.push_str("\r\n");
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(&reply.body).await;
            });
        }
    });
    Server {
        origin: format!("http://{addr}/").parse().unwrap(),
        task,
    }
}
fn http(origin: &Url, attempts: usize) -> Http {
    Http {
        boundary: Boundary::new(
            vec![origin.origin().ascii_serialization()],
            Some((
                origin.origin().ascii_serialization(),
                "127.0.0.1".parse().unwrap(),
            )),
        )
        .unwrap(),
        attempts,
        timeout: Duration::from_secs(3),
    }
}
fn envelope(data: Value) -> Value {
    json!({"data":data,"meta":{"contractVersion":"v1","accessClass":"importer"}})
}

#[tokio::test]
async fn retries_429_and_5xx_but_not_permanent_rejection_and_redacts_body() {
    let count = Arc::new(Mutex::new(0));
    let c = count.clone();
    let s = server(move |_| {
        let mut n = c.lock().unwrap();
        *n += 1;
        match *n {
            1 => {
                let mut r = Reply::status(429);
                r.headers.push(("Retry-After".into(), "0".into()));
                r
            }
            2 => Reply::status(503),
            _ => Reply::json(json!({"ok":true})),
        }
    })
    .await;
    assert_eq!(
        http(&s.origin, 3)
            .json(Method::GET, s.origin.clone(), None, None)
            .await
            .unwrap(),
        json!({"ok":true})
    );
    assert_eq!(*count.lock().unwrap(), 3);
    let count = Arc::new(Mutex::new(0));
    let c = count.clone();
    let s = server(move |_| {
        *c.lock().unwrap() += 1;
        Reply::status(401)
    })
    .await;
    let error = http(&s.origin, 3)
        .json(Method::GET, s.origin.clone(), None, None)
        .await
        .unwrap_err();
    assert_eq!(error.status, Some(401));
    assert!(!serde_json::to_string(&error).unwrap().contains("sentinel"));
    assert_eq!(*count.lock().unwrap(), 1);
}
#[tokio::test]
async fn redirects_revalidate_origin_and_do_not_contact_other_local_services() {
    let target_calls = Arc::new(Mutex::new(0));
    let c = target_calls.clone();
    let target = server(move |_| {
        *c.lock().unwrap() += 1;
        Reply::json(json!({}))
    })
    .await;
    let dest = target.origin.to_string();
    let source = server(move |_| Reply {
        status: 302,
        body: vec![],
        headers: vec![("Location".into(), dest.clone())],
    })
    .await;
    let e = http(&source.origin, 1)
        .json(Method::GET, source.origin.clone(), None, None)
        .await
        .unwrap_err();
    assert_eq!(e.code, "network_origin_denied");
    assert_eq!(*target_calls.lock().unwrap(), 0);
}
#[tokio::test]
async fn rejects_bot_html_and_oversize_body() {
    let source = server(|_| Reply {
        status: 200,
        body: b"<html>challenge</html>".to_vec(),
        headers: vec![("Content-Type".into(), "text/html".into())],
    })
    .await;
    assert_eq!(
        http(&source.origin, 1)
            .json(Method::GET, source.origin.clone(), None, None)
            .await
            .unwrap_err()
            .code,
        "source_not_json_or_bot_protection"
    );
    let source = server(|_| Reply {
        status: 200,
        body: vec![0; 100],
        headers: vec![],
    })
    .await;
    let r = http(&source.origin, 1)
        .send(
            Method::GET,
            source.origin.clone(),
            None,
            migration_system::http::Body::Empty,
            50,
        )
        .await;
    assert_eq!(r.err().unwrap().code, "response_too_large");
}
#[tokio::test]
async fn wordpress_exact_ids_authentication_pages_dates_and_all_comment_pages() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let c = calls.clone();
    let source = server(move |r| {
        assert!(r.headers.contains("authorization: Basic synthetic-only"));
        c.lock().unwrap().push(r.path.clone());
        let u = Url::parse(&format!("http://local{}", r.path)).unwrap();
        let q = u.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        assert_eq!(q["per_page"], "2");
        let page = q["page"].parse::<usize>().unwrap();
        let mut response = if u.path().ends_with("posts") {
            assert_eq!(q["include"], "3,9,25");
            assert_eq!(q["status"], "draft");
            assert_eq!(q["context"], "edit");
            Reply::json(if page == 1 {
                json!([{"id":3,"status":"draft"},{"id":9,"status":"draft"}])
            } else {
                json!([{"id":25,"status":"draft"}])
            })
        } else {
            Reply::json(if page == 1 {
                json!([{"id":1},{"id":2}])
            } else {
                json!([{"id":3}])
            })
        };
        response
            .headers
            .push(("X-WP-TotalPages".into(), "2".into()));
        response
    })
    .await;
    let wp = Wordpress {
        http: http(&source.origin, 1),
        endpoint: source.origin.join("wp-json/wp/v2/").unwrap(),
        authorization: Some("Basic synthetic-only".into()),
        page_size: 2,
        allow_private: true,
    };
    let posts = wp
        .posts(&["3".into(), "9".into(), "25".into()], "draft")
        .await
        .unwrap();
    assert_eq!(posts.len(), 3);
    assert!(!posts.contains_key("1"));
    assert_eq!(wp.comments("3").await.unwrap().len(), 3);
    assert_eq!(calls.lock().unwrap().len(), 4);
    let denied = Wordpress {
        allow_private: false,
        ..wp
    };
    assert_eq!(
        denied
            .posts(&["3".into()], "private")
            .await
            .unwrap_err()
            .code,
        "wordpress_status_forbidden"
    );
}
#[tokio::test]
async fn uncertain_success_is_looked_up_before_idempotent_retry() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let c = calls.clone();
    let key = source_key("wordpress", "17").unwrap();
    let k = key.clone();
    let run = format!("dsrun-{}", "a".repeat(48));
    let expected_run = run.clone();
    let source=server(move|r|{let mut calls=c.lock().unwrap();calls.push(r.method.clone());
        if calls.len()==1{return Reply::status(503);}
        if r.method=="GET"{let mut v=envelope(json!([{"sourceKey":k,"targetDocumentId":"actual-doc"}]));v["meta"]["pagination"]=json!({"pageCount":1});return Reply::json(v);}
        Reply::json(envelope(json!({"runId":expected_run,"type":"article","sourceKey":k,"targetDocumentId":"actual-doc","operation":"reused"})))
    }).await;
    let cms = Cms {
        local_media_base: None,
        http: http(&source.origin, 2),
        origin: source.origin.clone(),
        authorization: "Bearer synthetic-only".into(),
    };
    let result = cms
        .record(
            &run,
            "article",
            &json!({"source":{"sourceKey":key},"data":{"title":"Synthetic"}}),
        )
        .await
        .unwrap();
    assert_eq!(result["targetDocumentId"], "actual-doc");
    assert_eq!(*calls.lock().unwrap(), vec!["POST", "GET", "POST"]);
}

#[derive(Default)]
struct State {
    records: BTreeMap<String, String>,
    posts: usize,
    create_runs: usize,
    failed_once: bool,
    fail_id: Option<String>,
    lookups: usize,
    rolled_back: bool,
}
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nsynthetic-protocol-only";
async fn mock_cms(state: Arc<Mutex<State>>) -> Server {
    server(move|r|{
    if r.path=="/synthetic.png" {assert!(!r.headers.contains("authorization:"));return Reply{status:200,body:PNG.to_vec(),headers:vec![("Content-Type".into(),"image/png".into())]};}
    assert!(r.headers.contains("authorization: Bearer synthetic-local-api-token"));let mut s=state.lock().unwrap();let run=format!("dsrun-{}","b".repeat(48));
    if r.method=="POST"&&r.path=="/api/ds-migration/runs"{let b:Value=serde_json::from_slice(&r.body).unwrap();assert!(b["sourceIds"].as_array().unwrap().len()<=1000);assert_eq!(b["allowedTypes"],json!(["article"]));s.create_runs+=1;return Reply::json(envelope(json!({"runId":run})));}
    if r.path.ends_with("/media") {
        assert!(r.headers.contains("multipart/form-data; boundary="));assert!(r.body.windows(PNG.len()).any(|s|s==PNG));
        let fields=String::from_utf8_lossy(&r.body);assert!(fields.contains("name=\"manifest\""));assert!(fields.contains("name=\"file\""));assert!(fields.contains(&sha256(PNG)));assert!(!fields.contains("private-error-sentinel"));
        return Reply::json(envelope(json!({"runId":run,"sourceKey":source_key("wordpress","media-1").unwrap(),"assetKey":format!("asset-{}","c".repeat(64)),"targetDocumentId":"stored-image","operation":"reused","ingressUrl":format!("https://cdn.invalid/ptp/public/daily-squirt/{}/original","c".repeat(64))})));
    }
    if r.path.ends_with("/rollback"){s.records.clear();s.rolled_back=true;return Reply::json(envelope(json!({"runId":run,"remaining":0,"status":"rolled-back","conflicts":[]})));}
    if r.path.contains("/records/article"){
        if r.method=="GET"{s.lookups+=1;let u=Url::parse(&format!("http://local{}",r.path)).unwrap();let page=u.query_pairs().find(|(k,_)|k=="page").unwrap().1.parse::<usize>().unwrap();let data=s.records.iter().skip((page-1)*100).take(100).map(|(k,id)|json!({"sourceKey":k,"targetDocumentId":id})).collect::<Vec<_>>();let mut v=envelope(json!(data));v["meta"]["pagination"]=json!({"pageCount":s.records.len().div_ceil(100)});return Reply::json(v);}
        let b:Value=serde_json::from_slice(&r.body).unwrap();let key=b["source"]["sourceKey"].as_str().unwrap().to_owned();assert!(b["data"].get("documentId").is_none());assert_eq!(b["source"]["sourceSystem"],"wordpress");s.posts+=1;
        assert!(b["data"]["body"].as_str().unwrap().contains("https://cdn.invalid/ptp/public/daily-squirt/"));assert!(!b["data"]["body"].as_str().unwrap().contains("synthetic.png"));assert_eq!(b["data"]["coverImage"]["publicImage"],json!({"sourceKey":source_key("wordpress","media-1").unwrap()}));assert_eq!(b["data"]["publishDate"],"2020-01-01T00:00:00.000Z");
        if s.fail_id.as_ref().is_some_and(|id|b["source"]["sourceId"]==*id)&&!s.failed_once{s.failed_once=true;return Reply::status(400);}
        let operation=if s.records.contains_key(&key){"reused"}else{"created"};let id=format!("synthetic-doc-{}",b["source"]["sourceId"].as_str().unwrap());s.records.insert(key.clone(),id.clone());return Reply::json(envelope(json!({"runId":run,"type":"article","sourceKey":key,"targetDocumentId":id,"operation":operation})));
    }
    let total=s.records.len();Reply::json(envelope(json!({"runId":run,"reconciliation":{"selected":total,"created":total,"updated":0,"reused":0,"failed":0,"missing":0,"conflicts":0,"mediaMissing":0}})))
}).await
}
fn fixture(origin: &Url, count: usize) -> (tempfile::TempDir, Cli) {
    let dir = tempfile::tempdir().unwrap();
    let ids = (1..=count).map(|i| i.to_string()).collect::<Vec<_>>();
    let o = origin.origin().ascii_serialization();
    let metadata = json!({"schemaVersion":"v1","repositoryFixture":false,"sourceOwner":"Local QA","sourceAuthority":"synthetic.local","sourceLocation":"generated synthetic export","approvedAt":"2026-09-10T00:00:00Z"});
    let mut export = metadata.clone();
    export["records"]=json!(ids.iter().map(|id|json!({"sourceId":id,"sourceUrl":format!("{o}/articles/{id}"),"data":{"title":format!("Synthetic Article {id}"),"slug":format!("synthetic-{id}"),"publishDate":"2020-01-01T00:00:00.000Z","body":format!("<p>Article {id}</p><figure><img src=\"{o}/synthetic.png\"><figcaption>Caption</figcaption></figure>"),"author":{"sourceKey":source_key("wordpress","author-1").unwrap()},"subCategory":{"sourceKey":source_key("wordpress","child-1").unwrap()},"coverImage":{"publicImage":{"sourceKey":source_key("wordpress","media-1").unwrap()}}}})).collect::<Vec<_>>());
    let bytes = serde_json::to_vec(&export).unwrap();
    std::fs::write(dir.path().join("articles.json"), &bytes).unwrap();
    let mut media = metadata.clone();
    media["records"] = json!([{"sourceId":"media-1","url":format!("{o}/synthetic.png")}]);
    let media_bytes = serde_json::to_vec(&media).unwrap();
    std::fs::write(dir.path().join("media.json"), &media_bytes).unwrap();
    let mut m = metadata;
    m["sourceSystem"] = json!("wordpress");
    m["sourceOrigins"] = json!([o]);
    m["types"] = json!({"article":ids});
    m["files"] = json!({"article":{"path":"articles.json","sha256":sha256(&bytes)},"media":{"path":"media.json","sha256":sha256(&media_bytes)}});
    m["dependencies"] = json!({"author":[source_key("wordpress","author-1").unwrap()],"subCategory":[source_key("wordpress","child-1").unwrap()]});
    m["media"] = json!({"media-1":{"sourceKey":source_key("wordpress","media-1").unwrap(),"checksum":sha256(PNG),"accessLevel":"public","mimeType":"image/png","size":PNG.len(),"transformVersion":"v1","requiredFor":{"article":ids}}});
    m["manifestHash"] = json!(manifest_hash(&m).unwrap());
    std::fs::write(dir.path().join("manifest.json"), m.to_string()).unwrap();
    let config = json!({"target":origin,"targetTokenEnv":"DS_SYNTHETIC_IMPORT_TEST_TOKEN","local":true,"localSourceOrigin":origin,"localTargetOrigin":origin,"localSourceAddress":"127.0.0.1","localTargetAddress":"127.0.0.1","maxAttempts":2});
    std::fs::write(dir.path().join("config.json"), config.to_string()).unwrap();
    std::env::set_var(
        "DS_SYNTHETIC_IMPORT_TEST_TOKEN",
        "synthetic-local-api-token",
    );
    let cli = Cli {
        config: dir.path().join("config.json"),
        manifest: Some(dir.path().join("manifest.json")),
        phase: Phase::Import,
        run_id: "synthetic-protocol".into(),
        source_ids: ids,
        types: vec!["article".into()],
        checkpoint: Some(dir.path().join("checkpoint")),
        dry_run: false,
        skip_images: false,
        resume: false,
        wp_per_page: 50,
        users_file: None,
        batch_size: None,
        media_concurrency: None,
        record_batch_size: None,
    };
    (dir, cli)
}
#[tokio::test]
async fn protocol_runs_25_then_1000_exact_records_and_resumes_without_duplicates() {
    for count in [25, 1000] {
        let state = Arc::new(Mutex::new(State::default()));
        let cms = mock_cms(state.clone()).await;
        let (_dir, mut cli) = fixture(&cms.origin, count);
        let report = execute(&cli).await.unwrap();
        assert!(report.passed());
        assert_eq!(report.created, count);
        assert_eq!(state.lock().unwrap().records.len(), count);
        cli.resume = true;
        let repeat = execute(&cli).await.unwrap();
        assert_eq!(repeat.skipped, count);
        assert_eq!(repeat.created, 0);
        assert_eq!(state.lock().unwrap().records.len(), count);
        assert_eq!(state.lock().unwrap().create_runs, 1);
        cli.phase = Phase::Rollback;
        execute(&cli).await.unwrap();
        assert!(state.lock().unwrap().rolled_back);
        assert!(state.lock().unwrap().records.is_empty());
    }
}
#[tokio::test]
async fn dry_run_writes_nothing_and_failed_record_is_named_then_resume_recovers() {
    let state = Arc::new(Mutex::new(State {
        fail_id: Some("13".into()),
        ..Default::default()
    }));
    let cms = mock_cms(state.clone()).await;
    let (_dir, mut cli) = fixture(&cms.origin, 25);
    cli.dry_run = true;
    let inventory = execute(&cli).await.unwrap();
    assert_eq!(inventory.selected, 25);
    assert_eq!(inventory.created, 0);
    assert!(!cli.checkpoint.as_ref().unwrap().exists());
    assert_eq!(state.lock().unwrap().create_runs, 0);
    cli.dry_run = false;
    let failed = execute(&cli).await.unwrap();
    assert!(!failed.passed());
    assert_eq!(failed.failed, 1);
    assert_eq!(failed.created, 24);
    assert_eq!(failed.failures[0].kind, "article");
    assert_eq!(failed.failures[0].status, Some(400));
    assert_eq!(
        failed.selected,
        failed.created + failed.updated + failed.skipped + failed.exceptions + failed.failed
    );
    cli.resume = true;
    let recovered = execute(&cli).await.unwrap();
    assert!(recovered.passed());
    assert_eq!(recovered.created, 1);
    assert_eq!(recovered.skipped, 24);
    assert_eq!(state.lock().unwrap().records.len(), 25);
}
#[test]
fn config_has_no_fallback_and_rejects_unknown_secret_fields() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.json");
    std::fs::write(
        &p,
        b"{\"target\":\"http://localhost\",\"password\":\"secret\"}",
    )
    .unwrap();
    assert!(Config::load(&p).is_err());
}

#[tokio::test]
async fn a_silent_peer_times_out_without_unbounded_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin: Url = format!("http://{}/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let peer = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let mut client = http(&origin, 1);
    client.timeout = Duration::from_millis(40);
    let started = std::time::Instant::now();
    let result = client.json(Method::GET, origin, None, None).await;
    assert_eq!(result.unwrap_err().code, "transport_failed");
    assert!(started.elapsed() < Duration::from_secs(1));
    peer.abort();
}

#[tokio::test]
async fn local_public_ingress_requires_its_own_exact_bucket_prefix() {
    let run = format!("dsrun-{}", "a".repeat(48));
    let response_run = run.clone();
    let calls = Arc::new(Mutex::new(0));
    let observed = calls.clone();
    let source=server(move|_|{let mut call=observed.lock().unwrap();*call+=1;let ingress=if *call==3{format!("http://127.0.0.1:19000/other-bucket/ptp/public/daily-squirt/{}/original","a".repeat(64))}else{format!("http://127.0.0.1:19000/ds-local-public/ptp/public/daily-squirt/{}/original","a".repeat(64))};
        Reply::json(envelope(json!({"runId":response_run,"sourceKey":source_key("wordpress","9").unwrap(),"targetDocumentId":"actual-media","operation":"created","ingressUrl":ingress})))
    }).await;
    let media = migration_system::manifest::Media {
        id: "9".into(),
        url: source.origin.join("source.png").unwrap(),
        aliases: vec![],
        definition: json!({"checksum":sha256(PNG),"accessLevel":"public","mimeType":"image/png"}),
    };
    let mut cms = Cms {
        http: http(&source.origin, 1),
        origin: source.origin.clone(),
        authorization: "Bearer synthetic-only".into(),
        local_media_base: None,
    };
    assert_eq!(
        cms.media(&run, &media, PNG.to_vec(), "wordpress")
            .await
            .unwrap_err()
            .code,
        "cms_media_ingress_invalid"
    );
    cms.local_media_base = Some("http://127.0.0.1:19000/ds-local-public".parse().unwrap());
    cms.media(&run, &media, PNG.to_vec(), "wordpress")
        .await
        .unwrap();
    assert_eq!(
        cms.media(&run, &media, PNG.to_vec(), "wordpress")
            .await
            .unwrap_err()
            .code,
        "cms_media_ingress_invalid"
    );
    assert_eq!(*calls.lock().unwrap(), 3);
}

/// Distinct synthetic PNG per article so each needs its own fetch + upload.
fn distinct_png(i: usize) -> Vec<u8> {
    let mut bytes = PNG.to_vec();
    bytes.extend_from_slice(format!("-media-{i}").as_bytes());
    bytes
}
#[derive(Default)]
struct Concurrency {
    media_in_flight: usize,
    media_max: usize,
    record_in_flight: usize,
    record_max: usize,
    uploads: usize,
    records: BTreeMap<String, String>,
}
async fn concurrent_cms(state: Arc<Mutex<Concurrency>>, count: usize) -> Server {
    server(move |r| {
        let run = format!("dsrun-{}", "d".repeat(48));
        if let Some(i) = r
            .path
            .strip_prefix("/media-")
            .and_then(|s| s.strip_suffix(".png"))
        {
            return Reply {
                status: 200,
                body: distinct_png(i.parse().unwrap()),
                headers: vec![("Content-Type".into(), "image/png".into())],
            };
        }
        if r.method == "POST" && r.path == "/api/ds-migration/runs" {
            return Reply::json(envelope(json!({"runId":run})));
        }
        if r.path.ends_with("/media") {
            let text = String::from_utf8_lossy(&r.body).into_owned();
            let i = text
                .split("media-")
                .nth(1)
                .unwrap()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>();
            {
                let mut s = state.lock().unwrap();
                s.media_in_flight += 1;
                s.media_max = s.media_max.max(s.media_in_flight);
            }
            // Hold the request so overlapping uploads are observable.
            std::thread::sleep(Duration::from_millis(120));
            let mut s = state.lock().unwrap();
            s.media_in_flight -= 1;
            s.uploads += 1;
            let hash = sha256(&distinct_png(i.parse().unwrap()));
            return Reply::json(envelope(json!({"runId":run,"sourceKey":source_key("wordpress",&format!("media-{i}")).unwrap(),"assetKey":format!("asset-{hash}"),"targetDocumentId":format!("image-{i}"),"operation":"created","ingressUrl":format!("https://cdn.invalid/ptp/public/daily-squirt/{hash}/original")})));
        }
        if r.path.contains("/records/article") {
            {
                let mut s = state.lock().unwrap();
                s.record_in_flight += 1;
                s.record_max = s.record_max.max(s.record_in_flight);
            }
            std::thread::sleep(Duration::from_millis(5));
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            let id = b["source"]["sourceId"].as_str().unwrap().to_owned();
            // Ordering invariant: each article body carries its OWN media ingress.
            let own = sha256(&distinct_png(id.parse().unwrap()));
            assert!(b["data"]["body"].as_str().unwrap().contains(&own));
            let key = b["source"]["sourceKey"].as_str().unwrap().to_owned();
            let mut s = state.lock().unwrap();
            s.record_in_flight -= 1;
            s.records.insert(key.clone(), format!("doc-{id}"));
            return Reply::json(envelope(json!({"runId":run,"type":"article","sourceKey":key,"targetDocumentId":format!("doc-{id}"),"operation":"created"})));
        }
        Reply::json(envelope(json!({"runId":run,"reconciliation":{"selected":count,"created":count,"updated":0,"reused":0,"failed":0,"missing":0,"conflicts":0,"mediaMissing":0}})))
    })
    .await
}
fn concurrent_fixture(
    origin: &Url,
    count: usize,
    concurrency: Option<usize>,
) -> (tempfile::TempDir, Cli) {
    let (dir, mut cli) = fixture(origin, count);
    let o = origin.origin().ascii_serialization();
    let metadata = json!({"schemaVersion":"v1","repositoryFixture":false,"sourceOwner":"Local QA","sourceAuthority":"synthetic.local","sourceLocation":"generated synthetic export","approvedAt":"2026-09-10T00:00:00Z"});
    let ids = (1..=count).map(|i| i.to_string()).collect::<Vec<_>>();
    let mut export = metadata.clone();
    export["records"]=json!(ids.iter().map(|id|json!({"sourceId":id,"sourceUrl":format!("{o}/articles/{id}"),"data":{"title":format!("Synthetic Article {id}"),"slug":format!("synthetic-{id}"),"publishDate":"2020-01-01T00:00:00.000Z","body":format!("<p>{id}</p><img src=\"{o}/media-{id}.png\">"),"author":{"sourceKey":source_key("wordpress","author-1").unwrap()},"subCategory":{"sourceKey":source_key("wordpress","child-1").unwrap()},"coverImage":{"publicImage":{"sourceKey":source_key("wordpress",&format!("media-{id}")).unwrap()}}}})).collect::<Vec<_>>());
    let bytes = serde_json::to_vec(&export).unwrap();
    std::fs::write(dir.path().join("articles.json"), &bytes).unwrap();
    let mut media = metadata.clone();
    media["records"] = json!(ids
        .iter()
        .map(|id| json!({"sourceId":format!("media-{id}"),"url":format!("{o}/media-{id}.png")}))
        .collect::<Vec<_>>());
    let media_bytes = serde_json::to_vec(&media).unwrap();
    std::fs::write(dir.path().join("media.json"), &media_bytes).unwrap();
    let mut m = metadata;
    m["sourceSystem"] = json!("wordpress");
    m["sourceOrigins"] = json!([o]);
    m["types"] = json!({"article":ids});
    m["files"] = json!({"article":{"path":"articles.json","sha256":sha256(&bytes)},"media":{"path":"media.json","sha256":sha256(&media_bytes)}});
    m["dependencies"] = json!({"author":[source_key("wordpress","author-1").unwrap()],"subCategory":[source_key("wordpress","child-1").unwrap()]});
    m["media"] = json!(ids.iter().map(|id| {
        let png = distinct_png(id.parse().unwrap());
        (format!("media-{id}"), json!({"sourceKey":source_key("wordpress",&format!("media-{id}")).unwrap(),"checksum":sha256(&png),"accessLevel":"public","mimeType":"image/png","size":png.len(),"transformVersion":"v1","requiredFor":{"article":[id]}}))
    }).collect::<serde_json::Map<_, _>>());
    m["manifestHash"] = json!(manifest_hash(&m).unwrap());
    std::fs::write(dir.path().join("manifest.json"), m.to_string()).unwrap();
    cli.media_concurrency = concurrency;
    (dir, cli)
}
fn checkpoint_entries(dir: &std::path::Path) -> Vec<Value> {
    let mut files = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("batch-")
        })
        .collect::<Vec<_>>();
    files.sort();
    files
        .iter()
        .flat_map(|p| {
            let v: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
            v["entries"].as_array().unwrap().clone()
        })
        .collect()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn media_concurrency_flag_bounds_parallel_uploads_and_keeps_records_sequential() {
    const COUNT: usize = 12;
    for (flag, expected_max) in [(Some(1), 1), (None, 2), (Some(4), 4)] {
        let state = Arc::new(Mutex::new(Concurrency::default()));
        let cms = concurrent_cms(state.clone(), COUNT).await;
        let (_dir, cli) = concurrent_fixture(&cms.origin, COUNT, flag);
        let report = execute(&cli).await.unwrap();
        assert!(report.passed(), "{flag:?}");
        assert_eq!((report.created, report.media_created), (COUNT, COUNT));
        let s = state.lock().unwrap();
        assert_eq!(s.uploads, COUNT);
        assert_eq!(s.media_max, expected_max, "{flag:?}");
        assert_eq!(s.record_max, 1, "record writes stay sequential");
        assert_eq!(s.records.len(), COUNT);
        // Checkpoint journal: every media and record exactly once, media before records.
        let entries = checkpoint_entries(cli.checkpoint.as_ref().unwrap());
        assert_eq!(entries.len(), 2 * COUNT);
        let kinds = entries
            .iter()
            .map(|e| e["type"].as_str().unwrap_or("media").to_owned())
            .collect::<Vec<_>>();
        assert!(kinds[..COUNT].iter().all(|k| k == "media"));
        assert!(kinds[COUNT..].iter().all(|k| k == "article"));
        let keys = entries
            .iter()
            .map(|e| e["sourceKey"].as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(keys.len(), 2 * COUNT);
    }
    // Out-of-range values are rejected before any network or checkpoint work.
    let state = Arc::new(Mutex::new(Concurrency::default()));
    let cms = concurrent_cms(state.clone(), 1).await;
    for bad in [0, 9] {
        let (_dir, cli) = concurrent_fixture(&cms.origin, 1, Some(bad));
        assert_eq!(
            execute(&cli).await.err().unwrap().code,
            "media_concurrency_invalid"
        );
        assert!(!cli.checkpoint.as_ref().unwrap().exists());
    }
    assert_eq!(state.lock().unwrap().uploads, 0);
}

// ---------------------------------------------------------------- record batches (comments)
#[derive(Default)]
struct Batch {
    /// sourceKey -> targetDocumentId committed by the mock CMS.
    records: BTreeMap<String, String>,
    /// (path, item count) for every record write; GET lookups as ("GET", 0).
    calls: Vec<(String, usize)>,
    /// Status for the batch route instead of handling it (403/404/405: unavailable).
    batch_status: Option<u16>,
    /// sourceId rejected once with this item status (per-item failure).
    reject: Option<(String, u16)>,
    /// Commit the first batch, then answer 503 (lost response).
    lose_first: bool,
    /// Answer the first batch with a malformed (reordered) result list.
    malformed_first: bool,
}
fn upsert(s: &mut Batch, run: &str, b: &Value) -> Result<Value, u16> {
    let key = b["source"]["sourceKey"].as_str().unwrap().to_owned();
    assert!(
        b["data"]["user"]["sourceKey"].is_string() && b["data"]["article"]["sourceKey"].is_string()
    );
    if let Some((id, status)) = s.reject.clone() {
        if b["source"]["sourceId"] == id {
            s.reject = None;
            return Err(status);
        }
    }
    let operation = if s.records.contains_key(&key) {
        "reused"
    } else {
        "created"
    };
    let id = format!("comment-doc-{}", b["source"]["sourceId"].as_str().unwrap());
    s.records.insert(key.clone(), id.clone());
    Ok(
        json!({"runId":run,"type":"comment","sourceKey":key,"targetDocumentId":id,"operation":operation}),
    )
}
async fn batch_cms(state: Arc<Mutex<Batch>>) -> Server {
    server(move |r| {
        assert!(r.headers.contains("authorization: Bearer synthetic-local-api-token"));
        let mut s = state.lock().unwrap();
        let run = format!("dsrun-{}", "e".repeat(48));
        if r.method == "POST" && r.path == "/api/ds-migration/runs" {
            return Reply::json(envelope(json!({"runId":run})));
        }
        if r.path.ends_with("/records/comment/batch") {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            let items = b["data"].as_array().unwrap().clone();
            assert_eq!(b.as_object().unwrap().len(), 1);
            assert!(!items.is_empty() && items.len() <= 100);
            assert!(r.body.len() <= 512 * 1024 + 16);
            s.calls.push(("batch".into(), items.len()));
            if let Some(status) = s.batch_status {
                return Reply::status(status);
            }
            let results = items
                .iter()
                .enumerate()
                .map(|(i, item)| match upsert(&mut s, &run, item) {
                    Ok(result) => json!({"index":i,"result":result}),
                    Err(status) => json!({"index":i,"error":{"status":status,"code":"migration_target_changed"}}),
                })
                .collect::<Vec<_>>();
            if s.lose_first {
                s.lose_first = false;
                return Reply::status(503);
            }
            if s.malformed_first {
                s.malformed_first = false;
                let mut reordered = results.clone();
                reordered.reverse();
                return Reply::json(envelope(json!(reordered)));
            }
            return Reply::json(envelope(json!(results)));
        }
        if r.path.contains("/records/comment") {
            if r.method == "GET" {
                s.calls.push(("GET".into(), 0));
                let data = s.records.iter().map(|(k, id)| json!({"sourceKey":k,"targetDocumentId":id})).collect::<Vec<_>>();
                let mut v = envelope(json!(data));
                v["meta"]["pagination"] = json!({"pageCount":1});
                return Reply::json(v);
            }
            s.calls.push(("single".into(), 1));
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            return match upsert(&mut s, &run, &b) {
                Ok(result) => Reply::json(envelope(result)),
                Err(status) => Reply::status(status),
            };
        }
        let total = s.records.len();
        Reply::json(envelope(json!({"runId":run,"reconciliation":{"selected":total,"created":total,"updated":0,"reused":0,"failed":0,"missing":0,"conflicts":0,"mediaMissing":0}})))
    })
    .await
}
fn comment_fixture(origin: &Url, count: usize) -> (tempfile::TempDir, Cli) {
    let (dir, mut cli) = fixture(origin, 1);
    let o = origin.origin().ascii_serialization();
    let metadata = json!({"schemaVersion":"v1","repositoryFixture":false,"sourceOwner":"Local QA","sourceAuthority":"synthetic.local","sourceLocation":"generated synthetic export","approvedAt":"2026-09-10T00:00:00Z"});
    let ids = (1..=count).map(|i| format!("c{i}")).collect::<Vec<_>>();
    let user = source_key("wordpress", "wp-user-1").unwrap();
    let article = source_key("wordpress", "post-1").unwrap();
    let mut export = metadata.clone();
    export["records"] = json!(ids.iter().map(|id| json!({"sourceId":id,"sourceUrl":format!("{o}/comments/{id}"),"data":{"comment":format!("Synthetic comment {id}"),"commentedAt":"2020-01-01T00:00:00.000Z","user":{"sourceKey":user},"article":{"sourceKey":article}}})).collect::<Vec<_>>());
    let bytes = serde_json::to_vec(&export).unwrap();
    std::fs::write(dir.path().join("comments.json"), &bytes).unwrap();
    let mut m = metadata;
    m["sourceSystem"] = json!("wordpress");
    m["sourceOrigins"] = json!([o]);
    m["types"] = json!({"comment":ids});
    m["files"] = json!({"comment":{"path":"comments.json","sha256":sha256(&bytes)}});
    m["comments"] =
        json!({"mode":"existing-users","approval":"SYNTHETIC-CMT","users":{user.clone():5}});
    m["dependencies"] = json!({"article":[article]});
    m["manifestHash"] = json!(manifest_hash(&m).unwrap());
    std::fs::write(dir.path().join("manifest.json"), m.to_string()).unwrap();
    cli.source_ids = ids;
    cli.types = vec!["comment".into()];
    (dir, cli)
}
fn writes(s: &Batch) -> Vec<(String, usize)> {
    s.calls
        .iter()
        .filter(|(k, _)| k != "GET")
        .cloned()
        .collect()
}
#[tokio::test]
async fn comments_are_written_in_batches_of_100_with_per_item_checkpoint_and_idempotent_resume() {
    let state = Arc::new(Mutex::new(Batch::default()));
    let cms = batch_cms(state.clone()).await;
    let (_dir, mut cli) = comment_fixture(&cms.origin, 250);
    let report = execute(&cli).await.unwrap();
    assert!(report.passed());
    assert_eq!((report.created, report.record_requests), (250, 3));
    assert_eq!(
        writes(&state.lock().unwrap()),
        vec![
            ("batch".into(), 100),
            ("batch".into(), 100),
            ("batch".into(), 50)
        ]
    );
    let entries = checkpoint_entries(cli.checkpoint.as_ref().unwrap());
    assert_eq!(entries.len(), 250);
    assert!(entries.iter().all(|e| e["type"] == "comment"));
    cli.resume = true;
    let repeat = execute(&cli).await.unwrap();
    assert!(repeat.passed());
    assert_eq!(
        (repeat.skipped, repeat.created, repeat.record_requests),
        (250, 0, 3)
    );
    assert_eq!(state.lock().unwrap().records.len(), 250);
    // --record-batch-size bounds the batch; 1 sends one request per record.
    for (size, expected) in [(40, vec![40, 40, 20]), (1, vec![1; 5])] {
        let state = Arc::new(Mutex::new(Batch::default()));
        let cms = batch_cms(state.clone()).await;
        let (_dir, mut cli) = comment_fixture(&cms.origin, if size == 1 { 5 } else { 100 });
        cli.record_batch_size = Some(size);
        assert!(execute(&cli).await.unwrap().passed());
        let s = state.lock().unwrap();
        assert_eq!(
            writes(&s).iter().map(|(_, n)| *n).collect::<Vec<_>>(),
            expected
        );
        let kind = if size == 1 { "single" } else { "batch" };
        assert!(writes(&s).iter().all(|(k, _)| k == kind));
    }
    for bad in [0, 101] {
        let state = Arc::new(Mutex::new(Batch::default()));
        let cms = batch_cms(state.clone()).await;
        let (_dir, mut cli) = comment_fixture(&cms.origin, 3);
        cli.record_batch_size = Some(bad);
        assert_eq!(
            execute(&cli).await.err().unwrap().code,
            "record_batch_size_invalid"
        );
        assert!(!cli.checkpoint.as_ref().unwrap().exists());
        assert!(state.lock().unwrap().calls.is_empty());
    }
}
#[tokio::test]
async fn a_rejected_batch_item_fails_alone_with_its_status_and_resume_recovers_it() {
    let state = Arc::new(Mutex::new(Batch {
        reject: Some(("c7".into(), 409)),
        ..Default::default()
    }));
    let cms = batch_cms(state.clone()).await;
    let (_dir, mut cli) = comment_fixture(&cms.origin, 30);
    let failed = execute(&cli).await.unwrap();
    assert!(!failed.passed());
    assert_eq!((failed.created, failed.failed), (29, 1));
    assert_eq!(failed.failures[0].kind, "comment");
    assert_eq!(failed.failures[0].code, "http_rejected");
    assert_eq!(failed.failures[0].status, Some(409));
    assert_eq!(failed.failures[0].record_fingerprint, sha256(b"c7"));
    assert_eq!(
        checkpoint_entries(cli.checkpoint.as_ref().unwrap()).len(),
        29
    );
    cli.resume = true;
    let recovered = execute(&cli).await.unwrap();
    assert!(recovered.passed());
    assert_eq!((recovered.created, recovered.skipped), (1, 29));
}
#[tokio::test]
async fn a_lost_batch_response_is_reconciled_by_lookup_before_the_idempotent_retry() {
    let state = Arc::new(Mutex::new(Batch {
        lose_first: true,
        ..Default::default()
    }));
    let cms = batch_cms(state.clone()).await;
    let (_dir, cli) = comment_fixture(&cms.origin, 10);
    let report = execute(&cli).await.unwrap();
    assert!(report.passed());
    // Committed before the lost response: the retry reports them as reused, never duplicated.
    assert_eq!((report.skipped, report.created), (10, 0));
    let s = state.lock().unwrap();
    assert_eq!(s.records.len(), 10);
    assert_eq!(
        s.calls,
        vec![
            ("batch".into(), 10),
            ("GET".into(), 0),
            ("batch".into(), 10)
        ]
    );
}
#[tokio::test]
async fn a_cms_without_the_batch_route_or_scope_gets_single_record_requests() {
    for status in [404, 405, 403] {
        let state = Arc::new(Mutex::new(Batch {
            batch_status: Some(status),
            ..Default::default()
        }));
        let cms = batch_cms(state.clone()).await;
        let (_dir, cli) = comment_fixture(&cms.origin, 150);
        let report = execute(&cli).await.unwrap();
        assert!(report.passed(), "{status}");
        assert_eq!(report.created, 150);
        let s = state.lock().unwrap();
        // One probe, then every record (including later chunks) goes through the single route.
        assert_eq!(s.calls[0], ("batch".into(), 100));
        assert_eq!(s.calls[1..].len(), 150);
        assert!(s.calls[1..].iter().all(|(k, _)| k == "single"));
        assert_eq!(report.record_requests, 151);
        assert_eq!(
            checkpoint_entries(cli.checkpoint.as_ref().unwrap()).len(),
            150
        );
    }
}
#[tokio::test]
async fn a_malformed_batch_response_fails_every_item_and_a_resume_reconciles() {
    let state = Arc::new(Mutex::new(Batch {
        malformed_first: true,
        ..Default::default()
    }));
    let cms = batch_cms(state.clone()).await;
    let (_dir, mut cli) = comment_fixture(&cms.origin, 5);
    let failed = execute(&cli).await.unwrap();
    assert_eq!(failed.failed, 5);
    assert!(failed
        .failures
        .iter()
        .all(|f| f.code == "cms_batch_mismatch"));
    assert!(checkpoint_entries(cli.checkpoint.as_ref().unwrap()).is_empty());
    cli.resume = true;
    let recovered = execute(&cli).await.unwrap();
    assert!(recovered.passed());
    assert_eq!(recovered.skipped, 5);
}
