use crate::{
    config::Storage,
    embed::{self, Embedder, Index, Scope},
    install::atomic_write,
    mcp, observer,
    store::{Store, prepare_dir},
};
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, SecondsFormat};
use fs2::FileExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tiny_http::{Header, Method, Request, Response, Server};

const LOCK: &str = "daemon.lock";
const URL: &str = "viewer-url";

fn lock_file(data: &Storage) -> Result<File> {
    prepare_dir(&data.directory)?;
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.directory.join(LOCK))?)
}

fn try_lock(file: &File) -> Result<bool> {
    match FileExt::try_lock_exclusive(file) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub fn start_background(data: &Storage) -> Result<()> {
    if std::env::var_os("MIB_NO_AUTOSTART").is_some() {
        return Ok(());
    }
    let lock = lock_file(data)?;
    if try_lock(&lock)? {
        drop(lock);
        let mut command = Command::new(std::env::current_exe()?);
        if let Some(dir) = data.directory_override() {
            command.arg("--data-dir").arg(dir);
        }
        command
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
    }
    Ok(())
}

pub fn status(data: &Storage) -> Result<Value> {
    let lock = lock_file(data)?;
    let running = !try_lock(&lock)?;
    drop(lock);
    let url = running
        .then(|| fs::read_to_string(data.directory.join(URL)).ok())
        .flatten();
    let store = Store::open(data).context("opening the Mem in Black database")?;
    Ok(json!({
        "data_dir": data.directory, "database_path": data.database, "running": running, "viewer_url": url.as_deref().map(str::trim),
        "journal_mode": store.journal_mode()?,
        "records": store.counts().context("reading Mem in Black records")?,
        "observer_tokens": store.metric("observer_tokens")?,
        "summaries": match crate::summaries::load(&data.directory) {
            Ok(settings) => json!({"enabled": settings.enabled, "provider": settings.provider, "model": settings.model}),
            Err(error) => json!({"config_error": format!("{error:#}")}),
        },
        "context_injections": store.metric("context_injections")?,
        "context_bytes": store.metric("context_bytes")?
    }))
}

pub fn open_viewer(data: &Storage) -> Result<()> {
    start_background(data).context("starting the Mem in Black worker")?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(250)))
        .proxy(None)
        .max_redirects(0)
        .build()
        .into();
    let deadline = Instant::now() + Duration::from_secs(5);
    let url = loop {
        if let Ok(value) = fs::read_to_string(data.directory.join(URL))
            && let Ok(uri) = value.trim().parse::<ureq::http::Uri>()
            && uri.scheme_str() == Some("http")
            && uri.host() == Some("127.0.0.1")
            && agent
                .get(&uri)
                .call()
                .is_ok_and(|response| response.status().is_success())
        {
            break uri;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the viewer did not become ready. Check `mib status` or run `mib serve`."
        );
        thread::sleep(Duration::from_millis(50));
    };
    // Printed first, so the URL is at hand even when no browser opens.
    println!("{url}");
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let exit = Command::new(opener)
        .arg(url.to_string())
        .status()
        .with_context(|| format!("could not run {opener}. Open the URL above in a browser."))?;
    anyhow::ensure!(
        exit.success(),
        "{opener} failed ({exit}). Open the URL above in a browser."
    );
    Ok(())
}

pub fn stop(data: &Storage) -> Result<()> {
    let lock = lock_file(data)?;
    if try_lock(&lock)? {
        return Ok(());
    }
    let base = fs::read_to_string(data.directory.join(URL))
        .context("worker viewer address is not ready")?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .proxy(None)
        .build()
        .into();
    agent.post(format!("{}/stop", base.trim())).send_empty()?;
    Ok(())
}

pub fn observe_once(data: &Storage) -> Result<()> {
    let lock = lock_file(data)?;
    anyhow::ensure!(
        try_lock(&lock)?,
        "background worker is already processing observations"
    );
    let store = Store::open_worker(data)?;
    let settings = crate::summaries::load(&data.directory)?;
    let refresh = store.summary_refresh_bound()?;
    while observer::process_one(&store, &data.directory, &settings, refresh)? {}
    while observer::judge_batch(&store)? {}
    while crate::consolidate::batch(&store, &data.directory, &settings)? {}
    Ok(())
}

/// The worker is a background helper: keep its model inference to a couple
/// of cores and below interactive work. `RAYON_NUM_THREADS` or
/// `CANDLE_NUM_THREADS` override the thread count.
fn run_in_background() {
    const THREADS: &str = "2";
    if std::env::var_os("RAYON_NUM_THREADS").is_none()
        && std::env::var_os("CANDLE_NUM_THREADS").is_none()
    {
        // SAFETY: called at worker start, before this process creates any thread.
        unsafe {
            std::env::set_var("RAYON_NUM_THREADS", THREADS);
            std::env::set_var("CANDLE_NUM_THREADS", THREADS);
        }
    }
    // Threads created afterwards inherit the lower priority.
    unsafe {
        let current = libc::getpriority(libc::PRIO_PROCESS, 0);
        libc::setpriority(libc::PRIO_PROCESS, 0, current.max(10));
    }
}

pub fn run(data: &Storage) -> Result<()> {
    let lock = lock_file(data)?;
    if !try_lock(&lock)? {
        return Ok(());
    }
    run_in_background();
    let store = Store::open_worker(data)?;
    // Start any summary rebuild before the viewer serves its first revision.
    let refresh = store.summary_refresh_bound()?;
    // Invalid summary settings fall back to local summaries.
    let settings = crate::summaries::load(&data.directory).unwrap_or_else(|error| {
        eprintln!("Mem in Black summary settings: {error:#}");
        crate::summaries::Settings {
            enabled: false,
            ..Default::default()
        }
    });
    let server = Server::http("127.0.0.1:0").map_err(|error| anyhow!(error))?;
    let address = server
        .server_addr()
        .to_ip()
        .context("expected a local socket")?;
    let token = format!("/{}", uuid::Uuid::new_v4().simple());
    let url = format!("http://{address}{token}");
    atomic_write(&data.directory.join(URL), url.as_bytes())?;
    println!("{url}");
    io::stdout().flush()?;

    let worker_data = data.clone();
    thread::spawn(move || {
        let Ok(store) = Store::open(&worker_data) else {
            return;
        };
        // Recalls are judged and projects consolidated only when no summary is
        // waiting, each at most every 30 seconds.
        let mut judged = None::<Instant>;
        let mut consolidated = None::<Instant>;
        loop {
            match observer::process_one(&store, &worker_data.directory, &settings, refresh) {
                Ok(true) => {}
                Ok(false) => {
                    if judged.is_none_or(|at| at.elapsed() >= Duration::from_secs(30)) {
                        judged = Some(Instant::now());
                        if observer::judge_batch(&store).unwrap_or(false) {
                            judged = None;
                            continue;
                        }
                    }
                    if consolidated.is_none_or(|at| at.elapsed() >= Duration::from_secs(30)) {
                        consolidated = Some(Instant::now());
                        if crate::consolidate::batch(&store, &worker_data.directory, &settings)
                            .unwrap_or(false)
                        {
                            consolidated = None;
                            continue;
                        }
                    }
                    thread::sleep(Duration::from_secs(2));
                }
                Err(_) => thread::sleep(Duration::from_secs(5)),
            }
        }
    });

    let semantic = Arc::new(Semantic::default());
    if let Some(spec) = embed::configured(&data.directory).unwrap_or_else(|error| {
        eprintln!("Mem in Black embeddings configuration: {error:#}");
        None
    }) {
        let semantic = semantic.clone();
        let worker_data = data.clone();
        thread::spawn(move || {
            if let Err(error) = semantic.run(&worker_data, spec) {
                eprintln!("Mem in Black embeddings: {error:#}");
            }
        });
    }

    for request in server.incoming_requests() {
        if handle(request, &store, &semantic, &address.to_string(), &token) {
            break;
        }
    }
    let _ = fs::remove_file(data.directory.join(URL));
    drop(lock);
    Ok(())
}

/// The embedding model and vector index the worker keeps in memory.
#[derive(Default)]
pub struct Semantic {
    embedder: Mutex<Option<Embedder>>,
    index: RwLock<Index>,
    ready: AtomicBool,
    revision: AtomicU64,
    waiting_queries: AtomicUsize,
    /// Milliseconds since the Unix epoch of the last query.
    last_query: AtomicU64,
    /// A query found the model unloaded; load it again.
    wanted: AtomicBool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// How long the model stays loaded without queries. `MIB_MODEL_IDLE_SECS`
/// overrides the 30-minute default.
fn idle_limit() -> Duration {
    std::env::var("MIB_MODEL_IDLE_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(Duration::from_secs(30 * 60), Duration::from_secs)
}

/// Return freed heap pages to the operating system after unloading the model.
fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // malloc_trim only releases free memory held by the allocator.
    unsafe {
        libc::malloc_trim(0);
    }
}

impl Semantic {
    fn run(&self, data: &Storage, spec: &'static embed::Spec) -> Result<()> {
        let store = Store::open(data)?;
        let idle = idle_limit();
        let load = || -> Result<()> {
            let embedder = Embedder::load(&data.directory, spec)?;
            *self.embedder.lock().unwrap_or_else(|e| e.into_inner()) = Some(embedder);
            self.revision.fetch_add(1, Ordering::Release);
            self.last_query.store(now_ms(), Ordering::Release);
            // Requests that arrived during the load are served now.
            self.wanted.store(false, Ordering::Release);
            Ok(())
        };
        load()?;
        let mut rebuilt = Instant::now() - Duration::from_secs(3600);
        loop {
            // Reload from storage now and then so forgotten records leave the index.
            if rebuilt.elapsed() > Duration::from_secs(600) {
                let mut index = Index::default();
                for (id, workspace, project, vector) in store.vectors(&spec.vector_key())? {
                    index.insert(id, workspace, project, embed::from_bytes(&vector));
                }
                *self.index.write().unwrap_or_else(|e| e.into_inner()) = index;
                self.ready.store(true, Ordering::Release);
                self.revision.fetch_add(1, Ordering::Release);
                rebuilt = Instant::now();
            }
            let loaded = self
                .embedder
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some();
            if !loaded {
                // New records wait until a query brings the model back.
                if self.wanted.swap(false, Ordering::AcqRel) {
                    load()?;
                } else {
                    thread::sleep(Duration::from_millis(100));
                }
                continue;
            }
            let idle_now = || {
                let quiet = now_ms().saturating_sub(self.last_query.load(Ordering::Acquire));
                Duration::from_millis(quiet) >= idle
            };
            if idle_now() {
                *self.embedder.lock().unwrap_or_else(|e| e.into_inner()) = None;
                release_memory();
                continue;
            }
            let batch = store.unembedded(&spec.vector_key(), 16)?;
            if batch.is_empty() {
                thread::sleep(Duration::from_millis(500));
                continue;
            }
            // One record per lock, and queries go first: a waiting query is
            // delayed by at most one record, never by a whole batch.
            for (id, workspace, project, body) in batch {
                if idle_now() {
                    break;
                }
                while self.waiting_queries.load(Ordering::Acquire) > 0 {
                    thread::sleep(Duration::from_millis(5));
                }
                let vector = {
                    let guard = self.embedder.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(embedder) = guard.as_ref() else {
                        break;
                    };
                    embedder
                        .embed_documents(&[crate::hooks::cut(&body, 4000)])?
                        .remove(0)
                };
                store.save_vectors(&spec.vector_key(), &[(id, embed::to_bytes(&vector))])?;
                self.index
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, workspace, project, vector);
                self.revision.fetch_add(1, Ordering::Release);
            }
        }
    }

    fn search(
        &self,
        store: &Store,
        query: &str,
        scope: &Scope<'_>,
        limit: usize,
        memories_only: bool,
    ) -> Result<Option<Vec<(i64, f32)>>> {
        if !self.ready.load(Ordering::Acquire) {
            return Ok(None);
        }
        self.last_query.store(now_ms(), Ordering::Release);
        self.waiting_queries.fetch_add(1, Ordering::AcqRel);
        let vector = {
            let guard = self.embedder.lock().unwrap_or_else(|e| e.into_inner());
            self.waiting_queries.fetch_sub(1, Ordering::AcqRel);
            let Some(embedder) = guard.as_ref() else {
                // Answer now with keywords only; the model loads for the next query.
                self.wanted.store(true, Ordering::Release);
                return Ok(None);
            };
            embedder.embed_query(query)?
        };
        let allowed = memories_only.then(|| store.memory_ids()).transpose()?;
        Ok(Some(
            self.index
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .search_filtered(&vector, scope, limit, allowed.as_ref()),
        ))
    }
}

/// Ask the running worker for semantic matches. `None` when the worker or the
/// model is unavailable, so callers fall back to keyword search.
pub fn semantic_search(
    data: &Storage,
    query: &str,
    scope: &Scope<'_>,
    limit: usize,
    timeout: Duration,
    memories_only: bool,
) -> Option<Vec<(i64, f32)>> {
    let base = fs::read_to_string(data.directory.join(URL)).ok()?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .proxy(None)
        .max_redirects(0)
        .build()
        .into();
    let (workspace, project) = match scope {
        Scope::All => (None, None),
        Scope::Workspace(root) => (Some(*root), None),
        Scope::Project(project) => (None, Some(*project)),
    };
    let mut response = agent
        .post(format!("{}/semantic", base.trim()))
        .send_json(
            json!({"query": query, "workspace": workspace, "project": project, "limit": limit, "memories_only": memories_only}),
        )
        .ok()?;
    let value: Value = response
        .body_mut()
        .with_config()
        .limit(1 << 20)
        .read_json()
        .ok()?;
    let hits = value["hits"].as_array()?;
    Some(
        hits.iter()
            .filter_map(|hit| Some((hit[0].as_i64()?, hit[1].as_f64()? as f32)))
            .collect(),
    )
}

fn semantic_response(
    store: &Store,
    semantic: &Semantic,
    request: &mut Request,
) -> Response<io::Cursor<Vec<u8>>> {
    let mut body = String::new();
    let parsed = request
        .as_reader()
        .take(64 * 1024)
        .read_to_string(&mut body)
        .ok()
        .and_then(|_| serde_json::from_str::<Value>(&body).ok());
    let Some(input) = parsed else {
        return Response::from_string("Bad request").with_status_code(400);
    };
    let query = input["query"].as_str().unwrap_or_default();
    let scope = match (input["workspace"].as_str(), input["project"].as_str()) {
        (Some(root), _) => Scope::Workspace(root),
        (None, Some(project)) => Scope::Project(project),
        _ => Scope::All,
    };
    let limit = input["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
    let memories_only = input["memories_only"].as_bool().unwrap_or_default();
    match semantic.search(store, query, &scope, limit, memories_only) {
        Ok(Some(hits)) => Response::from_string(json!({"hits": hits}).to_string()),
        Ok(None) => Response::from_string("Not ready").with_status_code(503),
        Err(_) => Response::from_string("Embedding failed").with_status_code(500),
    }
}

fn handle(
    mut request: Request,
    store: &Store,
    semantic: &Semantic,
    address: &str,
    token: &str,
) -> bool {
    let url = request.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let host = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Host"))
        .map(|header| header.value.as_str());
    if host != Some(address) {
        let _ = request.respond(Response::from_string("Not found").with_status_code(404));
        return false;
    }
    if request.method() == &Method::Post && path == format!("{token}/stop") {
        let _ = request.respond(Response::from_string("Stopped"));
        return true;
    }
    if request.method() == &Method::Post && path == format!("{token}/semantic") {
        let response = semantic_response(store, semantic, &mut request);
        let _ = request.respond(response);
        return false;
    }
    if request.method() == &Method::Get && path == format!("{token}/live.js") {
        let response = Response::from_string(include_str!("viewer.js")).with_header(
            Header::from_bytes("Content-Type", "text/javascript; charset=utf-8").unwrap(),
        );
        let _ = request.respond(response);
        return false;
    }
    if request.method() != &Method::Get || path != token {
        let _ = request.respond(Response::from_string("Not found").with_status_code(404));
        return false;
    }
    let view = ViewerQuery::parse(query);
    let mut response = viewer_response(store, semantic, token, &view, &request)
        .unwrap_or_else(|_| Response::from_string("Unable to load memory").with_status_code(500));
    for (name, value) in [
        ("Content-Type", "text/html; charset=utf-8"),
        ("Cache-Control", "no-store"),
        ("Referrer-Policy", "no-referrer"),
        (
            "Content-Security-Policy",
            "default-src 'none'; style-src 'unsafe-inline'; script-src 'self'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
    ] {
        if let Ok(header) = Header::from_bytes(name, value) {
            response.add_header(header);
        }
    }
    let _ = request.respond(response);
    false
}

#[derive(Default)]
struct ViewerQuery {
    text: String,
    activity: bool,
    facts: bool,
    evidence: Option<i64>,
    record: Option<i64>,
}

impl ViewerQuery {
    fn parse(query: &str) -> Self {
        let mut view = Self::default();
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "q" => view.text = value.into_owned(),
                "view" => {
                    view.activity = value == "activity";
                    view.facts = value == "facts";
                }
                "evidence" => view.evidence = value.parse().ok(),
                "record" => view.record = value.parse().ok(),
                _ => {}
            }
        }
        view.activity |= view.evidence.is_some();
        view
    }
}

fn viewer_response(
    store: &Store,
    semantic: &Semantic,
    token: &str,
    view: &ViewerQuery,
    request: &Request,
) -> Result<Response<io::Cursor<Vec<u8>>>> {
    // Read the version before rendering so a concurrent commit triggers the next refresh.
    let data = store.data_version()?;
    let embeddings = semantic.revision.load(Ordering::Acquire);
    let tag = format!(r#""{data}-{embeddings}""#);
    let unchanged = request
        .headers()
        .iter()
        .any(|header| header.field.equiv("If-None-Match") && header.value.as_str() == tag);
    let response = if unchanged {
        Response::from_string("").with_status_code(304)
    } else {
        Response::from_string(viewer(store, semantic, token, view)?)
    };
    Ok(response.with_header(Header::from_bytes("ETag", tag).unwrap()))
}

fn viewer(store: &Store, semantic: &Semantic, token: &str, view: &ViewerQuery) -> Result<String> {
    let find = |text: &str, limit: usize| {
        semantic
            .search(store, text, &Scope::All, limit, !view.activity)
            .ok()
            .flatten()
            .map(|hits| hits.into_iter().map(|(id, _)| id).collect())
    };
    let rows = if let Some(id) = view.evidence {
        source_evidence(store, id)?
    } else if let Some(id) = view.record {
        store.live(&[id])?
    } else if view.facts {
        // Facts hold no keyword index of their own: the few there are are filtered here.
        let needle = view.text.trim().to_lowercase();
        store
            .facts(None)?
            .into_iter()
            .map(|(_, record)| record)
            .filter(|record| needle.is_empty() || record.body.to_lowercase().contains(&needle))
            .take(40)
            .collect()
    } else {
        mcp::search_filtered(
            store,
            Some(&view.text),
            None,
            40,
            Some(&find),
            !view.activity,
        )?
    };
    let current = if view.activity {
        Some("activity")
    } else if view.facts {
        Some("facts")
    } else {
        None
    };
    let mode_input = current
        .map(|mode| format!(r#"<input type="hidden" name="view" value="{mode}">"#))
        .unwrap_or_default();
    let view_tabs = [
        ("Memories", None),
        ("Activity", Some("activity")),
        ("Project facts", Some("facts")),
    ]
    .into_iter()
    .map(|(label, mode)| {
        let mut query = form_urlencoded::Serializer::new(String::new());
        if let Some(mode) = mode {
            query.append_pair("view", mode);
        }
        if !view.text.is_empty() {
            query.append_pair("q", &view.text);
        }
        let query = query.finish();
        let link = if query.is_empty() {
            token.to_owned()
        } else {
            format!("{token}?{query}")
        };
        let selected = if mode == current {
            r#" aria-current="page""#
        } else {
            ""
        };
        format!(r#"<a href="{}"{selected}>{label}</a>"#, escape(&link))
    })
    .collect::<String>();
    let evidence_heading = view
        .evidence
        .map(|id| format!("<h2>Source evidence for #{id}</h2>"))
        .or_else(|| view.record.map(|id| format!("<h2>Record #{id}</h2>")))
        .unwrap_or_default();
    let mut html = format!(
        r#"<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Mem in Black</title>
<script src="{token}/live.js" defer></script>
<style>
:root{{color-scheme:dark;--bg:#15171a;--panel:#1c2024;--line:#343b42;--ink:#f0f1f2;--muted:#afb9c3;--accent:#a4dac2}}*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--ink);font:16px system-ui,sans-serif}}main{{max-width:940px;margin:0 auto;padding:48px 22px}}h1{{margin:0;letter-spacing:-.035em;font-size:36px}}form{{display:flex;gap:10px;margin-bottom:30px}}input,button{{border:1px solid var(--line);border-radius:8px;font:inherit;padding:12px;background:var(--panel);color:var(--ink)}}input{{flex:1;min-width:0}}button{{color:var(--accent);cursor:pointer}}details{{background:var(--panel);border:1px solid var(--line);border-radius:10px;margin:0 0 10px;padding:16px}}summary{{cursor:pointer;overflow-wrap:anywhere}}small{{display:block;color:var(--muted);margin:8px 0 0;overflow-wrap:anywhere}}pre{{white-space:pre-wrap;overflow-wrap:anywhere;font:14px/1.55 ui-monospace,monospace;margin:16px 0 0}}p{{color:var(--muted)}}:focus-visible{{outline:2px solid var(--accent);outline-offset:3px}}
.viewer-heading{{display:flex;align-items:center;justify-content:space-between;flex-wrap:wrap;gap:8px 16px;margin:0 0 24px}}#viewer-status{{font-size:13px;color:var(--muted)}}#viewer-status[data-connected="true"]{{color:var(--accent)}}time{{display:block;margin-top:4px;color:var(--muted);font-size:13px;font-variant-numeric:tabular-nums}}
#viewer-activity details{{background:hsl(var(--project-hue) 36% 17%);border-color:hsl(var(--project-hue) 32% 33%)}}
#viewer-activity summary{{position:relative;list-style:none;padding-inline-start:1.15em}}
#viewer-activity summary::-webkit-details-marker{{display:none}}
#viewer-activity summary::before{{content:"";position:absolute;inset-inline-start:.1em;inset-block-start:.43em;border-block:.28em solid transparent;border-inline-start:.42em solid var(--muted)}}
#viewer-activity details[open]>summary::before{{transform:rotate(90deg)}}
.record-heading{{display:flex;align-items:flex-start;justify-content:space-between;gap:8px 16px}}
.record-title{{min-width:0}}.record-tags{{display:flex;flex:none;flex-wrap:wrap;justify-content:flex-end;gap:4px;max-width:48%}}
.project-label,.agent-label{{max-width:100%;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;border-radius:5px;padding:3px 7px;font-size:12px;font-weight:600;line-height:1.3}}
.project-label{{background:hsl(var(--project-hue) 35% 26%);color:hsl(var(--project-hue) 72% 87%)}}
.agent-label{{background:rgb(0 0 0 / .2);box-shadow:inset 0 0 0 1px rgb(255 255 255 / .24)}}
.stats{{display:grid;grid-template-columns:repeat(auto-fit,minmax(210px,1fr));gap:10px;margin:0 0 16px}}.stat{{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:14px}}.stat b{{display:block;font-size:24px;letter-spacing:-.02em;margin:4px 0 6px;font-variant-numeric:tabular-nums}}.stat small{{display:inline;margin:0;font-size:13px;font-weight:500;letter-spacing:0;color:var(--muted)}}.stat span,.stat em{{color:var(--muted);font-size:13px;font-style:normal}}
#viewer-recall{{margin:0 0 28px;padding:14px 16px;border:1px solid var(--line);border-radius:10px}}#viewer-recall h2{{font-size:15px;margin:0}}#viewer-recall p,#viewer-recall ul{{font-size:14px;margin:8px 0 0}}#viewer-recall ul{{padding:0;list-style:none}}#viewer-recall li+li{{border-top:1px solid var(--line);margin-top:8px;padding-top:8px}}#viewer-recall li{{overflow-wrap:anywhere}}
a{{color:var(--accent)}}nav{{display:flex;gap:8px;margin-bottom:12px}}nav a{{padding:8px 12px;border:1px solid var(--line);border-radius:8px;color:var(--muted);text-decoration:none}}nav a[aria-current="page"]{{background:var(--panel);color:var(--ink);border-color:var(--accent)}}.memory-preview{{display:block;margin-top:6px;color:var(--muted);font-size:14px;line-height:1.45}}details[open] .memory-preview{{display:none}}
.markdown{{margin:16px 0 0;line-height:1.55;overflow-wrap:anywhere}}.markdown>:first-child{{margin-top:0}}.markdown>:last-child{{margin-bottom:0}}.markdown p{{color:inherit;margin:0 0 .75em}}.markdown h1,.markdown h2,.markdown h3,.markdown h4,.markdown h5,.markdown h6{{font-size:1em;margin:1em 0 .5em}}.markdown ul,.markdown ol{{margin:0 0 .75em;padding-inline-start:1.4em}}.markdown pre{{margin:0 0 .75em;padding:12px;background:var(--bg);border:1px solid var(--line);border-radius:8px;white-space:pre;overflow-x:auto}}code{{font:.9em ui-monospace,monospace}}:not(pre)>code{{padding:1px 5px;border-radius:5px;background:rgb(255 255 255 / .08)}}.markdown blockquote{{margin:0 0 .75em;padding-inline-start:12px;border-inline-start:3px solid var(--line);color:var(--muted)}}.markdown table{{border-collapse:collapse;margin:0 0 .75em;display:block;overflow-x:auto}}.markdown th,.markdown td{{border:1px solid var(--line);padding:6px 10px;text-align:start}}.source-link{{display:inline-block;margin-top:12px;font-size:14px}}
</style><main><header class="viewer-heading"><h1>Mem in Black</h1><span id="viewer-status" role="status" hidden></span></header>{stats}<nav aria-label="Memory views">{view_tabs}</nav><form action="{token}" method="get">{mode_input}<input aria-label="Search memory" placeholder="Search memory" name="q" value="{}"><button>Search</button></form>{evidence_heading}<div id="viewer-activity">"#,
        escape(&view.text),
        stats = stat_cards(store, token)?,
    );
    if rows.is_empty() {
        let message = if view.evidence.is_some() {
            "No source records are available for this memory."
        } else if view.record.is_some() {
            "This record is no longer available."
        } else if !view.text.trim().is_empty() {
            "No records match this search."
        } else if view.facts {
            "Project facts appear once a project has 20 new work summaries."
        } else if view.activity {
            "Captured activity will appear here."
        } else {
            "Work summaries and saved notes will appear here."
        };
        html.push_str(&format!(r#"<p id="activity-empty">{message}</p>"#));
    }
    for item in rows {
        let heading = record_heading(&item);
        let preview = if view.activity {
            String::new()
        } else {
            memory_preview(&item)
        };
        let source_link = if !view.activity && !evidence_ids(&item).is_empty() {
            format!(
                r#"<a class="source-link" href="{token}?view=activity&amp;evidence={}">Source evidence</a>"#,
                item.id
            )
        } else {
            String::new()
        };
        let project = if item.project.is_empty() {
            &item.workspace
        } else {
            &item.project
        };
        let name = escape(project_name(project));
        let hue = project_hue(project);
        let agent = escape(agent_name(&item.source));
        let time = timestamp(item.created_at);
        let open = if view.record.is_some() { " open" } else { "" };
        let badges = if view.activity {
            String::new()
        } else {
            status_badges(store, &item)?
        };
        html.push_str(&format!(
            r#"<details id="record-{id}" style="--project-hue:{hue:.6}"{open}>
<summary><span class="record-heading"><span class="record-title">{heading}</span><span class="record-tags">
<span class="project-label" title="{name}">{name}</span><span class="agent-label" title="{agent}">{agent}</span>{badges}
</span></span>{preview}{time}</summary>
<small>#{id} · {kind} · {workspace}</small>{body}{source_link}</details>"#,
            id = item.id,
            heading = inline_markdown(crate::hooks::cut(heading, 165)),
            kind = escape(&item.kind),
            workspace = escape(&item.workspace),
            // Tool records hold command output, which is not Markdown.
            body = if item.kind == "tool" {
                format!("<pre>{}</pre>", escape(&item.body))
            } else {
                format!(r#"<div class="markdown">{}</div>"#, markdown(&item.body))
            },
        ));
    }
    html.push_str("</div></main></html>");
    Ok(html)
}

/// Badges for a summary that records an abandoned approach or was later
/// reverted or disputed.
fn status_badges(store: &Store, record: &crate::store::Record) -> Result<String> {
    if record.kind != "summary" {
        return Ok(String::new());
    }
    let mut labels = Vec::new();
    if crate::distill::summary_fact(&record.body, "Abandoned: ").is_some() {
        labels.push("abandoned");
    }
    if let Some((status, _)) = store.summary_statuses(&[record.id])?.get(&record.id) {
        labels.push(if status == "reverted" {
            "reverted"
        } else {
            "disputed"
        });
    }
    Ok(labels
        .into_iter()
        .map(|label| format!(r#"<span class="agent-label status-label">{label}</span>"#))
        .collect())
}

fn memory_preview(record: &crate::store::Record) -> String {
    if record.kind != "summary" {
        return String::new();
    }
    let preview = [
        "Outcome: ",
        "Model summary: ",
        "Decision: ",
        "learned: ",
        "Latest agent update: ",
    ]
    .into_iter()
    .find_map(|prefix| {
        record
            .body
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
    })
    .unwrap_or_default();
    let mut html = String::new();
    for (label, value) in [
        ("", Some(preview)),
        (
            "Abandoned: ",
            crate::distill::summary_fact(&record.body, "Abandoned: "),
        ),
        (
            "Checks: ",
            crate::distill::summary_fact(&record.body, "Checks: "),
        ),
        (
            "Next: ",
            crate::distill::summary_fact(&record.body, "Next: "),
        ),
    ] {
        if let Some(value) = value.filter(|text| !text.is_empty()) {
            html.push_str(&format!(
                r#"<span class="memory-preview">{label}{}</span>"#,
                inline_markdown(crate::hooks::cut(value, 240))
            ));
        }
    }
    html
}

fn record_heading(record: &crate::store::Record) -> &str {
    let mut lines = record.body.lines();
    let first = lines.next().unwrap_or_default();
    if record.kind == "summary" && first.starts_with("Evidence [#") {
        return lines
            .find_map(|line| line.strip_prefix("Request: "))
            .filter(|request| !request.is_empty())
            .unwrap_or("Work summary");
    }
    if record.kind == "summary" && first.starts_with("Model summary. Evidence [#") {
        return lines
            .find(|line| !line.trim().is_empty())
            .unwrap_or("Work summary");
    }
    first.strip_prefix("title: ").unwrap_or(first)
}

fn evidence_ids(record: &crate::store::Record) -> Vec<i64> {
    if record.kind != "summary" || record.source == "claude-mem" {
        return Vec::new();
    }
    let mut lines = record.body.lines();
    let first = lines.next().unwrap_or_default();
    first
        .strip_prefix("Evidence ")
        .or_else(|| first.strip_prefix("Model summary. Evidence "))
        .or_else(|| {
            lines
                .next_back()
                .and_then(|line| line.strip_prefix("Evidence "))
        })
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|part| {
            part.strip_prefix("[#")?
                .strip_suffix(']')?
                .parse::<i64>()
                .ok()
        })
        .collect()
}

fn source_evidence(store: &Store, id: i64) -> Result<Vec<crate::store::Record>> {
    let Some(summary) = store.live(&[id])?.pop() else {
        return Ok(Vec::new());
    };
    let mut rows = store.live(&evidence_ids(&summary))?;
    rows.retain(|row| {
        row.id < summary.id
            && row.source == summary.source
            && row.session == summary.session
            && row.workspace == summary.workspace
            && row.turn_id == summary.turn_id
            && matches!(row.kind.as_str(), "prompt" | "tool" | "assistant")
    });
    Ok(rows)
}

fn project_name(project: &str) -> &str {
    let path = Path::new(project.strip_prefix("claude-mem:").unwrap_or(project));
    let path = if path.ends_with(".git") {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("No project")
}

fn project_hue(project: &str) -> f64 {
    let hash = Sha256::digest(project.as_bytes());
    let seed = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]);
    f64::from(seed) * 360.0 / (f64::from(u32::MAX) + 1.0)
}

fn agent_name(source: &str) -> &str {
    match source {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "pi" => "Pi",
        "omp" => "Oh My Pi",
        "claude-mem" => "Claude-Mem",
        "manual" => "Manual note",
        "" => "Unknown source",
        source => source,
    }
}

fn timestamp(seconds: i64) -> String {
    DateTime::from_timestamp(seconds, 0)
        .map(|date| date.to_rfc3339_opts(SecondsFormat::Secs, true))
        .map(|date| format!(r#"<time datetime="{date}" title="{date}">{date}</time>"#))
        .unwrap_or_default()
}

fn stat_cards(store: &Store, token: &str) -> Result<String> {
    let stats = store.stats(None)?;
    let mut html = String::from(r#"<section id="viewer-stats" class="stats" aria-label="Usage">"#);
    for line in crate::stats::lines(&stats).into_iter().take(3) {
        html.push_str(&format!(
            "<div class=\"stat\"><span>{}</span><b>{} <small>{}</small></b><em>{}</em></div>",
            escape(line.label),
            escape(&line.value),
            escape(line.unit),
            escape(&line.detail)
        ));
    }
    html.push_str(r#"</section><section id="viewer-recall" aria-labelledby="recall-heading"><h2 id="recall-heading">Recent recall</h2>"#);
    let deliveries = store.recent_deliveries()?;
    if deliveries.is_empty() {
        html.push_str("<p>Records will appear here when MIB adds them to an agent’s context.</p>");
    } else {
        html.push_str("<ul>");
        for delivery in deliveries {
            let record = delivery.record;
            html.push_str(&format!(
                r#"<li>{} received <a href="{token}?record={}">{}</a>{}</li>"#,
                escape(agent_name(&delivery.recipient)),
                record.id,
                escape(crate::hooks::cut(record_heading(&record), 140)),
                timestamp(delivery.delivered_at),
            ));
        }
        html.push_str("</ul>");
    }
    html.push_str("</section>");
    Ok(html)
}

const MARKDOWN: pulldown_cmark::Options = pulldown_cmark::Options::ENABLE_TABLES
    .union(pulldown_cmark::Options::ENABLE_STRIKETHROUGH)
    .union(pulldown_cmark::Options::ENABLE_TASKLISTS);

/// Renders stored Markdown as HTML. The text comes from agents and tools, so
/// raw HTML is shown as code, images as their alt text, and only web and mail
/// links stay clickable. Single line breaks are kept, as agents write them.
fn markdown(text: &str) -> String {
    use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd, html};
    let text = repair_markdown(text);
    let mut dropped_link = false;
    let events = Parser::new_ext(&text, MARKDOWN).filter_map(|event| match event {
        Event::Start(Tag::HtmlBlock) => Some(Event::Start(Tag::CodeBlock(CodeBlockKind::Indented))),
        Event::End(TagEnd::HtmlBlock) => Some(Event::End(TagEnd::CodeBlock)),
        Event::Html(raw) | Event::InlineHtml(raw) => Some(Event::Text(raw)),
        Event::SoftBreak => Some(Event::HardBreak),
        Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => None,
        Event::Start(Tag::Link { ref dest_url, .. }) if !safe_link(dest_url) => {
            dropped_link = true;
            None
        }
        Event::End(TagEnd::Link) if dropped_link => {
            dropped_link = false;
            None
        }
        other => Some(other),
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

/// Renders the inline Markdown of a one-line heading or preview: code,
/// emphasis and strikethrough. Block structure and links become plain text.
fn inline_markdown(text: &str) -> String {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let text = repair_markdown(text);
    let mut out = String::new();
    for event in Parser::new_ext(&text, MARKDOWN) {
        match event {
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                out.push_str(&escape(&text))
            }
            Event::Code(code) => out.push_str(&format!("<code>{}</code>", escape(&code))),
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            Event::Start(Tag::Emphasis) => out.push_str("<em>"),
            Event::End(TagEnd::Emphasis) => out.push_str("</em>"),
            Event::Start(Tag::Strong) => out.push_str("<strong>"),
            Event::End(TagEnd::Strong) => out.push_str("</strong>"),
            Event::Start(Tag::Strikethrough) => out.push_str("<s>"),
            Event::End(TagEnd::Strikethrough) => out.push_str("</s>"),
            _ => {}
        }
    }
    out
}

/// Stored text is often shortened: a long reply keeps its start and end around
/// a `…` line, and a heading or preview is cut at a length. A cut can leave a
/// code block, a code span or a bold marker open, and the rest of the record
/// then renders as code or shows raw markers. This closes or drops them.
fn repair_markdown(text: &str) -> String {
    const DIVIDER: &str = "\n…\n";
    let is_fence = |line: &str| {
        let line = line.trim_start();
        line.starts_with("```") || line.starts_with("~~~")
    };
    let parts: Vec<&str> = text.split(DIVIDER).collect();
    let mut out = String::with_capacity(text.len() + 16);
    for (index, part) in parts.iter().enumerate() {
        let unbalanced = part.lines().filter(|line| is_fence(line)).count() % 2 == 1;
        // A complete reply has balanced fences, so an odd count means the cut
        // ended the start inside a block, or began the end inside one.
        let mut in_code = index > 0 && unbalanced;
        if index > 0 {
            out.push_str("\n\n…\n\n");
            if in_code {
                out.push_str("```\n");
            }
        }
        for (number, line) in part.split_inclusive('\n').enumerate() {
            if is_fence(line) {
                in_code = !in_code;
                out.push_str(line);
            } else if in_code {
                out.push_str(line);
            } else {
                // The end of a reply can begin inside a code span.
                out.push_str(&repair_inline(line, index > 0 && number == 0));
            }
        }
        if in_code {
            out.push_str("\n```");
        }
    }
    out
}

/// Closes a code span and drops bold markers that a cut left unpaired. When
/// the line begins where a cut was made, an unpaired span opens at its start.
fn repair_inline(line: &str, cut_before: bool) -> String {
    let (body, newline) = match line.strip_suffix('\n') {
        Some(body) => (body, "\n"),
        None => (line, ""),
    };
    let mut body = body.to_owned();
    if body.matches("**").count() % 2 == 1 {
        body = body.replace("**", "");
    }
    if body.matches('`').count() % 2 == 1 {
        if cut_before {
            body.insert(0, '`');
        } else {
            body.push('`');
        }
    }
    body + newline
}

fn safe_link(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    ["https://", "http://", "mailto:"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::{inline_markdown, markdown};

    #[test]
    fn markdown_renders_agent_formatting() {
        let html = markdown(
            "Pulled **4** commits into `main`:\nsecond line\n\n```\n9e7a1f7 feat: x\n```\n\n- one\n- two\n\n| a | b |\n| - | - |\n| 1 | 2 |",
        );
        assert!(html.contains("<strong>4</strong>"), "{html}");
        assert!(html.contains("<code>main</code>"), "{html}");
        assert!(html.contains("<br />"), "{html}");
        assert!(
            html.contains("<pre><code>9e7a1f7 feat: x\n</code></pre>"),
            "{html}"
        );
        assert!(html.contains("<li>one</li>"), "{html}");
        assert!(html.contains("<table>"), "{html}");
    }

    #[test]
    fn markdown_cannot_inject_html_scripts_or_remote_images() {
        let html = markdown(
            "<script>alert(1)</script>\n\nInline <img src=x onerror=alert(1)> text\n\n[bad](javascript:alert(1)) [data](data:text/html,x) [ok](https://example.com/a?b=\"c\") ![pixel](https://tracker.example/p.png)",
        );
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("javascript:"), "{html}");
        assert!(!html.contains("href=\"data:"), "{html}");
        assert!(html.contains("bad"), "{html}");
        assert!(
            html.contains("href=\"https://example.com/a?b=%22c%22\""),
            "{html}"
        );
        assert!(
            html.contains("pixel") && !html.contains("tracker.example"),
            "{html}"
        );
    }

    #[test]
    fn shortened_text_does_not_leave_markdown_open() {
        let html = markdown(
            "Outcome: it shows a bar:\n```\nDownloading\n\n…\nclean.\n```\n\nCommitted as `473b328`.",
        );
        assert!(html.contains("<code>473b328</code>"), "{html}");
        assert_eq!(html.matches("<pre>").count(), 2, "{html}");
        assert!(html.contains("<p>…</p>"), "{html}");
        let html = markdown("See `notes\n…\nscratchpad/recovered.db` for it.");
        assert!(html.contains("<code>notes</code>"), "{html}");
        assert!(
            html.contains("<code>scratchpad/recovered.db</code> for it."),
            "{html}"
        );
        let html = markdown("Decision: One test fix.** A test failed.\nIt's on `cesar/precise-rec");
        assert!(!html.contains("**"), "{html}");
        assert!(html.contains("<code>cesar/precise-rec</code>"), "{html}");
        assert_eq!(
            inline_markdown("I reinstalled it with `make inst"),
            "I reinstalled it with <code>make inst</code>"
        );
    }

    #[test]
    fn inline_markdown_keeps_headings_on_one_line() {
        assert_eq!(
            inline_markdown("Fix **recall** in `hooks.rs` <b>now</b>"),
            "Fix <strong>recall</strong> in <code>hooks.rs</code> &lt;b&gt;now&lt;/b&gt;"
        );
        assert_eq!(
            inline_markdown("[docs](javascript:x) and *more*"),
            "docs and <em>more</em>"
        );
    }
}
