//! `rayline top`: live request metrics from the local hybrid process.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use serde_json::Value;

use crate::top_view::{self, Context, Style, ViewState};

const TOP_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopRequest {
    pub json: bool,
}

pub async fn render_top(request: &TopRequest) -> io::Result<String> {
    let home = crate::home().map_err(|error| io::Error::new(io::ErrorKind::NotFound, error))?;
    render_top_from_home(&home, request).await
}

async fn render_top_from_home(home: &Path, request: &TopRequest) -> io::Result<String> {
    let state_dir = crate::hybrid::state_dir(home);
    // The live view waits for a local process and follows a restarted one.
    if !request.json && io::stdout().is_terminal() {
        run_top_tui(home, &state_dir).await?;
        return Ok(String::new());
    }
    let source = TopSource::connect(&state_dir).await?;

    if request.json {
        let snapshot = source.fetch_snapshot().await?;
        return serde_json::to_string_pretty(&snapshot)
            .map(|mut json| {
                json.push('\n');
                json
            })
            .map_err(io::Error::other);
    }

    let snapshot = source.fetch_snapshot().await?;
    let locals = Locals::load(home)?;
    Ok(top_view::text(&snapshot, &context(&locals, 0), TEXT_WIDTH))
}

/// The authenticated snapshot endpoint of one live local process, and the
/// identity every snapshot must carry.
struct TopSource {
    client: reqwest::Client,
    url: String,
    identity: Value,
}

impl TopSource {
    /// [`Self::open`], with one error for every no-local-process case (spec
    /// section 3.4 item 3). A running process top can't use keeps its own
    /// error.
    async fn connect(state_dir: &Path) -> io::Result<Self> {
        Self::open(state_dir).await.map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "no local process is running. Start one with `rayline local start` or \
                     `rayline run <harness> --mode local`.",
                )
            } else {
                error
            }
        })
    }

    /// [`Self::connect`] with `top_connection`'s error: `NotFound` when no
    /// local process runs.
    async fn open(state_dir: &Path) -> io::Result<Self> {
        let (client, url, identity) = crate::hybrid::top_connection(state_dir).await?;
        Ok(Self {
            client,
            url,
            identity,
        })
    }

    async fn fetch_snapshot(&self) -> io::Result<Value> {
        let snapshot = fetch_top_snapshot(&self.client, &self.url).await?;
        if snapshot.get("runtime").and_then(Value::as_str) != Some("rayline-local")
            || ["pid", "core_pid", "instance_id"]
                .iter()
                .any(|key| snapshot.get(key) != self.identity.get(key))
        {
            return Err(io::Error::other(
                "local process snapshot/process identity mismatch; restart top",
            ));
        }
        Ok(snapshot)
    }
}

/// What one refresh of the live view shows.
#[derive(Debug)]
enum Refresh {
    Snapshot(Value),
    NotRunning,
    /// The fetch failed. `same_instance`: the instance the last snapshot
    /// came from still runs, so that snapshot stays on screen.
    Error {
        message: String,
        same_instance: bool,
    },
}

/// One refresh of the live view. Fetches from the instance `source` holds;
/// when that fails, reads the state again and follows the instance it names
/// now (a restarted local process), and holds none while none runs.
async fn refresh(state_dir: &Path, source: &mut Option<TopSource>) -> Refresh {
    let fetch_error = match source.as_ref() {
        Some(current) => match current.fetch_snapshot().await {
            Ok(snapshot) => return Refresh::Snapshot(snapshot),
            Err(error) => Some(error.to_string()),
        },
        None => None,
    };
    let next = match TopSource::open(state_dir).await {
        Ok(next) => next,
        Err(error) => {
            *source = None;
            return if error.kind() == io::ErrorKind::NotFound {
                Refresh::NotRunning
            } else {
                Refresh::Error {
                    message: error.to_string(),
                    same_instance: false,
                }
            };
        }
    };
    if let (Some(current), Some(message)) = (source.as_ref(), fetch_error)
        && current.identity == next.identity
    {
        return Refresh::Error {
            message,
            same_instance: true,
        };
    }
    let fetched = next.fetch_snapshot().await;
    *source = Some(next);
    match fetched {
        Ok(snapshot) => Refresh::Snapshot(snapshot),
        Err(error) => Refresh::Error {
            message: error.to_string(),
            same_instance: false,
        },
    }
}

async fn fetch_top_snapshot(client: &reqwest::Client, url: &str) -> io::Result<Value> {
    client
        .get(url)
        .send()
        .await
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("local process metrics endpoint is not available: {error}"),
            )
        })?
        .error_for_status()
        .map_err(io::Error::other)?
        .json::<Value>()
        .await
        .map_err(io::Error::other)
        .and_then(|snapshot| {
            validate_top_snapshot(&snapshot)?;
            Ok(snapshot)
        })
}

/// Reject a snapshot whose shape the renderers cannot safely index. Every top
/// path fetches through `fetch_top_snapshot`, so this is the one gate. The
/// diagnostic never echoes the endpoint's body.
fn validate_top_snapshot(snapshot: &Value) -> io::Result<()> {
    let invalid = |what: &str| {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("local process metrics endpoint returned a malformed snapshot: {what}"),
        ))
    };
    let Some(object) = snapshot.as_object() else {
        return invalid("not a JSON object");
    };
    for key in ["active", "recent"] {
        match object.get(key) {
            Some(Value::Array(rows)) if rows.iter().all(Value::is_object) => {}
            _ => return invalid(&format!("`{key}` is not an array of objects")),
        }
    }
    let Some(Value::Object(totals)) = object.get("totals") else {
        return invalid("`totals` is not an object");
    };
    for key in TOTALS {
        if !totals.get(*key).is_some_and(Value::is_u64) {
            return invalid(&format!("`totals.{key}` is not a count"));
        }
    }
    Ok(())
}

/// The `totals` counts top shows.
const TOTALS: &[&str] = &[
    "completed_requests",
    "errored_requests",
    "local_requests",
    "cloud_requests",
    "input_tokens",
    "output_tokens",
];

struct TopTerminalGuard;

impl TopTerminalGuard {
    fn enter(stdout: &mut io::Stdout) -> io::Result<Self> {
        terminal::enable_raw_mode().map_err(io::Error::other)?;
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TopTerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

/// The running local process's local models, as it recorded them at start.
#[derive(Default)]
struct Locals {
    /// `local.inference.models` has a model.
    models: bool,
    /// The `local.inference.endpoints` key of each local deployment.
    endpoints: BTreeMap<String, String>,
}

impl Locals {
    /// From the running local process's record; none while none runs.
    fn load(home: &Path) -> io::Result<Self> {
        Ok(match crate::hybrid::running_endpoints(home)? {
            Some(endpoints) => Self {
                models: !endpoints.is_empty(),
                endpoints,
            },
            None => Self::default(),
        })
    }
}

/// What a frame is drawn with, at `now`.
fn context(locals: &Locals, tick: usize) -> Context<'_> {
    let now_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    Context {
        now_unix_ms,
        utc_offset_s,
        local_models: locals.models,
        endpoints: &locals.endpoints,
        on_mac: cfg!(target_os = "macos"),
        tick,
    }
}

async fn run_top_tui(home: &Path, state_dir: &Path) -> io::Result<()> {
    let mut stdout = io::stdout();
    let _guard = TopTerminalGuard::enter(&mut stdout)?;
    let mut source = None;
    // None until a snapshot is fetched, and again while no local process runs.
    let mut snapshot: Option<Value> = None;
    let mut not_running = false;
    let mut last_error: Option<String> = None;
    let mut last_fetch: Option<Instant> = None;
    let mut view = ViewState::default();
    let mut tick = 0usize;
    // The config of the instance `locals_for` names.
    let mut locals = Locals::default();
    let mut locals_for: Option<Value> = None;
    let mut needs_draw = true;

    loop {
        if last_fetch.is_none_or(|last| last.elapsed() >= TOP_REFRESH_INTERVAL) {
            match refresh(state_dir, &mut source).await {
                Refresh::Snapshot(next_snapshot) => {
                    snapshot = Some(next_snapshot);
                    not_running = false;
                    last_error = None;
                }
                Refresh::NotRunning => {
                    snapshot = None;
                    not_running = true;
                    last_error = None;
                }
                Refresh::Error {
                    message,
                    same_instance,
                } => {
                    if !same_instance {
                        snapshot = None;
                    }
                    not_running = false;
                    last_error = Some(message);
                }
            }
            if let Some(snapshot) = &snapshot {
                keep_view(snapshot, &mut view);
            }
            // A new local process may run from another config.
            let identity = source.as_ref().map(|source| source.identity.clone());
            if identity.is_some() && identity != locals_for {
                match Locals::load(home) {
                    Ok(loaded) => {
                        locals = loaded;
                        locals_for = identity;
                    }
                    Err(error) => {
                        locals = Locals::default();
                        last_error = Some(error.to_string());
                    }
                }
            }
            last_fetch = Some(Instant::now());
            tick = tick.wrapping_add(1);
            needs_draw = true;
        }

        if needs_draw {
            let mut warnings = Vec::new();
            if not_running {
                warnings.push("Local process: not running".to_owned());
            }
            if let Some(error) = &last_error {
                warnings.push(format!("last fetch error: {error}"));
            }
            let (width, height) = terminal::size().map_err(io::Error::other)?;
            let lines = top_view::frame(
                snapshot.as_ref(),
                &view,
                &context(&locals, tick),
                &warnings,
                usize::from(width),
                usize::from(height),
            );
            draw(&mut stdout, &lines)?;
            needs_draw = false;
        }

        if event::poll(Duration::from_millis(100)).map_err(io::Error::other)? {
            match event::read().map_err(io::Error::other)? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('q')
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL))
                    {
                        break;
                    }
                    if let Some(snapshot) = &snapshot {
                        needs_draw |= on_key(snapshot, &mut view, key.code);
                    }
                }
                Event::Resize(_, _) => needs_draw = true,
                _ => {}
            }
        }
    }

    Ok(())
}

/// Keeps the view on rows a new snapshot still has: a filter on a harness
/// with no requests left goes back to all requests, and a selected request
/// that left the list moves the selection to the first row.
fn keep_view(snapshot: &Value, view: &mut ViewState) {
    if let Some(harness) = &view.filter
        && !top_view::harnesses(snapshot).contains(harness)
    {
        view.filter = None;
    }
    let rows = top_view::list_rows(snapshot, view.filter.as_deref());
    let index = top_view::selected_index(&rows, view.selected.as_deref());
    let selected = index.and_then(|index| rows[index]["request_id"].as_str());
    if selected != view.selected.as_deref() {
        view.details = false;
    }
    view.selected = selected.map(str::to_owned);
}

/// Applies a key (spec section 3.4 item 7). Returns whether the view changed.
fn on_key(snapshot: &Value, view: &mut ViewState, key: KeyCode) -> bool {
    if view.details {
        if key == KeyCode::Esc {
            view.details = false;
            return true;
        }
        return false;
    }
    let rows = top_view::list_rows(snapshot, view.filter.as_deref());
    let index = top_view::selected_index(&rows, view.selected.as_deref());
    let select = |view: &mut ViewState, index: usize| {
        view.selected = rows[index]["request_id"].as_str().map(str::to_owned);
    };
    match (key, index) {
        (KeyCode::Up, Some(index)) => select(view, index.saturating_sub(1)),
        (KeyCode::Down, Some(index)) => select(view, (index + 1).min(rows.len() - 1)),
        (KeyCode::Enter, Some(_)) => view.details = true,
        (KeyCode::Char('f'), _) => {
            view.filter = top_view::next_filter(snapshot, view.filter.as_deref());
            view.selected = None;
            keep_view(snapshot, view);
        }
        _ => return false,
    }
    true
}

fn draw(stdout: &mut io::Stdout, lines: &[top_view::Line]) -> io::Result<()> {
    queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;
    for (y, line) in lines.iter().enumerate() {
        let Ok(y) = u16::try_from(y) else { break };
        queue!(stdout, MoveTo(0, y))?;
        match line.style {
            Style::Plain => queue!(stdout, Print(&line.text))?,
            Style::Title => queue!(
                stdout,
                SetAttribute(Attribute::Bold),
                Print(&line.text),
                SetAttribute(Attribute::Reset)
            )?,
            Style::ColumnNames | Style::Dim => queue!(
                stdout,
                SetForegroundColor(Color::DarkGrey),
                Print(&line.text),
                ResetColor
            )?,
            Style::Local => queue!(
                stdout,
                SetForegroundColor(Color::DarkCyan),
                Print(&line.text),
                ResetColor
            )?,
            Style::Errored => queue!(
                stdout,
                SetForegroundColor(Color::Red),
                Print(&line.text),
                ResetColor
            )?,
            Style::Warning => queue!(
                stdout,
                SetForegroundColor(Color::DarkYellow),
                Print(&line.text),
                ResetColor
            )?,
            Style::Selected => queue!(
                stdout,
                SetAttribute(Attribute::Reverse),
                Print(&line.text),
                SetAttribute(Attribute::Reset)
            )?,
        }
    }
    stdout.flush()
}

/// The width `rayline top` writes to a file or pipe.
const TEXT_WIDTH: usize = 140;

/// The local time zone's offset from UTC at `unix_ms`, in seconds; 0 when
/// the system can't say.
#[cfg(unix)]
fn utc_offset_s(unix_ms: u64) -> i64 {
    let Ok(time) = libc::time_t::try_from(unix_ms / 1000) else {
        return 0;
    };
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: `localtime_r` writes only to the `tm` it is given, which lives
    // for the call; a null return means it wrote nothing usable.
    let converted = unsafe { libc::localtime_r(&time, tm.as_mut_ptr()) };
    if converted.is_null() {
        return 0;
    }
    // SAFETY: `localtime_r` returned non-null, so it initialized `tm`.
    let offset = unsafe { tm.assume_init() }.tm_gmtoff;
    // `tm_gmtoff` is a C `long`: `i64` on 64-bit targets, `i32` on others.
    #[allow(clippy::useless_conversion)]
    i64::from(offset)
}

/// The local time zone's offset from UTC at `unix_ms`, in seconds, by the
/// zone's daylight-saving rules for that date; 0 when the system can't say.
#[cfg(windows)]
fn utc_offset_s(unix_ms: u64) -> i64 {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{
        FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime,
    };
    // FILETIME counts 100 ns from 1601-01-01 UTC.
    const UNIX_EPOCH_FILETIME: u64 = 116_444_736_000_000_000;
    let ticks =
        |time: &FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let Some(utc_ticks) = (unix_ms / 1000)
        .checked_mul(10_000_000)
        .and_then(|ticks| ticks.checked_add(UNIX_EPOCH_FILETIME))
    else {
        return 0;
    };
    let utc = FILETIME {
        dwLowDateTime: utc_ticks as u32,
        dwHighDateTime: (utc_ticks >> 32) as u32,
    };
    // SAFETY: SYSTEMTIME and FILETIME are plain data; all zeros is valid.
    let mut utc_parts: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut local_parts: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut local: FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: each call reads and writes only the valid, live values it is
    // given; a null zone means the current zone; a zero return means it
    // wrote nothing usable.
    let converted = unsafe {
        FileTimeToSystemTime(&utc, &mut utc_parts) != 0
            && SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc_parts, &mut local_parts) != 0
            && SystemTimeToFileTime(&local_parts, &mut local) != 0
    };
    if !converted {
        return 0;
    }
    let difference = i128::from(ticks(&local)) - i128::from(utc_ticks);
    i64::try_from(difference / 10_000_000).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{name}-{}-{nanos}", std::process::id()))
    }

    const JSON: TopRequest = TopRequest { json: true };

    async fn hybrid_top_fixture(
        home: &Path,
        health_override: Value,
    ) -> tokio::task::JoinHandle<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state_dir = crate::hybrid::state_dir(home);
        fs::create_dir_all(&state_dir).unwrap();
        let mut state = serde_json::json!({
            "contract_version": 1, "ready": true,
            "pid": std::process::id(), "core_pid": std::process::id(),
            "instance_id": "test-core",
            "capabilities": {"context_window": 98304, "max_output_tokens": 4096, "input_modalities": ["text"]},
            "listen_url": format!("http://{}", listener.local_addr().unwrap())
        });
        fs::write(
            state_dir.join("ready.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        fs::write(state_dir.join("gateway-token"), "test-private-token").unwrap();
        fs::write(
            state_dir.join("supervisor.json"),
            serde_json::json!({"pid": std::process::id(), "config": "/c.json", "env": null,
                "router": null})
            .to_string(),
        )
        .unwrap();
        for (key, value) in health_override.as_object().unwrap() {
            state[key] = value.clone();
        }
        tokio::spawn(async move {
            let mut snapshots = 0;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buffer = [0u8; 4096];
                let size = stream.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..size]).to_lowercase();
                let authenticated =
                    request.contains("authorization: bearer test-private-token\r\n");
                let health_request = request.starts_with("get /healthz ");
                let mut body = if health_request {
                    state.clone()
                } else if let Some(body) = state.get("snapshot_body") {
                    body.clone()
                } else {
                    serde_json::json!({"runtime": "rayline-local", "instance_id": "test-core",
                        "pid": std::process::id(), "core_pid": std::process::id(),
                        "totals": {"completed_requests": 1, "errored_requests": 0, "local_requests": 0,
                            "cloud_requests": 1, "input_tokens": 0, "output_tokens": 12}, "active": [],
                        "recent": [{"request_id": "hybrid_1", "target": "cloud", "state": "completed",
                            "executed_model": null, "requested_model": "virtual-model", "output_tokens": 12}]})
                };
                if !health_request {
                    snapshots += 1;
                    if state.get("replace_snapshot_identity") == Some(&Value::Bool(true))
                        && snapshots > 1
                    {
                        body["instance_id"] = Value::from("replacement-core");
                    }
                }
                let body = serde_json::to_vec(&body).unwrap();
                let status = if authenticated {
                    "200 OK"
                } else {
                    "401 Unauthorized"
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
            }
        })
    }

    #[tokio::test]
    async fn the_live_view_waits_for_a_local_process_and_follows_a_restarted_one() {
        let home = unique_test_dir("hybrid-top-follow");
        let state_dir = crate::hybrid::state_dir(&home);
        let mut source = None;
        assert!(matches!(
            refresh(&state_dir, &mut source).await,
            Refresh::NotRunning
        ));

        let first = hybrid_top_fixture(&home, serde_json::json!({})).await;
        let Refresh::Snapshot(snapshot) = refresh(&state_dir, &mut source).await else {
            panic!("the first instance");
        };
        assert_eq!(snapshot["instance_id"], "test-core");

        // A restart: the old instance is gone and the state names another.
        first.abort();
        let second = hybrid_top_fixture(
            &home,
            serde_json::json!({"instance_id": "second-core", "snapshot_body": {
                "runtime": "rayline-local", "instance_id": "second-core",
                "pid": std::process::id(), "core_pid": std::process::id(),
                "totals": {"completed_requests": 0, "errored_requests": 0, "local_requests": 0,
                    "cloud_requests": 0, "input_tokens": 0, "output_tokens": 0},
                "active": [], "recent": []}}),
        )
        .await;
        let ready_path = state_dir.join("ready.json");
        let mut ready: Value = serde_json::from_slice(&fs::read(&ready_path).unwrap()).unwrap();
        ready["instance_id"] = Value::from("second-core");
        fs::write(&ready_path, serde_json::to_vec(&ready).unwrap()).unwrap();
        let Refresh::Snapshot(snapshot) = refresh(&state_dir, &mut source).await else {
            panic!("the restarted instance");
        };
        assert_eq!(snapshot["instance_id"], "second-core");
        assert_eq!(snapshot["totals"]["completed_requests"], 0);

        // A running instance top can't authenticate to is an error, not
        // "not running".
        let token_path = state_dir.join("gateway-token");
        fs::write(&token_path, "wrong-private-token").unwrap();
        let mut fresh = None;
        let Refresh::Error {
            message,
            same_instance: false,
        } = refresh(&state_dir, &mut fresh).await
        else {
            panic!("an error on a running instance");
        };
        assert!(message.contains("health/authentication"), "{message}");
        assert!(!message.contains("private-token"), "{message}");
        fs::write(&token_path, "test-private-token").unwrap();

        // Stopped: the state is gone, and the view waits.
        second.abort();
        fs::remove_file(&ready_path).unwrap();
        assert!(matches!(
            refresh(&state_dir, &mut source).await,
            Refresh::NotRunning
        ));
        assert!(source.is_none());
    }

    #[tokio::test]
    async fn top_authenticates_and_preserves_unknown_execution() {
        let home = unique_test_dir("hybrid-top");
        let server = hybrid_top_fixture(&home, serde_json::json!({})).await;
        let rendered = render_top_from_home(&home, &JSON).await.unwrap();
        let snapshot: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(snapshot["runtime"], "rayline-local");
        assert_eq!(snapshot["recent"][0]["executed_model"], Value::Null);
        assert_eq!(snapshot["recent"][0]["output_tokens"], 12);
        assert_eq!(snapshot["recent"][0]["target"], "cloud");

        fs::write(
            crate::hybrid::state_dir(&home).join("gateway-token"),
            "wrong-private-token",
        )
        .unwrap();
        let error = render_top_from_home(&home, &JSON)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("health/authentication"));
        assert!(!error.contains("private-token"));
        server.abort();

        let server =
            hybrid_top_fixture(&home, serde_json::json!({"instance_id": "different-core"})).await;
        assert!(
            render_top_from_home(&home, &JSON)
                .await
                .unwrap_err()
                .to_string()
                .contains("identity")
        );
        server.abort();

        let server = hybrid_top_fixture(
            &home,
            serde_json::json!({"replace_snapshot_identity": true}),
        )
        .await;
        let source = TopSource::connect(&crate::hybrid::state_dir(&home))
            .await
            .unwrap();
        assert!(source.fetch_snapshot().await.is_ok());
        assert!(
            source
                .fetch_snapshot()
                .await
                .unwrap_err()
                .to_string()
                .contains("snapshot/process identity mismatch")
        );
        server.abort();

        let ready_path = crate::hybrid::state_dir(&home).join("ready.json");
        let original: Value = serde_json::from_slice(&fs::read(&ready_path).unwrap()).unwrap();
        for patch in [
            serde_json::json!({"ready": false}),
            serde_json::json!({"pid": 0}),
            serde_json::json!({"core_pid": 0}),
            serde_json::json!({"listen_url": "http://private-token@127.0.0.1:1234"}),
            serde_json::json!({"listen_url": "http://example.com:1234"}),
        ] {
            let mut state = original.clone();
            for (key, value) in patch.as_object().unwrap() {
                state[key] = value.clone();
            }
            fs::write(&ready_path, serde_json::to_vec(&state).unwrap()).unwrap();
            let error = render_top_from_home(&home, &JSON)
                .await
                .unwrap_err()
                .to_string();
            // A live supervisor with unusable readiness is an error; one
            // whose `ready.json` names an exited process (pid 0) is none.
            assert!(
                error.contains("readiness")
                    || error.contains("not live")
                    || error.starts_with("no local process is running. Start one"),
                "{error}"
            );
            assert!(!error.contains("private-token"));
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn top_without_a_local_process_is_an_error() {
        let home = unique_test_dir("top-no-process");
        let error = render_top_from_home(&home, &JSON).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(
            error.to_string(),
            "no local process is running. Start one with `rayline local start` or \
             `rayline run <harness> --mode local`."
        );
    }

    /// State a stopped or crashed process left behind is no local process,
    /// with the same error as no state at all; a running process with
    /// state top can't read keeps its own error.
    #[tokio::test]
    async fn top_with_stale_state_is_the_no_process_error() {
        let no_process = |error: &io::Error| {
            assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
            assert_eq!(
                error.to_string(),
                "no local process is running. Start one with `rayline local start` or \
             `rayline run <harness> --mode local`."
            );
        };
        let home = unique_test_dir("top-stale-state");
        let state_dir = crate::hybrid::state_dir(&home);
        fs::create_dir_all(&state_dir).unwrap();
        let ready = state_dir.join("ready.json");
        let supervisor = state_dir.join("supervisor.json");
        // State without a supervisor record.
        for state in [
            "{}",
            "not json",
            r#"{"contract_version": 1, "ready": false}"#,
        ] {
            fs::write(&ready, state).unwrap();
            no_process(&render_top_from_home(&home, &JSON).await.unwrap_err());
        }
        // A complete record of a process that has exited.
        let server = hybrid_top_fixture(&home, serde_json::json!({})).await;
        server.abort();
        let mut exited = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        exited.wait().unwrap();
        let mut record: Value = serde_json::from_slice(&fs::read(&supervisor).unwrap()).unwrap();
        record["pid"] = Value::from(exited.id());
        fs::write(&supervisor, record.to_string()).unwrap();
        no_process(&render_top_from_home(&home, &JSON).await.unwrap_err());
        let mut live = None;
        assert!(matches!(
            refresh(&state_dir, &mut live).await,
            Refresh::NotRunning
        ));
        // A running process whose `ready.json` top can't read.
        record["pid"] = Value::from(std::process::id());
        fs::write(&supervisor, record.to_string()).unwrap();
        let mut state: Value = serde_json::from_slice(&fs::read(&ready).unwrap()).unwrap();
        state.as_object_mut().unwrap().remove("capabilities");
        fs::write(&ready, state.to_string()).unwrap();
        let error = render_top_from_home(&home, &JSON)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("readiness"), "{error}");
        fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn malformed_snapshots_are_errors_never_panics() {
        let totals = serde_json::json!({"completed_requests": 0, "errored_requests": 0,
            "local_requests": 0, "cloud_requests": 0, "input_tokens": 0, "output_tokens": 0});
        let valid = serde_json::json!({"runtime": "rayline-local", "pid": std::process::id(),
            "core_pid": std::process::id(), "instance_id": "test-core",
            "active": [], "recent": [], "totals": totals});
        validate_top_snapshot(&valid).unwrap();
        let with = |key: &str, value: Value| {
            let mut body = valid.clone();
            body[key] = value;
            body
        };
        let without = |key: &str| {
            let mut body = valid.clone();
            body.as_object_mut().unwrap().remove(key);
            body
        };
        let mut partial_totals = totals.clone();
        partial_totals
            .as_object_mut()
            .unwrap()
            .remove("errored_requests");
        for body in [
            serde_json::json!([]),
            serde_json::json!(0),
            serde_json::json!("secret-body-marker"),
            with("active", serde_json::json!([1])),
            with("recent", serde_json::json!("x")),
            with("totals", serde_json::json!(3)),
            without("active"),
            without("recent"),
            without("totals"),
            with("totals", partial_totals),
            with("totals", serde_json::json!({"completed_requests": -1})),
        ] {
            let home = unique_test_dir("top-malformed");
            let server =
                hybrid_top_fixture(&home, serde_json::json!({ "snapshot_body": body })).await;
            let error = render_top_from_home(&home, &JSON).await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{body}");
            assert!(!error.to_string().contains("secret-body-marker"));
            server.abort();
            fs::remove_dir_all(&home).ok();
        }
    }

    /// The keys of spec section 3.4 item 7, and a refresh that drops the
    /// selected request or the filtered harness.
    #[test]
    fn keys_select_open_details_and_cycle_the_filter() {
        let row = |id: &str, harness: &str, started: u64| {
            serde_json::json!({"request_id": id, "harness": harness,
                "started_at_unix_ms": started, "completed_at_unix_ms": started})
        };
        let snapshot = serde_json::json!({"active": [], "recent": [
            row("a", "claude", 3), row("b", "codex", 2), row("c", "claude", 1)
        ]});
        let mut view = ViewState::default();
        keep_view(&snapshot, &mut view);
        assert_eq!(view.selected.as_deref(), Some("a"));
        assert!(on_key(&snapshot, &mut view, KeyCode::Down));
        assert!(on_key(&snapshot, &mut view, KeyCode::Down));
        assert!(on_key(&snapshot, &mut view, KeyCode::Down));
        assert_eq!(view.selected.as_deref(), Some("c"));
        assert!(on_key(&snapshot, &mut view, KeyCode::Up));
        assert_eq!(view.selected.as_deref(), Some("b"));
        assert!(on_key(&snapshot, &mut view, KeyCode::Enter));
        assert!(view.details);
        // In the details view only Esc acts.
        assert!(!on_key(&snapshot, &mut view, KeyCode::Down));
        assert!(on_key(&snapshot, &mut view, KeyCode::Esc));
        assert!(!view.details);

        for expected in [Some("claude"), Some("codex"), None] {
            assert!(on_key(&snapshot, &mut view, KeyCode::Char('f')));
            assert_eq!(view.filter.as_deref(), expected);
        }
        on_key(&snapshot, &mut view, KeyCode::Char('f'));
        assert_eq!(view.selected.as_deref(), Some("a"));

        // The harness and the request left the list: all requests, first row.
        view.details = true;
        let later = serde_json::json!({"active": [], "recent": [row("d", "codex", 4)]});
        keep_view(&later, &mut view);
        assert_eq!(view.filter, None);
        assert_eq!(view.selected.as_deref(), Some("d"));
        assert!(!view.details);
    }
}
