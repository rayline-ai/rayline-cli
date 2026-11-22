use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::claude_daemon::{LaunchPreflight, LaunchRecord, PreflightOutcome, RequestSpec};
use serde_json::Value;

/// Claude Code's model for subagents that name no model of their own
/// (`CLAUDE_CODE_SUBAGENT_MODEL`).
const SUBAGENT_MODEL_ENV: &str = "CLAUDE_CODE_SUBAGENT_MODEL";
/// Makes `CLAUDE_CODE_SUBAGENT_MODEL` apply to every subagent. Without it, an
/// agent defined with a model wins, and Claude Code's built-in Explore and
/// Plan agents are defined with `inherit`: they run on the main model.
const SUBAGENT_MODEL_FORCE_ENV: &str = "CLAUDE_CODE_SUBAGENT_MODEL_FORCE";
/// Claude Code's model for its `haiku` alias, which its background tasks
/// (titles, summaries) use.
const HAIKU_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_HAIKU_MODEL";
pub(crate) const AUTO_COMPACT_WINDOW_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
/// The context window Claude Code budgets a model it doesn't know against
/// (otherwise 200K).
pub(crate) const MAX_CONTEXT_TOKENS_ENV: &str = "CLAUDE_CODE_MAX_CONTEXT_TOKENS";
/// Removed from every Claude Code launch, cloud and local: the token Rayline
/// sets is the only credential, and a vendor switch would send requests
/// around Rayline.
const CLAUDE_REMOVED_ENV: [&str; 4] = [
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];
/// Claude Code's switch for message threads ("tether"): server-held history
/// that later requests continue by `previous_message_id`. Always `0`: the
/// router holds no message-thread history.
const MESSAGE_THREADS_ENV: &str = "CLAUDE_CODE_TETHER_LIVE";
pub(crate) const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
const CLAUDE_DISABLE_AGENT_VIEW_ENV: &str = "CLAUDE_CODE_DISABLE_AGENT_VIEW";
/// Claude Code's own refusal recovery, switched off because the router serves
/// the main conversation: refusals are the router's to handle. The fallback
/// resends a refused turn to another Claude model (it fires for models with
/// the `refusal_fallback` capability, e.g. a Claude id, not for a router
/// alias); the retry (2.1.282+, on by default) resends it once to the same
/// model with a nudge that stays in the history, so every refused turn hits
/// the router twice.
const CLAUDE_REFUSAL_RECOVERY_OFF_ENV: [&str; 2] = [
    "CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK",
    "CLAUDE_CODE_DISABLE_REFUSAL_RETRY",
];
pub(crate) const RAYLINE_ENV_NAME_ENV: &str = "RAYLINE_ENV_NAME";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    /// The hosted environment the loaded config selects.
    pub hosted: crate::status::HostedEnvironment,
    /// `cloud`: the models the launch gives the harness.
    pub cloud: crate::config::Cloud,
    pub args: Vec<OsString>,
    /// Run against an isolated Claude config dir (`~/.rayline/harness/claude-code`) so this
    /// session can start its own background-agents supervisor alongside a
    /// standard Claude Code daemon instead of conflicting with it. The dir is a
    /// thin overlay: shared content (projects, sessions, history, skills,
    /// commands, agents, CLAUDE.md) is symlinked back to the user's main
    /// `~/.claude`, settings.json is seeded as a local copy, .claude.json is
    /// mirrored from the selected source profile without its auth keys, and
    /// daemon/runtime state stays local so two live supervisors never collide.
    pub isolated: bool,
}

/// The subagent and background-task models, as `(env var, value)`: the child
/// runs with each `Some` and without each `None`, and a reused Claude Code
/// daemon must hold the same.
///
/// Subagents and background tasks run on ARC Swift (the key's id for it, the
/// hosted router's background model). `CLAUDE_CODE_SUBAGENT_MODEL_FORCE` makes
/// the model reach every subagent: Claude Code's built-in Explore and Plan
/// agents (defined with `inherit`), a custom agent's own `model`, and a model
/// the main agent names when it spawns one. Claude Code's own config pins (a
/// subagent model in its settings.json) win over this env.
fn role_model_env_switches(
    hosted: &crate::harness::LaunchModels,
) -> Vec<(&'static str, Option<String>)> {
    vec![
        (SUBAGENT_MODEL_ENV, Some(hosted.background.clone())),
        (SUBAGENT_MODEL_FORCE_ENV, Some("1".to_owned())),
        (HAIKU_MODEL_ENV, Some(hosted.background.clone())),
    ]
}

/// Claude Code's model aliases `opus`, `sonnet` and `fable`, as `(env var,
/// value)`, so a skill, agent or `--model` that names one runs a model the
/// router serves: `opus` (also Claude Code's default) is `cloud.model.id`,
/// and `sonnet` and `fable` take the other models the router lists for the
/// key, in its order, or `cloud.model.id` when none is left. `haiku` is
/// `cloud.smol_model.id` ([`role_model_env_switches`]). A reused Claude Code
/// daemon must hold the same.
fn alias_env(hosted: &crate::harness::LaunchModels) -> [(&'static str, String); 3] {
    let mut others = hosted
        .offered
        .iter()
        .filter(|model| model.id != hosted.main && model.id != hosted.background)
        .map(|model| model.id.clone());
    let mut next = || others.next().unwrap_or_else(|| hosted.main.clone());
    [
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", hosted.main.clone()),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", next()),
        ("ANTHROPIC_DEFAULT_FABLE_MODEL", next()),
    ]
}

/// The `--settings` JSON that makes Claude Code's `/model` picker show its
/// Default row and one row per model the launch offers, `cloud.model.id`
/// first, then the router's listing in its order, instead of its built-in
/// lineup. A row is labelled with the model's `display_name` (Claude Code
/// uses the id for an unlisted one) and described by its id.
fn picker_settings(hosted: &crate::harness::LaunchModels) -> String {
    let options: Vec<Value> = hosted
        .offered
        .iter()
        .map(|model| {
            let mut row = serde_json::json!({"model": model.id, "description": model.id});
            if let Some(details) = &model.details {
                row["label"] = Value::from(details.display_name.clone());
            }
            row
        })
        .collect();
    serde_json::json!({
        "modelPicker": {"replaceBuiltInOptions": true, "options": options}
    })
    .to_string()
}

/// `~/.rayline/harness/claude-code/model-picker/`: the picker settings of
/// each cloud launch, one file per launch.
fn picker_settings_dir(home: &Path) -> PathBuf {
    isolated_cc_dir(home).join("model-picker")
}

/// Writes `settings` to `model-picker/<pid>-<nanos>.json`, owner-only, for
/// the launch whose process is `pid`, started at `nanos` since the Unix
/// epoch (Claude Code runs in Rayline's place on Linux and macOS and as its
/// child on Windows, so the file is needed while that process runs). Files
/// of launches whose process is gone (`is_alive`) are removed first; the
/// time in the name keeps a later launch that reuses an ended launch's pid
/// from having its file removed with the ended one's. A file whose name is
/// not `<pid>-<nanos>.json` is left alone. Every failure is an error naming
/// the path.
fn write_picker_settings(
    home: &Path,
    pid: u32,
    nanos: u128,
    settings: &str,
    is_alive: impl Fn(u32) -> io::Result<bool>,
) -> io::Result<PathBuf> {
    let dir = picker_settings_dir(home);
    let failed = |action: &str, path: &Path, error: io::Error| {
        io::Error::new(
            error.kind(),
            format!("failed to {action} {}: {error}", path.display()),
        )
    };
    crate::harness::create_private_dir(&dir).map_err(|error| failed("create", &dir, error))?;
    let entries = fs::read_dir(&dir).map_err(|error| failed("read", &dir, error))?;
    let name = format!("{pid}-{nanos}.json");
    for entry in entries {
        let path = entry.map_err(|error| failed("read", &dir, error))?.path();
        let Some(other) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|stem| stem.split_once('-'))
            .filter(|(_, time)| time.parse::<u128>().is_ok())
            .and_then(|(pid, _)| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if other == pid {
            continue;
        }
        if !is_alive(other).map_err(|error| failed("check the launch of", &path, error))? {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(failed("remove", &path, error)),
            }
        }
    }
    let path = dir.join(name);
    crate::harness::write_private(&path, settings)
        .map_err(|error| failed("write", &path, error))?;
    Ok(path)
}

/// Nanoseconds since the Unix epoch, for a picker settings file's name
/// ([`write_picker_settings`]); 0 when the clock is before the epoch.
fn launch_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default()
}

/// The main model's context window for `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, when
/// the router sizes the session (`routed`: it answered an auto-compact
/// window, so Claude Code doesn't know the id and would assume 200K, capping
/// that window too) and the key's model listing gives the main model's
/// window. `None` for a model Claude Code sizes itself, and for a
/// `cloud.model.id` the listing doesn't have, which Claude Code sizes with
/// its own default. A value in the user's environment is not read.
fn max_context_tokens(hosted: &crate::harness::LaunchModels, routed: bool) -> Option<String> {
    if !routed {
        return None;
    }
    hosted
        .offered
        .iter()
        .find(|model| model.id == hosted.main)
        .and_then(|model| model.details.as_ref())
        .map(|details| details.context_window.to_string())
}

#[derive(Debug)]
pub enum RunError {
    ClaudeMissing,
    HomeNotFound(crate::HomeError),
    DaemonConflict(String),
    Router(String),
    Auth(crate::status::AuthError),
    ConfigDir(io::Error),
    IsolatedProfile(io::Error),
    LaunchRecord(io::Error),
    PickerSettings(io::Error),
    DaemonProbe(io::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClaudeMissing => formatter.write_str(
                "`claude` not found on PATH. Install Claude Code: https://docs.claude.com/en/docs/claude-code/setup",
            ),
            Self::HomeNotFound(error) => error.fmt(formatter),
            Self::DaemonConflict(message) => formatter.write_str(message),
            Self::Router(message) => formatter.write_str(message),
            Self::Auth(error) => error.fmt(formatter),
            Self::ConfigDir(error) => error.fmt(formatter),
            Self::IsolatedProfile(error) => {
                write!(formatter, "failed to prepare the isolated Claude profile: {error}")
            }
            Self::LaunchRecord(error) => {
                write!(formatter, "Claude Code launch record: {error}")
            }
            Self::PickerSettings(error) => {
                write!(formatter, "Claude Code model picker settings: {error}")
            }
            Self::DaemonProbe(error) => write!(formatter, "Claude Code daemon: {error}"),
        }
    }
}

impl std::error::Error for RunError {}

impl From<crate::status::AuthError> for RunError {
    fn from(error: crate::status::AuthError) -> Self {
        Self::Auth(error)
    }
}

pub async fn run_command(request: &RunRequest) -> Result<Command, RunError> {
    let home = crate::home().map_err(RunError::HomeNotFound)?;
    let claude_bin = crate::harness::find_on_path("claude").ok_or(RunError::ClaudeMissing)?;
    run_command_from_home(request, &home, claude_bin).await
}

/// The print-mode flag (`-p` / `--print`) in the forwarded Claude args, if any.
/// Print mode suppresses Claude Code's own startup output and is
/// non-interactive. Returns the exact flag the user passed, so messaging can
/// name it.
pub fn print_mode_flag(args: &[OsString]) -> Option<&'static str> {
    args.iter().find_map(|arg| {
        if arg == "-p" {
            Some("-p")
        } else if arg == "--print" {
            Some("--print")
        } else {
            None
        }
    })
}

/// Builds the Claude Code command for a cloud launch: Claude Code talks to the
/// environment's hosted router directly through `ANTHROPIC_BASE_URL`.
async fn run_command_from_home(
    request: &RunRequest,
    home: &Path,
    claude_bin: PathBuf,
) -> Result<Command, RunError> {
    let hosted = &request.hosted;
    let env_name = hosted.name().to_owned();
    let router_url = hosted.base_url.clone();
    let key = crate::status::router_key(hosted, home).await?;

    // A cloud-mode launch runs the ids `cloud` names, at the limits the
    // router lists for the key.
    let hosted_models = crate::harness::fetch_hosted_models(&router_url, &key, &request.cloud)
        .await
        .map_err(RunError::Router)?;
    let model = hosted_models.main.clone();
    // The auto-compact window is the router's window for the main ARC model,
    // or none, so Claude Code sizes the session itself. A window in the
    // user's environment is not read.
    let auto_compact_window = router_auto_compact_window(&router_url, &key, &model).await?;

    // `--isolated` (or choosing `[i]` at the conflict prompt) targets a private
    // config dir. Inspect the matching config dir for a daemon conflict.
    let mut isolated = request.isolated;
    let mut inspect_dir = claude_config_dir(home, isolated).map_err(RunError::ConfigDir)?;
    let launch_switches = cloud_launch_switches(&hosted_models, auto_compact_window.is_some());
    let daemon_request = RequestSpec {
        env_name: &env_name,
        env_flag: hosted.env.as_deref(),
        router_url: &router_url,
        auto_compact_window: auto_compact_window.as_deref(),
        refusal_recovery: &launch_switches,
        args: &request.args,
    };
    let preflight = LaunchPreflight {
        home,
        config_dir: &inspect_dir,
        request: &daemon_request,
        claude_bin: &claude_bin,
        isolated,
    }
    .resolve()?;
    let preserve_spawned_by_pids: Vec<u32> = match preflight {
        PreflightOutcome::Proceed(preflight) => {
            preflight.preserve_spawned_by_pid.into_iter().collect()
        }
        PreflightOutcome::SwitchToIsolated(default_preflight) => {
            // The user chose to run isolated: re-check the isolated config dir,
            // which may already host a daemon for a different env. No further
            // isolated escape from here. The default folder's daemon keeps
            // running, so its spawner's record stays kept too.
            isolated = true;
            inspect_dir = claude_config_dir(home, true).map_err(RunError::ConfigDir)?;
            let isolated_preflight = LaunchPreflight {
                home,
                config_dir: &inspect_dir,
                request: &daemon_request,
                claude_bin: &claude_bin,
                isolated: true,
            }
            .resolve()?;
            match isolated_preflight {
                PreflightOutcome::Proceed(preflight) => default_preflight
                    .preserve_spawned_by_pid
                    .into_iter()
                    .chain(preflight.preserve_spawned_by_pid)
                    .collect(),
                PreflightOutcome::SwitchToIsolated(_) => {
                    unreachable!("isolated re-check cannot isolate")
                }
            }
        }
    };

    let picker_path = write_picker_settings(
        home,
        std::process::id(),
        launch_nanos(),
        &picker_settings(&hosted_models),
        crate::claude_daemon::pid_is_alive,
    )
    .map_err(RunError::PickerSettings)?;
    let mut command = claude_command(&claude_bin);
    // Before the user's arguments: a `--settings` of their own replaces it.
    command.arg("--settings").arg(&picker_path);
    command.args(&request.args);
    if isolated {
        apply_isolated_config_dir(&mut command, home).map_err(RunError::IsolatedProfile)?;
    }
    command.env("ANTHROPIC_BASE_URL", &router_url);
    command.env("ANTHROPIC_AUTH_TOKEN", &key);
    command.env("ANTHROPIC_MODEL", &model);
    set_auto_compact_window_env(&mut command, auto_compact_window.as_deref());
    command.env(RAYLINE_ENV_NAME_ENV, &env_name);
    remove_claude_env(&mut command);
    apply_refusal_recovery_env(&mut command, &launch_switches);

    crate::claude_daemon::record_rayline_claude_launch(LaunchRecord {
        home,
        pid: std::process::id(),
        request: &daemon_request,
        preserve_spawned_by_pids: &preserve_spawned_by_pids,
    })
    .map_err(RunError::LaunchRecord)?;
    Ok(command)
}

/// The env a cloud launch sets or removes and a reused Claude Code daemon
/// must hold: the refusal-recovery switches, the role and alias models and
/// `CLAUDE_CODE_MAX_CONTEXT_TOKENS` (`routed`: the router answered an
/// auto-compact window).
fn cloud_launch_switches(
    hosted: &crate::harness::LaunchModels,
    routed: bool,
) -> Vec<(&'static str, Option<String>)> {
    let mut switches = launch_env_switches();
    switches.extend(role_model_env_switches(hosted));
    switches.extend(
        alias_env(hosted)
            .into_iter()
            .map(|(name, value)| (name, Some(value))),
    );
    switches.push((MAX_CONTEXT_TOKENS_ENV, max_context_tokens(hosted, routed)));
    switches
}

/// The refusal-recovery switches, both `1`: the child runs with them and a
/// reused daemon must hold them, since its background workers inherit the
/// daemon's env.
pub(crate) fn refusal_recovery_values() -> Vec<(&'static str, Option<String>)> {
    CLAUDE_REFUSAL_RECOVERY_OFF_ENV
        .into_iter()
        .map(|key| (key, Some("1".to_owned())))
        .collect()
}

/// The refusal-recovery switches plus the message-threads switch (`0`):
/// every env switch the child runs with and a reused daemon must hold.
pub(crate) fn launch_env_switches() -> Vec<(&'static str, Option<String>)> {
    let mut values = refusal_recovery_values();
    values.push((MESSAGE_THREADS_ENV, Some("0".to_owned())));
    values
}

/// Sets the launch switches on the child: a `Some` is set and a `None` is
/// removed, so a value in the user's env never reaches it.
pub(crate) fn apply_refusal_recovery_env(
    command: &mut Command,
    values: &[(&'static str, Option<String>)],
) {
    for (key, value) in values {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
}

/// The Claude Code session command, with Rayline's router keys withheld.
/// Claude Code never needs them: the launch passes the key as
/// `ANTHROPIC_AUTH_TOKEN`. Its Bash tool passes Claude Code's environment on to
/// every command it runs, so a key left there would reach a model-visible `env`
/// call. Every `claude` spawn goes through here.
pub(crate) fn claude_command(claude_bin: &Path) -> Command {
    let mut command = Command::new(claude_bin);
    crate::codex::withhold_router_key(&mut command);
    command
}

/// Removes [`CLAUDE_REMOVED_ENV`] from a Claude Code launch.
pub(crate) fn remove_claude_env(command: &mut Command) {
    for name in CLAUDE_REMOVED_ENV {
        command.env_remove(name);
    }
}

/// Pin `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, or clear it so Claude Code applies
/// its own window for the model.
fn set_auto_compact_window_env(command: &mut Command, auto_compact_window: Option<&str>) {
    match auto_compact_window {
        Some(window) => command.env(AUTO_COMPACT_WINDOW_ENV, window),
        None => command.env_remove(AUTO_COMPACT_WINDOW_ENV),
    };
}

/// The hosted router's auto-compact window for the main ARC model:
/// `GET /v1/settings?model=<model>&client_sizes=1` returns the window of the model serving a
/// launch id Claude Code doesn't know, and a null or absent `autoCompactWindow`
/// for one Claude Code sizes itself, which is `None` here. A failed request or a
/// window that isn't a positive integer fails the launch rather than guessing.
async fn router_auto_compact_window(
    router_base: &str,
    router_key: &str,
    model: &str,
) -> Result<Option<String>, RunError> {
    get_router_settings(router_base, router_key, model)
        .await
        .and_then(|body| match body.get("autoCompactWindow") {
            None | Some(Value::Null) => Ok(None),
            Some(window) => window
                .as_u64()
                .filter(|window| *window > 0)
                .map(|window| Some(window.to_string()))
                .ok_or_else(|| {
                    format!("the response's autoCompactWindow {window} is not a positive integer")
                }),
        })
        .map_err(|error| {
            RunError::Router(format!(
                "Couldn't read the auto-compact window for {model} from {router_base}/v1/settings: \
                 {error}"
            ))
        })
}

/// `GET /v1/settings?model=<model>` with the router key.
async fn get_router_settings(
    router_base: &str,
    router_key: &str,
    model: &str,
) -> Result<Value, String> {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|error| error.to_string())?
        .get(format!("{router_base}/v1/settings"))
        .bearer_auth(router_key)
        // `client_sizes=1` tells the router this client accepts a null window
        // (Claude Code sizes the model itself).
        .query(&[("model", model), ("client_sizes", "1")])
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("HTTP {status}: {error}"))?;
    if let Some(error) = body.get("error") {
        let message = error
            .as_str()
            .map_or_else(|| error.to_string(), ToOwned::to_owned);
        return Err(format!("HTTP {status}: {message}"));
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    Ok(body)
}

pub(crate) fn claude_agent_view_disabled() -> bool {
    env::var(CLAUDE_DISABLE_AGENT_VIEW_ENV).is_ok_and(|value| agent_view_disabled_by(&value))
}

/// Claude Code's own reading of `CLAUDE_CODE_DISABLE_AGENT_VIEW`: only `1`,
/// `true`, `yes` and `on` (trimmed, any case) disable the agent view.
fn agent_view_disabled_by(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Resolve the Claude Code config dir this run targets. In isolated mode that is
/// Rayline's own dir (`~/.rayline/harness/claude-code`); otherwise it honors an externally
/// set `CLAUDE_CONFIG_DIR` as given, as Claude Code does, else the standard
/// `~/.claude`. An empty `CLAUDE_CONFIG_DIR` is an error naming it.
fn claude_config_dir(home: &Path, isolated: bool) -> io::Result<PathBuf> {
    if isolated {
        return Ok(isolated_cc_dir(home));
    }
    Ok(
        crate::harness::exported_dir(CLAUDE_CONFIG_DIR_ENV)?
            .unwrap_or_else(|| home.join(".claude")),
    )
}

/// The Claude config dir used by `--isolated`: `~/.rayline/harness/claude-code`.
pub(crate) fn isolated_cc_dir(home: &Path) -> PathBuf {
    home.join(crate::DOT_CONFIG_DIR)
        .join("harness")
        .join("claude-code")
}

/// Write-target directories Claude creates session work under. Created in the
/// shared root if missing so the symlink always points at the shared copy, even
/// on a fresh `~/.claude`: otherwise Claude would create them locally under the
/// isolated config and isolated-mode work would never show up in standard runs.
/// Daemon/runtime state (daemon.lock, daemon/, jobs/, tasks/, teams/, todos/) is
/// deliberately absent so it stays local and the supervisors never collide.
const ISOLATED_SHARED_DIRS: [&str; 4] = ["projects", "sessions", "plans", "paste-cache"];

/// Write-target files: created empty in the shared root if missing, then
/// symlinked, so isolated and standard runs append to the same file.
const ISOLATED_SHARED_FILES: [&str; 1] = ["history.jsonl"];

/// Read-only customization shared via symlink only when present in the shared
/// root. Claude does not create these, so a missing entry cannot diverge; we
/// avoid materializing empty `CLAUDE.md`/statusline/skills entries the user
/// never had.
const ISOLATED_OPTIONAL_ENTRIES: [&str; 7] = [
    "skills",
    "plugins",
    "commands",
    "agents",
    "CLAUDE.md",
    "hooks",
    "statusline-worktree.js",
];

/// Per-profile config dir files seeded as independent copies so they can
/// diverge. `.claude.json` is seeded separately (see
/// [`seed_isolated_claude_json`]) because it lives outside the config dir.
const ISOLATED_SEED_FILES: [&str; 1] = ["settings.json"];

/// Lay down the isolated overlay and point Claude Code at it. Idempotent: safe
/// to call on every run. The isolated profile needs no Claude login: the launch
/// authenticates Claude Code through `ANTHROPIC_AUTH_TOKEN`.
fn apply_isolated_config_dir(command: &mut Command, home: &Path) -> io::Result<()> {
    let source_dir = claude_config_dir(home, false)?;
    let isolated_dir = isolated_cc_dir(home);
    ensure_isolated_overlay(&source_dir, &isolated_dir)?;
    // `.claude.json` (global app state, project-trust, personal MCP) lives at
    // $HOME/.claude.json by default, NOT inside ~/.claude. Seed the isolated
    // profile from it so project-trust, MCP servers and settings carry over,
    // without the auth keys.
    seed_isolated_claude_json(
        &global_claude_json_path(home)?,
        &isolated_dir.join(".claude.json"),
    )?;
    command.env(CLAUDE_CONFIG_DIR_ENV, &isolated_dir);
    Ok(())
}

/// Path of Claude Code's global `.claude.json` (app state, project-trust,
/// personal MCP servers, OAuth). It lives inside `CLAUDE_CONFIG_DIR` when that
/// is set, otherwise at `$HOME/.claude.json` (NOT inside `~/.claude`).
fn global_claude_json_path(home: &Path) -> io::Result<PathBuf> {
    Ok(match crate::harness::exported_dir(CLAUDE_CONFIG_DIR_ENV)? {
        Some(dir) => dir.join(".claude.json"),
        None => home.join(".claude.json"),
    })
}

/// Make `isolated_dir` a thin overlay on `shared_root` (the user's main Claude
/// config dir): symlink shared content back to the shared root and seed
/// settings.json as a local copy. A failed directory, file, link or seed copy
/// is an error with the path.
fn ensure_isolated_overlay(shared_root: &Path, isolated_dir: &Path) -> io::Result<()> {
    if shared_root == isolated_dir {
        // The shared root already is the isolated dir (e.g. the user pointed
        // CLAUDE_CONFIG_DIR at it); there is nothing to overlay onto itself.
        return Ok(());
    }
    fs::create_dir_all(isolated_dir).map_err(|error| error_with_path(isolated_dir, error))?;
    for dir in ISOLATED_SHARED_DIRS {
        let source = shared_root.join(dir);
        // Create the shared target first so the symlink always resolves to it,
        // even on a brand-new ~/.claude.
        fs::create_dir_all(&source).map_err(|error| error_with_path(&source, error))?;
        link_shared_entry(&source, &isolated_dir.join(dir))?;
    }
    for file in ISOLATED_SHARED_FILES {
        let source = shared_root.join(file);
        if !source.exists() {
            fs::write(&source, b"").map_err(|error| error_with_path(&source, error))?;
        }
        link_shared_entry(&source, &isolated_dir.join(file))?;
    }
    for entry in ISOLATED_OPTIONAL_ENTRIES {
        link_shared_entry(&shared_root.join(entry), &isolated_dir.join(entry))?;
    }
    for file in ISOLATED_SEED_FILES {
        seed_local_copy(&shared_root.join(file), &isolated_dir.join(file))?;
    }
    Ok(())
}

/// Symlink `link` -> `source` when the source exists and the slot is still
/// empty. An existing file or symlink is left untouched (respect the user),
/// including one a concurrent isolated launch created first. A failed link is
/// an error with both paths.
fn link_shared_entry(source: &Path, link: &Path) -> io::Result<()> {
    if !source.exists() {
        return Ok(());
    }
    if fs::symlink_metadata(link).is_ok() {
        return Ok(());
    }
    match make_symlink(source, link) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(link_error(source, link, error)),
    }
}

/// Windows' ERROR_PRIVILEGE_NOT_HELD: the user may not create symlinks.
#[cfg(windows)]
const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

/// The error for a failed `link` -> `source` symlink, with both paths. On
/// Windows, a missing symlink privilege also says what `--isolated` needs.
fn link_error(source: &Path, link: &Path, error: io::Error) -> io::Error {
    let paths = format!("link {} to {}", link.display(), source.display());
    #[cfg(windows)]
    if error.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD) {
        return io::Error::new(
            error.kind(),
            format!(
                "{paths}: {error}. {} claude --isolated needs symlink rights on Windows: turn on Developer Mode or run it from an elevated shell",
                crate::CLI_BIN
            ),
        );
    }
    io::Error::new(error.kind(), format!("{paths}: {error}"))
}

/// Copy `source` -> `dest` once, when the destination does not yet exist. These
/// files hold credentials, so keep the copy (and its parent dir) user-private.
fn seed_local_copy(source: &Path, dest: &Path) -> io::Result<()> {
    if fs::symlink_metadata(dest).is_ok() {
        return Ok(());
    }
    copy_private_file(source, dest)
}

/// Auth-bearing keys in `.claude.json` — stripped when seeding an isolated
/// profile so it starts logged out (the launch authenticates it through the
/// env), while project-trust, MCP servers and settings carry over. `primaryApiKey` is
/// where Claude stores a Console/API key when keychain storage is unavailable;
/// `customApiKeyResponses` caches API-key approval prompts; `oauthAccount` is the
/// subscription login. None of these should be copied into the isolated profile.
const CLAUDE_JSON_AUTH_KEYS: [&str; 3] = ["oauthAccount", "customApiKeyResponses", "primaryApiKey"];

/// Seed the isolated `.claude.json` from the user's profile, minus the auth
/// keys, so the isolated session inherits project-trust, MCP servers and
/// settings but starts logged out. A missing source seeds a minimal
/// onboarding-complete stub. An unreadable source, one that is not a JSON
/// object, or a failed write is an error with the path. No-op if the
/// destination already exists (the profile keeps its own state).
fn seed_isolated_claude_json(source: &Path, dest: &Path) -> io::Result<()> {
    if fs::symlink_metadata(dest).is_ok() {
        return Ok(());
    }
    let contents = match fs::read_to_string(source) {
        Ok(raw) => claude_json_without_auth(&raw).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: not a JSON object", source.display()),
            )
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            "{\n  \"hasCompletedOnboarding\": true\n}".to_owned()
        }
        Err(error) => return Err(error_with_path(source, error)),
    };
    fs::write(dest, contents).map_err(|error| error_with_path(dest, error))?;
    #[cfg(unix)]
    set_user_private_permissions(dest)?;
    Ok(())
}

/// Return `raw` (`.claude.json` contents) with the auth keys removed, or `None`
/// if it is not a JSON object. Used to seed a logged-out isolated profile that
/// still carries project-trust, MCP servers and settings.
fn claude_json_without_auth(raw: &str) -> Option<String> {
    let mut value = serde_json::from_str::<Value>(raw).ok()?;
    let object = value.as_object_mut()?;
    for key in CLAUDE_JSON_AUTH_KEYS {
        object.remove(key);
    }
    serde_json::to_string_pretty(&value).ok()
}

/// Copy `source` to a user-private `dest`. A missing source copies nothing;
/// any other failure is an error with the path.
fn copy_private_file(source: &Path, dest: &Path) -> io::Result<()> {
    match fs::metadata(source) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error_with_path(source, error)),
    }
    fs::copy(source, dest).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("copy {} to {}: {error}", source.display(), dest.display()),
        )
    })?;
    #[cfg(unix)]
    set_user_private_permissions(dest)?;
    Ok(())
}

fn error_with_path(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

#[cfg(unix)]
fn make_symlink(source: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, link)
}

#[cfg(windows)]
fn make_symlink(source: &Path, link: &Path) -> io::Result<()> {
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(source, link)
    } else {
        std::os::windows::fs::symlink_file(source, link)
    }
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_source: &Path, _link: &Path) -> io::Result<()> {
    Ok(())
}

/// Makes `path` readable by the user only (`0600`) and its parent dir `0700`.
/// A failed chmod is an error with the path. Unix only: Windows has no mode
/// bits, and a file in the user's profile keeps the ACL it inherits there.
#[cfg(unix)]
fn set_user_private_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let restrict = |path: &Path, mode: u32| {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|error| error_with_path(path, error))
    };
    if let Some(parent) = path.parent() {
        restrict(parent, 0o700)?;
    }
    restrict(path, 0o600)
}

#[cfg(test)]
mod refusal_recovery_env_tests {
    use super::*;
    use std::collections::BTreeMap;

    const FALLBACK: &str = "CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK";
    const RETRY: &str = "CLAUDE_CODE_DISABLE_REFUSAL_RETRY";

    fn child_env(values: &[(&'static str, Option<String>)]) -> Vec<(String, Option<String>)> {
        let mut command = Command::new("claude");
        apply_refusal_recovery_env(&mut command, values);
        command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    /// A router answering `/v1/settings` with `window` as its
    /// `autoCompactWindow` and every other request with a standard key's
    /// `/v1/models`.
    async fn router_stub(window: Value) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let listing = crate::harness::tests::listing(None).to_string();
        let settings = serde_json::json!({ "autoCompactWindow": window }).to_string();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let read = stream.read(&mut buf).await.unwrap_or(0);
                let body = if buf[..read].starts_with(b"GET /v1/settings") {
                    &settings
                } else {
                    &listing
                };
                let response = format!(
                    "HTTP/1.1 200 OK
content-type: application/json
content-length: {}
connection: close

{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        base
    }

    /// What the real launcher builds against [`router_stub`] answering
    /// `window`, with the user's `args`.
    struct Launched {
        env: BTreeMap<String, Option<String>>,
        router_url: String,
        args: Vec<String>,
        /// The `--settings` file's contents and, on Unix, its mode.
        picker: String,
        #[cfg(unix)]
        picker_mode: u32,
    }

    /// The child the real launcher builds against [`router_stub`] answering
    /// `window`. With a stored key it starts no process, so the command can
    /// be read directly. Every switch, a compact window, an API key and the
    /// vendor switches are exported with values the child must not get.
    async fn launched_env(window: Value) -> (BTreeMap<String, Option<String>>, String) {
        let launched = launch(window, Vec::new()).await;
        (launched.env, launched.router_url)
    }

    async fn launch(window: Value, args: Vec<OsString>) -> Launched {
        static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _guard = ENV_LOCK.lock().await;
        let home =
            std::env::temp_dir().join(format!("rayline-cli-refusal-launch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        crate::credentials::save_test_router_key(&home, Some("stub"), "rlk-test");
        let router_url = router_stub(window).await;
        let request = RunRequest {
            hosted: crate::status::HostedEnvironment {
                env: Some("stub".to_owned()),
                base_url: router_url.clone(),
                login_url: "http://127.0.0.1:9/cli-auth".to_owned(),
            },
            cloud: crate::harness::tests::cloud(None),
            args,
            isolated: false,
        };
        let exported = [
            (FALLBACK, "0"),
            (RETRY, "0"),
            (MESSAGE_THREADS_ENV, "1"),
            (SUBAGENT_MODEL_ENV, "exported-model"),
            (SUBAGENT_MODEL_FORCE_ENV, "0"),
            (HAIKU_MODEL_ENV, "exported-model"),
            // An exported model is not a model source: the child runs on the
            // router's main model.
            ("ANTHROPIC_MODEL", "exported-model"),
            // An exported window is not read: the child gets the router's.
            (AUTO_COMPACT_WINDOW_ENV, "180000"),
            ("ANTHROPIC_API_KEY", "exported-key"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("CLAUDE_CODE_USE_VERTEX", "1"),
            ("CLAUDE_CODE_USE_FOUNDRY", "1"),
        ];
        let saved: Vec<_> = [CLAUDE_CONFIG_DIR_ENV]
            .into_iter()
            .chain(exported.iter().map(|(key, _)| *key))
            .map(|key| (key, std::env::var_os(key)))
            .collect();
        // A private Claude config dir, so no real daemon on this machine is
        // inspected.
        // SAFETY: serialized by ENV_LOCK; restored below.
        unsafe {
            std::env::set_var(CLAUDE_CONFIG_DIR_ENV, home.join(".claude"));
            for (key, value) in exported {
                std::env::set_var(key, value);
            }
        }
        let command = run_command_from_home(&request, &home, PathBuf::from("/nonexistent/claude"))
            .await
            .expect("the launch builds its command");
        let env = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "--settings");
        let picker_path = PathBuf::from(&args[1]);
        assert_eq!(
            picker_path.parent(),
            Some(picker_settings_dir(&home).as_path())
        );
        let name = picker_path.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with(&format!("{}-", std::process::id())) && name.ends_with(".json"),
            "{name}"
        );
        let picker = std::fs::read_to_string(&picker_path).unwrap();
        #[cfg(unix)]
        let picker_mode = {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::metadata(&picker_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        for (key, value) in saved {
            // SAFETY: serialized by ENV_LOCK.
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        let _ = std::fs::remove_dir_all(&home);
        Launched {
            env,
            router_url,
            args,
            picker,
            #[cfg(unix)]
            picker_mode,
        }
    }

    // The picker rows go to an owner-only file named by this process's pid,
    // and its path is the first `--settings`, before the user's arguments,
    // so a `--settings` of the user's own comes later and replaces it.
    #[tokio::test]
    async fn the_launcher_passes_the_picker_settings_by_file() {
        let user = ["--settings", "mine.json", "-p", "hi"];
        let launched = launch(Value::Null, user.iter().map(OsString::from).collect()).await;
        assert_eq!(launched.args[2..], user);
        assert_eq!(
            launched.picker,
            picker_settings(&crate::harness::tests::hosted_models())
        );
        #[cfg(unix)]
        assert_eq!(launched.picker_mode, 0o600);
    }

    #[tokio::test]
    async fn the_launcher_sets_the_switches_on_the_child_it_builds() {
        let (env, router_url) = launched_env(serde_json::json!(250_000)).await;
        let value = |key: &str| env.get(key).cloned().flatten();
        let hosted = crate::harness::tests::hosted_models();
        assert_eq!(value("ANTHROPIC_BASE_URL"), Some(router_url));
        assert_eq!(value(AUTO_COMPACT_WINDOW_ENV).as_deref(), Some("250000"));
        assert_eq!(value("ANTHROPIC_MODEL"), Some(hosted.main));
        assert_eq!(value(FALLBACK).as_deref(), Some("1"));
        assert_eq!(value(RETRY).as_deref(), Some("1"));
        assert_eq!(value(MESSAGE_THREADS_ENV).as_deref(), Some("0"));
        assert_eq!(value(SUBAGENT_MODEL_ENV), Some(hosted.background.clone()));
        assert_eq!(value(SUBAGENT_MODEL_FORCE_ENV).as_deref(), Some("1"));
        assert_eq!(value(HAIKU_MODEL_ENV), Some(hosted.background));
        for name in CLAUDE_REMOVED_ENV {
            assert_eq!(env.get(name), Some(&None), "{name}");
        }
    }

    #[tokio::test]
    async fn the_launcher_removes_an_exported_window_the_router_does_not_set() {
        let (env, _) = launched_env(Value::Null).await;
        assert_eq!(env.get(AUTO_COMPACT_WINDOW_ENV), Some(&None));
    }

    #[test]
    fn the_child_command_sets_each_switch_and_removes_the_unset_ones() {
        // The haiku model is unset under a Claude Code alias.
        assert_eq!(
            child_env(&[(HAIKU_MODEL_ENV, None)]),
            vec![(HAIKU_MODEL_ENV.to_owned(), None)]
        );
        assert_eq!(
            refusal_recovery_values(),
            vec![
                (FALLBACK, Some("1".to_owned())),
                (RETRY, Some("1".to_owned()))
            ]
        );
        assert_eq!(
            child_env(&launch_env_switches()),
            vec![
                (FALLBACK.to_owned(), Some("1".to_owned())),
                (RETRY.to_owned(), Some("1".to_owned())),
                (MESSAGE_THREADS_ENV.to_owned(), Some("0".to_owned()))
            ]
        );
    }
}

#[cfg(test)]
mod router_auto_compact_window_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A `/v1/settings` stub answering one request with `status` and `body`;
    /// the join handle yields the request it received.
    async fn settings_stub(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let read = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..read]).into_owned();
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            request
        });
        (base, served)
    }

    async fn window(
        status: &'static str,
        body: &'static str,
    ) -> (Result<Option<String>, String>, String) {
        let (base, served) = settings_stub(status, body).await;
        let window = router_auto_compact_window(&base, "key", "arc-sage-1.0")
            .await
            .map_err(|error| error.to_string());
        (window, served.await.unwrap())
    }

    #[tokio::test]
    async fn asks_the_router_with_the_main_model_and_returns_its_window() {
        let (window, request) =
            window("200 OK", r#"{"settings":{},"autoCompactWindow":262144}"#).await;
        assert_eq!(window, Ok(Some("262144".to_owned())));
        assert!(
            request.starts_with("GET /v1/settings?model=arc-sage-1.0&client_sizes=1 "),
            "{request}"
        );
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("authorization: Bearer key")),
            "{request}"
        );
    }

    #[tokio::test]
    async fn a_null_or_absent_window_leaves_the_window_to_claude_code() {
        for body in [
            r#"{"settings":{},"autoCompactWindow":null}"#,
            r#"{"settings":{}}"#,
        ] {
            let (window, _) = window("200 OK", body).await;
            assert_eq!(window, Ok(None), "{body}");
        }
    }

    #[tokio::test]
    async fn fails_the_launch_on_a_router_error_or_a_malformed_window() {
        for (status, body, reason) in [
            (
                "400 Bad Request",
                r#"{"error":"Unknown model: rayline-router[1m]"}"#,
                "Unknown model: rayline-router[1m]",
            ),
            (
                "200 OK",
                r#"{"settings":{},"autoCompactWindow":"950000"}"#,
                r#"autoCompactWindow "950000" is not a positive integer"#,
            ),
            (
                "200 OK",
                r#"{"settings":{},"autoCompactWindow":0}"#,
                "autoCompactWindow 0 is not a positive integer",
            ),
        ] {
            let (window, _) = window(status, body).await;
            let error = window.unwrap_err();
            assert!(error.contains(reason), "{error}");
            // The user's window is not read, so the error offers none.
            assert!(
                !error.contains("CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
                "{error}"
            );
        }
    }
}

#[cfg(test)]
mod claude_command_tests {
    use super::*;

    #[test]
    fn isolated_claude_code_writes_only_under_its_harness_folder() {
        let home = crate::harness::tests::temp_dir("claude-code-home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/settings.json"), "{}").unwrap();
        fs::write(home.join(".claude.json"), "{}").unwrap();
        apply_isolated_config_dir(&mut Command::new("claude"), &home).unwrap();
        crate::claude_daemon::record_rayline_claude_launch(LaunchRecord {
            home: &home,
            pid: 1,
            request: &RequestSpec {
                env_name: "prod",
                env_flag: None,
                router_url: crate::ROUTER_PROD_URL,
                auto_compact_window: None,
                refusal_recovery: &[],
                args: &[],
            },
            preserve_spawned_by_pids: &[],
        })
        .unwrap();
        crate::harness::tests::assert_only_harness_files(
            &home,
            "claude-code",
            &[".claude", ".claude.json"],
        );
        assert!(
            home.join(".rayline/harness/claude-code/daemon-launches.json")
                .is_file()
        );
        fs::remove_dir_all(home).unwrap();
    }

    /// A router key in the launcher's environment must not reach Claude Code,
    /// whose Bash tool hands its environment to every command it runs.
    #[cfg(unix)]
    #[test]
    fn claude_session_never_inherits_a_router_key() {
        let vars = ["RAYLINE_ROUTER_API_KEY"];
        let previous: Vec<_> = vars.iter().map(env::var_os).collect();
        for var in vars {
            // SAFETY: tests run with --test-threads=1 (AGENTS.md), so no other
            // thread reads the environment concurrently.
            unsafe { env::set_var(var, "rlk-test-not-a-real-key") };
        }
        let output = claude_command(Path::new("/usr/bin/env"))
            .output()
            .expect("run env");
        for (var, value) in vars.iter().zip(previous) {
            // SAFETY: as above.
            match value {
                Some(value) => unsafe { env::set_var(var, value) },
                None => unsafe { env::remove_var(var) },
            }
        }
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success());
        assert!(
            !printed.contains("rlk-test-not-a-real-key"),
            "the Claude Code environment still carries a router key"
        );
    }
}

#[cfg(test)]
mod auto_compact_window_env_tests {
    use super::*;

    #[test]
    fn unpinned_window_clears_an_inherited_env_value() {
        let mut command = Command::new("true");
        command.env(AUTO_COMPACT_WINDOW_ENV, "");
        set_auto_compact_window_env(&mut command, None);
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == AUTO_COMPACT_WINDOW_ENV && value.is_none())
        );

        set_auto_compact_window_env(&mut command, Some("180000"));
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == AUTO_COMPACT_WINDOW_ENV
                    && value == Some(std::ffi::OsStr::new("180000")))
        );
    }
}

#[cfg(test)]
mod claude_json_without_auth_tests {
    use super::*;

    #[test]
    fn strips_auth_keys_but_keeps_trust_and_mcp() {
        let raw = r#"{
            "oauthAccount": {"emailAddress": "me@example.com"},
            "customApiKeyResponses": {"approved": ["k"]},
            "primaryApiKey": "sk-ant-secret",
            "hasCompletedOnboarding": true,
            "projects": {"/repo": {"hasTrustDialogAccepted": true, "mcpServers": {}}},
            "mcpServers": {"local": {"command": "x"}}
        }"#;
        let out = claude_json_without_auth(raw).expect("valid object");
        assert!(!out.contains("sk-ant-secret"), "API key leaked: {out}");
        let value: Value = serde_json::from_str(&out).unwrap();
        let object = value.as_object().unwrap();

        // Auth state removed.
        assert!(!object.contains_key("oauthAccount"));
        assert!(!object.contains_key("customApiKeyResponses"));
        assert!(!object.contains_key("primaryApiKey"));
        // Trust, MCP and onboarding state preserved.
        assert!(object.contains_key("projects"));
        assert!(object.contains_key("mcpServers"));
        assert_eq!(
            object.get("hasCompletedOnboarding"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn returns_none_for_non_object() {
        assert!(claude_json_without_auth("[1, 2, 3]").is_none());
        assert!(claude_json_without_auth("not json").is_none());
    }
}

#[cfg(test)]
mod isolated_seed_tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rayline-claude-seed-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mentions(error: &io::Error, path: &Path) -> bool {
        error.to_string().contains(&path.display().to_string())
    }

    #[test]
    fn missing_claude_json_seeds_the_onboarding_stub() {
        let dir = temp_dir("json-missing");
        let dest = dir.join(".claude.json");
        seed_isolated_claude_json(&dir.join("absent.json"), &dest).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(value, serde_json::json!({"hasCompletedOnboarding": true}));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_or_invalid_claude_json_fails_with_the_source_path() {
        let dir = temp_dir("json-bad");
        let dest = dir.join(".claude.json");

        let unreadable = dir.join("unreadable.json");
        fs::create_dir(&unreadable).unwrap();
        let error = seed_isolated_claude_json(&unreadable, &dest).unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::NotFound);
        assert!(mentions(&error, &unreadable), "{error}");

        let invalid = dir.join("invalid.json");
        fs::write(&invalid, "[1, 2, 3]").unwrap();
        let error = seed_isolated_claude_json(&invalid, &dest).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(mentions(&error, &invalid), "{error}");

        assert!(fs::symlink_metadata(&dest).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_claude_json_write_fails_with_the_dest_path() {
        let dir = temp_dir("json-write");
        let dest = dir.join("no-such-dir").join(".claude.json");
        let error = seed_isolated_claude_json(&dir.join("absent.json"), &dest).unwrap_err();
        assert!(mentions(&error, &dest), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_skips_a_missing_source_or_a_taken_slot() {
        let dir = temp_dir("link-skip");
        link_shared_entry(&dir.join("absent"), &dir.join("link")).unwrap();
        assert!(fs::symlink_metadata(dir.join("link")).is_err());

        let source = dir.join("source");
        fs::write(&source, "shared").unwrap();
        let taken = dir.join("taken");
        fs::write(&taken, "mine").unwrap();
        link_shared_entry(&source, &taken).unwrap();
        assert_eq!(fs::read_to_string(&taken).unwrap(), "mine");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_link_fails_with_both_paths() {
        let dir = temp_dir("link-fail");
        let source = dir.join("source");
        fs::write(&source, "shared").unwrap();
        let link = dir.join("no-such-dir").join("link");
        let error = link_shared_entry(&source, &link).unwrap_err();
        assert!(mentions(&error, &source), "{error}");
        assert!(mentions(&error, &link), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn missing_windows_symlink_privilege_says_what_isolated_needs() {
        let source = Path::new(r"C:\shared\skills");
        let link = Path::new(r"C:\isolated\skills");
        let error = link_error(
            source,
            link,
            io::Error::from_raw_os_error(ERROR_PRIVILEGE_NOT_HELD),
        );
        let message = error.to_string();
        assert!(mentions(&error, source), "{message}");
        assert!(mentions(&error, link), "{message}");
        assert!(
            message.contains("claude --isolated needs symlink rights on Windows"),
            "{message}"
        );
        assert!(message.contains("Developer Mode"), "{message}");
        assert!(message.contains("elevated shell"), "{message}");
    }

    #[test]
    fn other_link_errors_name_only_the_paths() {
        let source = Path::new("shared-source");
        let link = Path::new("isolated-link");
        let error = link_error(source, link, io::Error::from(io::ErrorKind::NotFound));
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            error
                .to_string()
                .starts_with("link isolated-link to shared-source: ")
        );
        assert!(!error.to_string().contains("symlink rights"));
    }

    #[test]
    fn overlay_fails_with_the_path_when_the_isolated_dir_cannot_be_created() {
        let dir = temp_dir("overlay-dir");
        let shared_root = dir.join("shared");
        fs::create_dir_all(&shared_root).unwrap();
        // The isolated dir's parent is a file, so the dir cannot be created.
        let parent = dir.join("parent-file");
        fs::write(&parent, "").unwrap();
        let isolated_dir = parent.join("isolated");
        let error = ensure_isolated_overlay(&shared_root, &isolated_dir).unwrap_err();
        assert!(mentions(&error, &isolated_dir), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlay_fails_with_the_path_when_a_shared_entry_cannot_be_created() {
        let dir = temp_dir("overlay-shared");
        // The shared root is a file, so its shared dirs cannot be created.
        let shared_root = dir.join("shared-file");
        fs::write(&shared_root, "").unwrap();
        let error = ensure_isolated_overlay(&shared_root, &dir.join("isolated")).unwrap_err();
        let first_shared_dir = shared_root.join(ISOLATED_SHARED_DIRS[0]);
        assert!(mentions(&error, &first_shared_dir), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_private_file_skips_only_a_missing_source() {
        let dir = temp_dir("copy");
        let dest = dir.join("settings.json");
        copy_private_file(&dir.join("absent.json"), &dest).unwrap();
        assert!(fs::symlink_metadata(&dest).is_err());

        let source = dir.join("source.json");
        fs::write(&source, "{}").unwrap();
        copy_private_file(&source, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "{}");

        let failed_dest = dir.join("no-such-dir").join("settings.json");
        let error = copy_private_file(&source, &failed_dest).unwrap_err();
        assert!(mentions(&error, &source), "{error}");
        assert!(mentions(&error, &failed_dest), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A chmod that fails is an error with the path, not a file left with
    /// whatever mode it had.
    #[cfg(unix)]
    #[test]
    fn a_failed_chmod_is_an_error_with_the_path() {
        let dir = temp_dir("chmod");
        let missing = dir.join("settings.json");
        let error = set_user_private_permissions(&missing).unwrap_err();
        assert!(mentions(&error, &missing), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod arc_default_tests {
    use super::*;

    /// An ARC model the router sizes gets its listed window as Claude Code's
    /// context window; a model Claude Code sizes itself, or a `cloud.model.id`
    /// the listing doesn't have, gets none.
    #[test]
    fn claude_code_gets_the_main_model_s_window_when_the_router_sizes_it() {
        let models = crate::harness::tests::hosted_models();
        let main = models
            .offered
            .iter()
            .find(|model| model.id == models.main)
            .unwrap();
        assert_eq!(
            max_context_tokens(&models, true),
            Some(main.details.as_ref().unwrap().context_window.to_string())
        );
        assert_eq!(max_context_tokens(&models, false), None);

        let mut unlisted = models.clone();
        unlisted.offered.retain(|model| model.id != models.main);
        assert_eq!(max_context_tokens(&unlisted, true), None);
    }

    // Two listed models: the picker shows each once, main first; `sonnet`
    // and `fable` fall back to the main one.
    #[test]
    fn the_picker_shows_each_listed_model_once() {
        let models = crate::harness::tests::hosted_models();
        let settings: Value = serde_json::from_str(&picker_settings(&models)).unwrap();
        assert_eq!(
            settings,
            serde_json::json!({"modelPicker": {"replaceBuiltInOptions": true, "options": [
                {"model": "arc-sage-1.0", "label": "ARC Sage", "description": "arc-sage-1.0"},
                {"model": "arc-swift-1.0", "label": "ARC Swift", "description": "arc-swift-1.0"}
            ]}})
        );
        assert_eq!(
            alias_env(&models),
            [
                ("ANTHROPIC_DEFAULT_OPUS_MODEL", "arc-sage-1.0".to_owned()),
                ("ANTHROPIC_DEFAULT_SONNET_MODEL", "arc-sage-1.0".to_owned()),
                ("ANTHROPIC_DEFAULT_FABLE_MODEL", "arc-sage-1.0".to_owned()),
            ]
        );
    }

    // More listed models: every one is a picker row, and `sonnet` and
    // `fable` take the first two others. An unlisted main id is a row with
    // no label.
    #[test]
    fn every_listed_model_is_a_row_and_the_aliases_take_the_first_others() {
        let mut body = crate::harness::tests::listing(None);
        for id in ["a/one", "b/two", "c/three", "d/four"] {
            body["data"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "id": id, "object": "model", "owned_by": "x", "display_name": id.to_uppercase(),
                    "context_length": 32_000, "max_output_tokens": 4_096,
                    "capabilities": {"vision": false, "tools": true, "reasoning": true}
                }));
        }
        let mut cloud = crate::harness::tests::cloud(None);
        cloud.model.id = "unlisted/main".to_owned();
        let models = crate::harness::LaunchModels::from_listing(&body, &cloud).unwrap();
        let settings: Value = serde_json::from_str(&picker_settings(&models)).unwrap();
        let rows = settings["modelPicker"]["options"].as_array().unwrap();
        let ids: Vec<_> = rows
            .iter()
            .map(|row| row["model"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "unlisted/main",
                "arc-swift-1.0",
                "arc-sage-1.0",
                "a/one",
                "b/two",
                "c/three",
                "d/four"
            ]
        );
        assert_eq!(
            rows[0],
            serde_json::json!({"model": "unlisted/main", "description": "unlisted/main"})
        );
        assert_eq!(rows[3]["label"], "A/ONE");
        assert_eq!(
            alias_env(&models),
            [
                ("ANTHROPIC_DEFAULT_OPUS_MODEL", "unlisted/main".to_owned()),
                ("ANTHROPIC_DEFAULT_SONNET_MODEL", "arc-sage-1.0".to_owned()),
                ("ANTHROPIC_DEFAULT_FABLE_MODEL", "a/one".to_owned()),
            ]
        );
    }

    fn picker_home(name: &str) -> PathBuf {
        let home =
            std::env::temp_dir().join(format!("rayline-cli-picker-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        home
    }

    // Each launch has its own file, so concurrent launches don't overwrite
    // each other's; a launch removes the files of launches that have ended
    // and leaves those still running and any file it didn't name.
    #[test]
    fn each_launch_has_its_own_picker_file_and_ended_ones_are_removed() {
        let home = picker_home("prune");
        let alive = |pid: u32| Ok(pid != 11);
        let first = write_picker_settings(&home, 10, 1, "{\"a\":1}", alive).unwrap();
        let ended = write_picker_settings(&home, 11, 2, "{\"b\":1}", alive).unwrap();
        let dir = picker_settings_dir(&home);
        fs::write(dir.join("notes.txt"), "x").unwrap();
        fs::write(dir.join("11.json"), "x").unwrap();
        fs::write(dir.join("11-x.json"), "x").unwrap();
        let second = write_picker_settings(&home, 12, 3, "{\"c\":1}", alive).unwrap();
        assert_eq!(first, dir.join("10-1.json"));
        assert_eq!(second, dir.join("12-3.json"));
        assert_eq!(fs::read_to_string(&first).unwrap(), "{\"a\":1}");
        assert_eq!(fs::read_to_string(&second).unwrap(), "{\"c\":1}");
        assert!(!ended.exists());
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "10-1.json",
                "11-x.json",
                "11.json",
                "12-3.json",
                "notes.txt"
            ]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&second), 0o600);
        }
        let _ = fs::remove_dir_all(&home);
    }

    // A launch that reuses an ended launch's pid has a file of its own, so
    // pruning the ended one's file, even by a launch that found that pid
    // gone just before the new launch took it, leaves the new one's. A
    // failed liveness check fails the launch naming the file.
    #[test]
    fn a_reused_pid_has_its_own_file_and_a_failed_check_names_the_file() {
        let home = picker_home("reuse");
        let ended = write_picker_settings(&home, 20, 1, "old", |_| Ok(true)).unwrap();
        // The first probe finds pid 20 gone, then a new launch takes it;
        // pid 20 is running from then on.
        let reused = std::cell::RefCell::new(None);
        write_picker_settings(&home, 21, 3, "x", |pid| {
            assert_eq!(pid, 20);
            if reused.borrow().is_some() {
                return Ok(true);
            }
            let path = write_picker_settings(&home, 20, 2, "new", |_| Ok(true))?;
            *reused.borrow_mut() = Some(path);
            Ok(false)
        })
        .unwrap();
        let reused = reused.into_inner().unwrap();
        assert!(!ended.exists());
        assert_eq!(fs::read_to_string(&reused).unwrap(), "new");
        let error = write_picker_settings(&home, 22, 4, "x", |_| Err(io::Error::other("probe")))
            .unwrap_err();
        assert!(error.to_string().contains("-2.json"), "{error}");
        let _ = fs::remove_dir_all(&home);
    }

    // Many listed models: the whole list is in the file and the argument is
    // only its path, so the command line stays short however many models the
    // key lists (cmd.exe limits it to 8,191 characters for npm's claude.cmd).
    #[test]
    fn the_picker_argument_stays_short_with_many_models() {
        let mut body = crate::harness::tests::listing(None);
        for index in 0..1_000 {
            let id = format!("vendor/a-long-model-name-{index}");
            body["data"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "id": id, "object": "model", "owned_by": "x", "display_name": id.to_uppercase(),
                    "context_length": 32_000, "max_output_tokens": 4_096,
                    "capabilities": {"vision": false, "tools": true, "reasoning": true}
                }));
        }
        let models =
            crate::harness::LaunchModels::from_listing(&body, &crate::harness::tests::cloud(None))
                .unwrap();
        let settings = picker_settings(&models);
        assert!(settings.len() > 8_191);
        let home = picker_home("many");
        let path =
            write_picker_settings(&home, 30, launch_nanos(), &settings, |_| Ok(true)).unwrap();
        assert!(path.as_os_str().len() < 300, "{}", path.display());
        assert_eq!(fs::read_to_string(&path).unwrap(), settings);
        let _ = fs::remove_dir_all(&home);
    }

    // A reused daemon must hold the alias models too, so its agents'
    // `opus`, `sonnet` and `fable` run the same models as this launch's.
    #[test]
    fn a_reused_daemon_must_hold_the_alias_models() {
        let models = crate::harness::tests::hosted_models();
        let switches = cloud_launch_switches(&models, false);
        for (name, value) in alias_env(&models) {
            assert!(
                switches.contains(&(name, Some(value.clone()))),
                "{name}={value} in {switches:?}"
            );
        }
    }

    /// `(subagent model, force, haiku model)` for a key's data mode (`None`:
    /// standard).
    fn roles(mode: Option<&str>) -> (Option<String>, Option<String>, Option<String>) {
        let models = crate::harness::LaunchModels::from_listing(
            &crate::harness::tests::listing(mode),
            &crate::harness::tests::cloud(mode),
        )
        .unwrap();
        let switches = role_model_env_switches(&models);
        let value = |key: &str| {
            switches
                .iter()
                .find(|(name, _)| *name == key)
                .and_then(|(_, value)| value.clone())
        };
        (
            value(SUBAGENT_MODEL_ENV),
            value(SUBAGENT_MODEL_FORCE_ENV),
            value(HAIKU_MODEL_ENV),
        )
    }

    fn all(model: &str) -> (Option<String>, Option<String>, Option<String>) {
        (
            Some(model.to_owned()),
            Some("1".to_owned()),
            Some(model.to_owned()),
        )
    }

    // `rayline run claude`: ARC Swift for every subagent (Explore and Plan
    // included, hence the force) and background task.
    #[test]
    fn default_launch_puts_subagents_and_background_on_arc_swift() {
        assert_eq!(roles(None), all("arc-swift-1.0"));
        // A contributor or ZDR key gets ARC Swift under its mode's id.
        for mode in ["contributor", "zdr"] {
            assert_eq!(roles(Some(mode)), all(&format!("arc-swift-{mode}-1.0")));
        }
        let switches = role_model_env_switches(&crate::harness::tests::hosted_models());
        assert_eq!(switches.len(), 3);
        assert!(switches.iter().all(|(_, value)| value.is_some()));
    }
}

#[cfg(test)]
mod agent_view_env_tests {
    use super::agent_view_disabled_by;

    #[test]
    fn only_the_values_claude_code_reads_as_true_disable_the_agent_view() {
        for value in [
            "1", "true", "TRUE", "yes", "Yes", "on", "ON", " 1 ", "	true
",
        ] {
            assert!(agent_view_disabled_by(value), "{value:?}");
        }
        for value in [
            "", " ", "0", "false", "no", "off", "2", "-1", "enabled", "disable", "y", "t", "1.0",
            "true1",
        ] {
            assert!(!agent_view_disabled_by(value), "{value:?}");
        }
    }
}
