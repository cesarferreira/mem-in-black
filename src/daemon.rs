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
        "observer_config_error": observer::settings(&data.directory).err().map(|error| error.to_string()),
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
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let exit = Command::new(opener)
        .arg(url.to_string())
        .status()
        .with_context(|| format!("could not run {opener}. Run `mib status` for the viewer URL."))?;
    anyhow::ensure!(
        exit.success(),
        "{opener} failed ({exit}). Run `mib status` for the viewer URL."
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

/// Stops the worker and waits until it releases the data directory.
/// Returns false if the worker is still running after `timeout`.
pub fn stop_and_wait(data: &Storage, timeout: Duration) -> bool {
    let _ = stop(data);
    let deadline = Instant::now() + timeout;
    loop {
        match lock_file(data).and_then(|lock| try_lock(&lock)) {
            Ok(true) => return true,
            Ok(false) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => return false,
        }
    }
}

pub fn observe_once(data: &Storage) -> Result<()> {
    let lock = lock_file(data)?;
    anyhow::ensure!(
        try_lock(&lock)?,
        "background worker is already processing observations"
    );
    let store = Store::open_worker(data)?;
    let config = observer::settings(&data.directory)?;
    while observer::process_one(&store, config.as_ref())? {}
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
    let config = observer::settings(&data.directory).unwrap_or_else(|error| {
        eprintln!("Mem in Black observer configuration: {error:#}");
        None
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
        loop {
            match observer::process_one(&store, config.as_ref()) {
                Ok(true) => {}
                Ok(false) => thread::sleep(Duration::from_secs(2)),
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
        query: &str,
        scope: &Scope<'_>,
        limit: usize,
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
        Ok(Some(
            self.index
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .search(&vector, scope, limit),
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
            json!({"query": query, "workspace": workspace, "project": project, "limit": limit}),
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

fn semantic_response(semantic: &Semantic, request: &mut Request) -> Response<io::Cursor<Vec<u8>>> {
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
    match semantic.search(query, &scope, limit) {
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
        let response = semantic_response(semantic, &mut request);
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
    let query = form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "q")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    let mut response = viewer_response(store, semantic, token, &query, &request)
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

fn viewer_response(
    store: &Store,
    semantic: &Semantic,
    token: &str,
    query: &str,
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
        Response::from_string(viewer(store, semantic, token, query)?)
    };
    Ok(response.with_header(Header::from_bytes("ETag", tag).unwrap()))
}

fn viewer(store: &Store, semantic: &Semantic, token: &str, query: &str) -> Result<String> {
    let find = |text: &str, limit: usize| {
        semantic
            .search(text, &Scope::All, limit)
            .ok()
            .flatten()
            .map(|hits| hits.into_iter().map(|(id, _)| id).collect())
    };
    let rows = mcp::search(store, Some(query), None, 40, Some(&find))?;
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
.stats{{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:10px;margin:0 0 28px}}.stat{{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:14px}}.stat b{{display:block;font-size:24px;letter-spacing:-.02em;margin:4px 0 6px;font-variant-numeric:tabular-nums}}.stat small{{display:inline;margin:0;font-size:13px;font-weight:500;letter-spacing:0;color:var(--muted)}}.stat span,.stat em{{color:var(--muted);font-size:13px;font-style:normal}}.note{{font-size:13px;margin:-16px 0 28px}}
</style><main><header class="viewer-heading"><h1>Mem in Black</h1><span id="viewer-status" role="status" hidden></span></header>{stats}<form action="{token}" method="get"><input aria-label="Search memory" placeholder="Search memory" name="q" value="{}"><button>Search</button></form><div id="viewer-activity">"#,
        escape(query),
        stats = stat_cards(store)?,
    );
    if rows.is_empty() {
        let message = if query.trim().is_empty() {
            "Captured work will appear here."
        } else {
            "No observations match this search."
        };
        html.push_str(&format!(r#"<p id="activity-empty">{message}</p>"#));
    }
    for item in rows {
        let heading = item.body.lines().next().unwrap_or_default();
        let project = if item.project.is_empty() {
            &item.workspace
        } else {
            &item.project
        };
        let name = escape(project_name(project));
        let hue = project_hue(project);
        let agent = escape(match item.source.as_str() {
            "claude" => "Claude Code",
            "codex" => "Codex",
            "opencode" => "OpenCode",
            "pi" => "Pi",
            "omp" => "Oh My Pi",
            "claude-mem" => "Claude-Mem",
            "manual" => "Manual note",
            "" => "Unknown source",
            source => source,
        });
        let time = DateTime::from_timestamp(item.created_at, 0)
            .map(|date| date.to_rfc3339_opts(SecondsFormat::Secs, true))
            .map(|date| format!(r#"<time datetime="{date}" title="{date}">{date}</time>"#))
            .unwrap_or_default();
        html.push_str(&format!(
            r#"<details id="record-{id}" style="--project-hue:{hue:.6}">
<summary><span class="record-heading"><span class="record-title">{heading}</span><span class="record-tags">
<span class="project-label" title="{name}">{name}</span><span class="agent-label" title="{agent}">{agent}</span>
</span></span>{time}</summary>
<small>#{id} · {kind} · {workspace}</small><pre>{body}</pre></details>"#,
            id = item.id,
            heading = escape(crate::hooks::cut(heading, 165)),
            kind = escape(&item.kind),
            workspace = escape(&item.workspace),
            body = escape(&item.body),
        ));
    }
    html.push_str("</div></main></html>");
    Ok(html)
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

fn stat_cards(store: &Store) -> Result<String> {
    let stats = store.stats(None)?;
    let mut html = String::from(r#"<section id="viewer-stats" class="stats" aria-label="Usage">"#);
    for line in crate::stats::lines(&stats) {
        html.push_str(&format!(
            "<div class=\"stat\"><span>{}</span><b>{} <small>{}</small></b><em>{}</em></div>",
            escape(line.label),
            escape(&line.value),
            escape(line.unit),
            escape(&line.detail)
        ));
    }
    html.push_str(
        "</section><p class=\"note\">Tokens are estimated at 4 bytes each. Savings are an upper bound.</p>",
    );
    Ok(html)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
