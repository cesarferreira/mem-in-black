use crate::{
    config::Storage,
    embed::{self, Embedder, Index, Scope},
    install::atomic_write,
    mcp, observer,
    store::{Store, prepare_dir},
};
use anyhow::{Context, Result, anyhow};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Read, Write},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
    if std::env::var_os("QUIETMEM_NO_AUTOSTART").is_some() {
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
    let store = Store::open(data).context("opening the QuietMem database")?;
    Ok(json!({
        "data_dir": data.directory, "database_path": data.database, "running": running, "viewer_url": url.as_deref().map(str::trim),
        "journal_mode": store.journal_mode()?,
        "records": store.counts().context("reading QuietMem records")?,
        "observer_tokens": store.metric("observer_tokens")?,
        "observer_config_error": observer::settings(&data.directory).err().map(|error| error.to_string()),
        "context_injections": store.metric("context_injections")?,
        "context_bytes": store.metric("context_bytes")?
    }))
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
    let config = observer::settings(&data.directory)?;
    while observer::process_one(&store, config.as_ref())? {}
    Ok(())
}

pub fn run(data: &Storage) -> Result<()> {
    let lock = lock_file(data)?;
    if !try_lock(&lock)? {
        return Ok(());
    }
    let store = Store::open_worker(data)?;
    let config = observer::settings(&data.directory).unwrap_or_else(|error| {
        eprintln!("QuietMem observer configuration: {error:#}");
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
        eprintln!("QuietMem embeddings configuration: {error:#}");
        None
    }) {
        let semantic = semantic.clone();
        let worker_data = data.clone();
        thread::spawn(move || {
            if let Err(error) = semantic.run(&worker_data, spec) {
                eprintln!("QuietMem embeddings: {error:#}");
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
    waiting_queries: AtomicUsize,
}

impl Semantic {
    fn run(&self, data: &Storage, spec: &'static embed::Spec) -> Result<()> {
        let store = Store::open(data)?;
        let embedder = Embedder::load(&data.directory, spec)?;
        *self.embedder.lock().unwrap_or_else(|e| e.into_inner()) = Some(embedder);
        let mut rebuilt = Instant::now() - Duration::from_secs(3600);
        loop {
            // Reload from storage now and then so forgotten records leave the index.
            if rebuilt.elapsed() > Duration::from_secs(600) {
                let mut index = Index::default();
                for (id, workspace, project, vector) in store.vectors(spec.name)? {
                    index.insert(id, workspace, project, embed::from_bytes(&vector));
                }
                *self.index.write().unwrap_or_else(|e| e.into_inner()) = index;
                self.ready.store(true, Ordering::Release);
                rebuilt = Instant::now();
            }
            // Queries go first; small batches bound how long one can wait.
            while self.waiting_queries.load(Ordering::Acquire) > 0 {
                thread::sleep(Duration::from_millis(10));
            }
            let batch = store.unembedded(spec.name, 4)?;
            if batch.is_empty() {
                thread::sleep(Duration::from_secs(2));
                continue;
            }
            let texts: Vec<_> = batch
                .iter()
                .map(|(_, _, _, body)| crate::hooks::cut(body, 4000))
                .collect();
            let vectors = {
                let guard = self.embedder.lock().unwrap_or_else(|e| e.into_inner());
                guard
                    .as_ref()
                    .context("model unloaded")?
                    .embed_documents(&texts)?
            };
            store.save_vectors(
                spec.name,
                &batch
                    .iter()
                    .zip(&vectors)
                    .map(|((id, ..), vector)| (*id, embed::to_bytes(vector)))
                    .collect::<Vec<_>>(),
            )?;
            let mut index = self.index.write().unwrap_or_else(|e| e.into_inner());
            for ((id, workspace, project, _), vector) in batch.into_iter().zip(vectors) {
                index.insert(id, workspace, project, vector);
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
        self.waiting_queries.fetch_add(1, Ordering::AcqRel);
        let vector = {
            let guard = self.embedder.lock().unwrap_or_else(|e| e.into_inner());
            self.waiting_queries.fetch_sub(1, Ordering::AcqRel);
            let Some(embedder) = guard.as_ref() else {
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
    if request.method() != &Method::Get || path != token {
        let _ = request.respond(Response::from_string("Not found").with_status_code(404));
        return false;
    }
    let query = form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "q")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    let page = viewer(store, semantic, token, &query);
    let mut response = match page {
        Ok(html) => Response::from_string(html),
        Err(_) => Response::from_string("Unable to load memory").with_status_code(500),
    };
    for (name, value) in [
        ("Content-Type", "text/html; charset=utf-8"),
        ("Cache-Control", "no-store"),
        ("Referrer-Policy", "no-referrer"),
        (
            "Content-Security-Policy",
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
    ] {
        if let Ok(header) = Header::from_bytes(name, value) {
            response.add_header(header);
        }
    }
    let _ = request.respond(response);
    false
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
        r#"<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>QuietMem</title>
<style>
:root{{color-scheme:dark;--bg:#15171a;--panel:#1c2024;--line:#343b42;--ink:#f0f1f2;--muted:#afb9c3;--accent:#a4dac2}}*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--ink);font:16px system-ui,sans-serif}}main{{max-width:940px;margin:0 auto;padding:48px 22px}}h1{{margin:0 0 24px;letter-spacing:-.035em;font-size:36px}}form{{display:flex;gap:10px;margin-bottom:30px}}input,button{{border:1px solid var(--line);border-radius:8px;font:inherit;padding:12px;background:var(--panel);color:var(--ink)}}input{{flex:1;min-width:0}}button{{color:var(--accent);cursor:pointer}}details{{background:var(--panel);border:1px solid var(--line);border-radius:10px;margin:0 0 10px;padding:16px}}summary{{cursor:pointer;overflow-wrap:anywhere}}small{{display:block;color:var(--muted);margin:8px 0 0;overflow-wrap:anywhere}}pre{{white-space:pre-wrap;overflow-wrap:anywhere;font:14px/1.55 ui-monospace,monospace;margin:16px 0 0}}p{{color:var(--muted)}}:focus-visible{{outline:2px solid var(--accent);outline-offset:3px}}
.stats{{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:10px;margin:0 0 28px}}.stat{{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:14px}}.stat b{{display:block;font-size:24px;letter-spacing:-.02em;margin:4px 0 6px;font-variant-numeric:tabular-nums}}.stat small{{display:inline;margin:0;font-size:13px;font-weight:500;letter-spacing:0;color:var(--muted)}}.stat span,.stat em{{color:var(--muted);font-size:13px;font-style:normal}}.note{{font-size:13px;margin:-16px 0 28px}}
</style><main><h1>QuietMem</h1>{stats}<form action="{token}" method="get"><input aria-label="Search memory" placeholder="Search memory" name="q" value="{}"><button>Search</button></form>"#,
        escape(query),
        stats = stat_cards(store)?,
    );
    if rows.is_empty() {
        let message = if query.trim().is_empty() {
            "Captured work will appear here."
        } else {
            "No observations match this search."
        };
        html.push_str(&format!("<p>{message}</p>"));
    }
    for item in rows {
        let heading = item.body.lines().next().unwrap_or_default();
        html.push_str(&format!(
            "<details><summary>{}</summary><small>#{} · {} · {} · {}</small><pre>{}</pre></details>",
            escape(crate::hooks::cut(heading, 165)), item.id, escape(&item.source),
            escape(&item.kind), escape(&item.workspace), escape(&item.body)
        ));
    }
    html.push_str("</main></html>");
    Ok(html)
}

fn stat_cards(store: &Store) -> Result<String> {
    let stats = store.stats(None)?;
    let mut html = String::from(r#"<section class="stats" aria-label="Usage">"#);
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
