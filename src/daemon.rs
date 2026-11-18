use crate::{
    config::Storage,
    install::atomic_write,
    mcp, observer,
    store::{Store, prepare_dir},
};
use anyhow::{Context, Result, anyhow};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Write},
    process::{Command, Stdio},
    thread,
    time::Duration,
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

    for request in server.incoming_requests() {
        if handle(request, &store, &address.to_string(), &token) {
            break;
        }
    }
    let _ = fs::remove_file(data.directory.join(URL));
    drop(lock);
    Ok(())
}

fn handle(request: Request, store: &Store, address: &str, token: &str) -> bool {
    let (path, query) = request.url().split_once('?').unwrap_or((request.url(), ""));
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
    if request.method() != &Method::Get || path != token {
        let _ = request.respond(Response::from_string("Not found").with_status_code(404));
        return false;
    }
    let query = form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "q")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    let page = viewer(store, token, &query);
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

fn viewer(store: &Store, token: &str, query: &str) -> Result<String> {
    let rows = mcp::search(store, Some(query), None, 40)?;
    let mut html = format!(
        r#"<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>QuietMem</title>
<style>
:root{{color-scheme:dark;--bg:#15171a;--panel:#1c2024;--line:#343b42;--ink:#f0f1f2;--muted:#afb9c3;--accent:#a4dac2}}*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--ink);font:16px system-ui,sans-serif}}main{{max-width:940px;margin:0 auto;padding:48px 22px}}h1{{margin:0 0 24px;letter-spacing:-.035em;font-size:36px}}form{{display:flex;gap:10px;margin-bottom:30px}}input,button{{border:1px solid var(--line);border-radius:8px;font:inherit;padding:12px;background:var(--panel);color:var(--ink)}}input{{flex:1;min-width:0}}button{{color:var(--accent);cursor:pointer}}details{{background:var(--panel);border:1px solid var(--line);border-radius:10px;margin:0 0 10px;padding:16px}}summary{{cursor:pointer;overflow-wrap:anywhere}}small{{display:block;color:var(--muted);margin:8px 0 0;overflow-wrap:anywhere}}pre{{white-space:pre-wrap;overflow-wrap:anywhere;font:14px/1.55 ui-monospace,monospace;margin:16px 0 0}}p{{color:var(--muted)}}:focus-visible{{outline:2px solid var(--accent);outline-offset:3px}}
</style><main><h1>QuietMem</h1><form action="{token}" method="get"><input aria-label="Search memory" placeholder="Search memory" name="q" value="{}"><button>Search</button></form>"#,
        escape(query)
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

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
