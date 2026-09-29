//! The background updater against a local kit server: a kit is added on
//! a bundle, the publisher ships a new version, and the updater applies
//! what changed, announces what is new, and never runs twice at once.

mod common;

use std::sync::{Arc, Mutex};

use common::synth_kit;
use crewkit_core::kits::{self, Auth, KitRegistry, KitSource};
use crewkit_core::updater::{self, Event, ItemRef, Trigger, UpdateState};
use crewkit_core::Paths;

type Routes = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// A file server whose routes the test swaps between "releases".
fn serve(routes: Routes) -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buffer = [0u8; 2048];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
            let body = routes
                .lock()
                .unwrap()
                .iter()
                .find(|(route, _)| *route == path)
                .map(|(_, body)| body.clone());
            let response = match body {
                Some(body) => {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    r.extend(body);
                    r
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            let _ = stream.write_all(&response);
        }
    });
    port
}

fn release(
    secret: &str,
    public: &str,
    version: &str,
    plugins: serde_json::Value,
    bundle_plugins: &[&str],
) -> Vec<(String, Vec<u8>)> {
    let manifest = serde_json::json!({
        "id": "team-kit",
        "name": "Team Kit",
        "version": version,
        "publisher": "Test",
        "publisherKey": public,
        "marketplaceName": "teamkit",
        "bundles": [{ "id": "team", "plugins": bundle_plugins, "mcpServers": ["team-mcp"] }],
        "mcpServers": [{ "id": "team-mcp", "url": "https://mcp.example.dev/mcp" }],
        "plugins": plugins,
    });
    let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    let signature = kits::sign_manifest(&bytes, secret).unwrap();
    vec![
        ("/kit.json".into(), bytes),
        ("/kit.json.sig".into(), signature.into_bytes()),
    ]
}

#[test]
fn updater_applies_new_releases_and_announces_new_items() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(tmp.path());
    let crewkit_dir = paths.crewkit_dir();
    let (_, zips) = synth_kit(tmp.path());
    let zip = std::fs::read(zips.join("notes.zip")).unwrap();
    let sha = kits::sha256_hex(&zip);
    let (secret, public) = kits::generate_keypair();
    let artifact = |name: &str, version: &str| {
        serde_json::json!({
            "name": name, "version": version,
            "artifact": { "url": "/payload.zip", "sha256": sha },
        })
    };

    let v1 = release(
        &secret,
        &public,
        "1.0.0",
        serde_json::json!([
            artifact("notes", "1.0.0"),
            artifact("legacy", "1.0.0"),
            artifact("extra", "1.0.0")
        ]),
        &["notes", "legacy"],
    );
    let routes: Routes = Arc::new(Mutex::new(v1));
    routes
        .lock()
        .unwrap()
        .push(("/payload.zip".into(), zip.clone()));
    let port = serve(Arc::clone(&routes));
    let url = format!("http://127.0.0.1:{port}/kit.json");

    let source = KitSource {
        id: "team-kit".into(),
        source: url.clone(),
        channel: "stable".into(),
        pinned_key: Some(public.clone()),
        bundle: Some("team".into()),
    };
    kits::refresh(&source, &crewkit_dir, Auth::Silent).unwrap();
    KitRegistry {
        kits: vec![source.clone()],
    }
    .save(&crewkit_dir)
    .unwrap();

    // Startup pass: same release as the cache — nothing to apply.
    let report = updater::run(&paths, Trigger::Startup, None, |_| {})
        .unwrap()
        .expect("startup is never throttled");
    assert_eq!(report.kits.len(), 1);
    assert!(report.kits[0].error.is_none(), "{:?}", report.kits[0].error);
    assert!(report.kits[0].diff.updated.is_empty() && report.kits[0].diff.added.is_empty());
    let state = UpdateState::load(&crewkit_dir).unwrap();
    assert!(state.last_check_unix > 0);
    assert!(state.notifications.is_empty());

    // The hourly trigger right after a check is throttled without a fetch.
    assert!(updater::run(&paths, Trigger::Hourly, None, |_| {})
        .unwrap()
        .is_none());

    // The publisher ships 1.1.0: notes bumped, extra joins the bundle,
    // legacy retired.
    *routes.lock().unwrap() = release(
        &secret,
        &public,
        "1.1.0",
        serde_json::json!([
            artifact("notes", "1.1.0"),
            artifact("extra", "1.0.0"),
            { "name": "legacy", "remove": true },
        ]),
        &["notes", "extra"],
    );
    routes.lock().unwrap().push(("/payload.zip".into(), zip));

    let held = updater::lock(&crewkit_dir).unwrap();
    assert!(updater::run(&paths, Trigger::Manual, None, |_| {})
        .unwrap()
        .is_none());
    drop(held);

    let report = updater::run(&paths, Trigger::Manual, None, |_| {})
        .unwrap()
        .unwrap();
    let kit = &report.kits[0];
    assert!(kit.error.is_none(), "{:?}", kit.error);
    assert_eq!(kit.diff.updated.len(), 1);
    assert_eq!(kit.diff.updated[0].item, ItemRef::plugin("notes@teamkit"));
    assert_eq!(kit.diff.added, vec![ItemRef::plugin("extra@teamkit")]);
    assert_eq!(kit.diff.removed, vec![ItemRef::plugin("legacy@teamkit")]);

    let state = UpdateState::load(&crewkit_dir).unwrap();
    assert_eq!(
        state.new_items["team-kit"],
        vec![ItemRef::plugin("extra@teamkit")]
    );
    assert!(state.notifications.iter().any(|n| matches!(
        &n.event,
        Event::NewItems { kit, items } if kit == "team-kit" && items.len() == 1
    )));
    let cached = kits::load_cached(&crewkit_dir, &source).unwrap();
    assert_eq!(cached.version.as_deref(), Some("1.1.0"));
    assert!(!cached
        .plugins
        .iter()
        .any(|p| p.name == "legacy" && !p.remove));
}
