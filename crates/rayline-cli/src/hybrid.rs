//! The local process (spec 3.3): `rayline local start|status|stop`, and the
//! connection a local-mode `rayline run` makes to it, starting it first when
//! none is running.
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use rayline_local_router::ModelCapabilities;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::Settings;

/// How long `start` waits for a model's tokenizer files before it uses the
/// cached copy.
const TOKENIZER_FETCH_DEADLINE: Duration = Duration::from_secs(20);

/// The deadline for the core to initialize and report ready, including its
/// runtime profile checks, and for the Decisions readiness check.
const STARTUP_TIMEOUT_SECS: u64 = 120;

struct Paths {
    dir: PathBuf,
}
impl Paths {
    fn token(&self) -> PathBuf {
        self.dir.join("gateway-token")
    }
    fn ready(&self) -> PathBuf {
        self.dir.join("ready.json")
    }
    /// The token other local callers present to the local Routes API.
    fn routes_token(&self) -> PathBuf {
        self.dir.join(rayline_daemon::ROUTES_TOKEN_FILE)
    }
    fn log(&self) -> PathBuf {
        self.dir.join("runtime.log")
    }
    fn supervisor(&self) -> PathBuf {
        self.dir.join("supervisor.json")
    }
    fn core_config(&self) -> PathBuf {
        self.dir.join("core-config.json")
    }
    fn lock(&self) -> PathBuf {
        self.dir.join("runtime.lock")
    }
}

/// `~/.rayline/local/`, the local process's state (spec 8.2).
pub(crate) fn state_dir(home: &Path) -> PathBuf {
    home.join(crate::DOT_CONFIG_DIR).join("local")
}

/// `~/.rayline/local/harness/<name>/`, apart from cloud mode's folder because
/// each mode writes its own endpoint, key and model list.
fn harness_dir(home: &Path, harness: crate::harness::Harness) -> PathBuf {
    state_dir(home).join("harness").join(harness.name())
}

/// What `supervisor.json` records about the running local process.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Supervisor {
    pid: u32,
    /// The config file it was started from, canonical.
    config: PathBuf,
    /// The `--env` it was started with. `None` is prod.
    env: Option<String>,
    /// Where it gets its decisions. Absent in a record from a Rayline that
    /// didn't record them: such a process serves no run, and `rayline local
    /// stop` still stops it.
    #[serde(default)]
    router: Option<RouterSource>,
    /// `local.routing.decisions_model.id`. Absent in a record from a Rayline
    /// that didn't record it.
    #[serde(default)]
    decisions_model: Option<String>,
    /// `local.routing.classifier_model`: Claude Code runs only on a process
    /// started with one.
    #[serde(default)]
    classifier_model: Option<String>,
    /// Started from the `--router` defaults (no `local.routing` in its
    /// config): a `rayline run --mode local` without `local.routing` runs on
    /// it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    routing_defaults: bool,
}

/// `local.routing.router`, and for `local` the ARC release with the
/// built-in pin resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum RouterSource {
    Cloud,
    Local {
        repo: String,
        revision: String,
        manifest_sha256: String,
        dir: Option<String>,
    },
}

impl RouterSource {
    fn of(settings: &Settings) -> Self {
        let Some(routing) = settings
            .routing
            .as_ref()
            .filter(|routing| routing.router == crate::config::Router::Local)
        else {
            return Self::Cloud;
        };
        let release = routing
            .arc_release
            .clone()
            .unwrap_or_else(crate::arc_release::built_in);
        Self::Local {
            repo: release.repo,
            revision: release.revision,
            manifest_sha256: release.manifest_sha256,
            dir: release.dir,
        }
    }
}

impl std::fmt::Display for RouterSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cloud => f.write_str("cloud decisions"),
            Self::Local {
                repo,
                revision,
                manifest_sha256,
                dir,
            } => {
                write!(f, "local decisions from {repo} at {revision}")?;
                if let Some(dir) = dir {
                    write!(f, " in {dir}")?;
                }
                write!(f, " (manifest sha256 {manifest_sha256})")
            }
        }
    }
}

impl Supervisor {
    /// Refuses Claude Code on a process started without a classifier model,
    /// which its auto-mode safety checks need.
    fn check_runs(&self, harness: &str) -> io::Result<()> {
        if harness != "claude" {
            return Ok(());
        }
        // Every record this Rayline writes has `decisions_model`.
        if self.decisions_model.is_none() {
            return Err(io::Error::other(format!(
                "the local process (pid {}) was started by an earlier Rayline, which didn't \
                 record its classifier model; stop it with `rayline local stop` and run again",
                self.pid
            )));
        }
        if self.classifier_model.is_none() {
            return Err(io::Error::other(format!(
                "the local process (pid {}) was started without \
                 `local.routing.classifier_model`, which Claude Code needs; stop it with \
                 `rayline local stop` and run again",
                self.pid
            )));
        }
        Ok(())
    }

    /// Refuses a run whose config file or `--env` is not the running
    /// process's (spec 3.3 item 3): it never runs on another config.
    fn check_serves(&self, settings: &Settings) -> io::Result<()> {
        let config = canonical_config(settings)?;
        if self.config != config {
            return Err(io::Error::other(format!(
                "the local process (pid {}) runs from {}, not {}; stop it with `rayline local stop` first",
                self.pid,
                self.config.display(),
                config.display()
            )));
        }
        if self.env != settings.env {
            return Err(io::Error::other(format!(
                "the local process (pid {}) was started with {}, and this run has {}; stop it with `rayline local stop` first",
                self.pid,
                env_words(self.env.as_deref()),
                env_words(settings.env.as_deref())
            )));
        }
        let decisions = RouterSource::of(settings);
        if self.router.as_ref() != Some(&decisions) {
            let started = self.router.as_ref().map_or_else(
                || "decisions it didn't record (an earlier Rayline started it)".to_owned(),
                ToString::to_string,
            );
            return Err(io::Error::other(format!(
                "the local process (pid {}) was started with {started}, and this run has {decisions}; stop it with `rayline local stop` first",
                self.pid
            )));
        }
        Ok(())
    }
}

/// `--env <name>` as an error names it.
fn env_words(env: Option<&str>) -> String {
    match env {
        Some(name) => format!("`--env {name}`"),
        None => "no `--env`".to_owned(),
    }
}

/// The config file, canonical. The default file may not exist (an absent
/// default file is an empty config): its deepest existing directory is then
/// canonical and the rest appended, so the path matches the file's once it
/// is created.
fn canonical_config(settings: &Settings) -> io::Result<PathBuf> {
    let failed = |error: io::Error| {
        io::Error::new(
            error.kind(),
            format!("{}: {error}", settings.file.display()),
        )
    };
    let mut missing = Vec::new();
    let mut at = settings.file.as_path();
    loop {
        match fs::canonicalize(at) {
            Ok(mut path) => {
                path.extend(missing.iter().rev());
                return Ok(without_verbatim_prefix(path));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (at.parent(), at.file_name()) else {
                    return Err(failed(error));
                };
                missing.push(name);
                at = parent;
            }
            Err(error) => return Err(failed(error)),
        }
    }
}

/// `path` without the `\\?\` prefix Windows' canonical paths carry, where
/// the plain form names the same file: a drive path (`\\?\C:\x` is `C:\x`)
/// or a UNC share (`\\?\UNC\host\share` is `\\host\share`) within
/// `MAX_PATH`, with no component ending in `.` or a space (which plain paths
/// drop). Shown to the user and compared between runs, so it
/// reads as the user wrote it. Other paths, and every path elsewhere, are
/// unchanged.
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        if let Some(plain) = path.to_str().and_then(plain_windows_path) {
            return PathBuf::from(plain);
        }
    }
    path
}

fn plain_windows_path(verbatim: &str) -> Option<String> {
    // MAX_PATH less the terminating NUL.
    const MAX_PLAIN: usize = 259;
    let plain = if let Some(unc) = verbatim.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        let rest = verbatim.strip_prefix(r"\\?\")?;
        let bytes = rest.as_bytes();
        if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || &bytes[1..3] != br":\" {
            return None;
        }
        rest.to_owned()
    };
    let trimmed = plain
        .split('\\')
        .any(|component| component.ends_with('.') || component.ends_with(' '));
    (plain.encode_utf16().count() <= MAX_PLAIN && !trimmed).then_some(plain)
}

/// `rayline local start`: start the local process from `settings`.
pub(crate) async fn start(settings: &Settings) -> ExitCode {
    exit_code(
        async {
            let (home, paths) = local_paths()?;
            let _lock = acquire_lock(&paths.lock())?;
            if let Some(running) = running(&paths)? {
                return Err(io::Error::other(format!(
                    "the local process is already running (pid {}, config {}); stop it with `rayline local stop` first",
                    running.pid,
                    running.config.display()
                )));
            }
            // Progress lines only: a closed stdout (`| head`) must not stop
            // the launch halfway.
            launch(&home, &paths, settings, &|line| {
                let _ = io::Write::write_all(&mut io::stdout(), format!("{line}\n").as_bytes());
            })
            .await?;
            if let Some(routing) = &settings.routing
                && routing.classifier_model.is_none()
            {
                let _ = io::Write::write_all(
                    &mut io::stdout(),
                    format!(
                        "Note: Claude Code needs `local.routing.classifier_model` in {}, \
                         set to a model Rayline Platform serves:\n{}",
                        settings.file.display(),
                        routing.with_classifier_model()
                    )
                    .as_bytes(),
                );
            }
            Ok(())
        }
        .await,
    )
}

/// `rayline local status [--json]`.
pub(crate) async fn status(json: bool) -> ExitCode {
    exit_code(
        async {
            let (_, paths) = local_paths()?;
            let Some(running) = running(&paths)? else {
                if json {
                    println!("{}", json!({"running": false}));
                } else {
                    println!("Local process: not running");
                }
                return Ok(());
            };
            let state = ready(&paths)?;
            health(&gateway_client()?, &paths, &state).await?;
            if json {
                println!("{}", status_json(&running, &state, &paths)?);
                return Ok(());
            }
            println!(
                "Local process: running (pid {})\nAddress: {}\nConfig: {}\nEnvironment: {}",
                running.pid,
                url(&state)?,
                running.config.display(),
                running.env.as_deref().unwrap_or("prod")
            );
            Ok(())
        }
        .await,
    )
}

/// `rayline local status --json` of a running process (spec 3.3).
fn status_json(running: &Supervisor, state: &Value, paths: &Paths) -> io::Result<Value> {
    let mut status = json!({
        "running": true,
        "pid": running.pid,
        "url": url(state)?,
        "config": running.config,
        "environment": running.env.as_deref().unwrap_or("prod"),
        "router": match running.router {
            Some(RouterSource::Local { .. }) => "local",
            Some(RouterSource::Cloud) | None => "cloud",
        },
        "decisions_model": running.decisions_model,
    });
    if let Some(routes_url) = state.get("routes_url").and_then(Value::as_str) {
        status["routes"] = json!({
            "url": routes_url,
            "token_file": paths.routes_token(),
        });
    }
    Ok(status)
}

/// `rayline local stop`: stop the gateway and the execution core. External
/// model servers keep running.
pub(crate) async fn stop() -> ExitCode {
    exit_code(
        async {
            let (_, paths) = local_paths()?;
            let _lock = acquire_lock(&paths.lock())?;
            let Some(supervisor) = running(&paths)? else {
                clear_state(&paths)?;
                println!("Local process: not running");
                return Ok(());
            };
            let pid = i32::try_from(supervisor.pid).map_err(io::Error::other)?;
            let state = ready(&paths)?;
            // Shutdown remains available when core health fails. The private
            // gateway token authenticates this command independently.
            let shutdown: Value = gateway_client()?
                .post(format!("{}/_rayline/shutdown", url(&state)?))
                .bearer_auth(fs::read_to_string(paths.token())?)
                .json(&json!({}))
                .send()
                .await
                .map_err(io::Error::other)?
                .error_for_status()
                .map_err(io::Error::other)?
                .json()
                .await
                .map_err(io::Error::other)?;
            if shutdown.get("pid") != state.get("pid")
                || shutdown.get("instance_id") != state.get("instance_id")
                || shutdown.get("stopping") != Some(&json!(true))
            {
                return Err(io::Error::other("local process shutdown identity mismatch"));
            }
            // The local process gives its core up to 5 seconds to stop,
            // then the ARC encoder up to 3, and removes its state files, the
            // ready file last.
            let deadline = Instant::now() + Duration::from_secs(10);
            while paths.ready().exists() {
                if Instant::now() >= deadline {
                    return Err(io::Error::other(format!(
                        "the local process did not stop within 10s; see {}",
                        paths.log().display()
                    )));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            // Its state is gone; give the process up to 2 seconds to exit.
            // One that still looks alive after that (a zombie its parent
            // hasn't reaped, or a reused pid) doesn't fail the stop.
            let exit_by = Instant::now() + Duration::from_secs(2);
            while Instant::now() < exit_by && process_exists(pid).unwrap_or(false) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            fs::remove_file(paths.supervisor())?;
            println!("Local process stopped.");
            Ok(())
        }
        .await,
    )
}

/// A local-mode `rayline run <harness>`: connect `harness` to the local
/// process running from `settings`, starting it if none is running.
pub(crate) async fn run_local(settings: &Settings, harness: &str, args: &[OsString]) -> ExitCode {
    match run_harness(settings, harness, args).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn exit_code(result: io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Whether a local process is running, for the note a cloud-mode run prints.
/// Reads its state without creating anything; state it can't read is no.
pub(crate) fn local_process_running() -> bool {
    crate::home().is_ok_and(|home| running_in(&state_dir(&home)))
}

/// Runs `act` when no local process of `home` runs or starts: none has
/// state, or the runtime lock is free (taken without waiting, held while
/// `act` runs) and none is running. Otherwise `act` doesn't run.
pub(crate) fn while_no_local_process(home: &Path, act: impl FnOnce()) {
    let paths = Paths {
        dir: state_dir(home),
    };
    if !paths.dir.exists() {
        act();
        return;
    }
    if let Ok(Some(_lock)) = lock_file(&paths.lock(), false)
        && !running_in(&paths.dir)
    {
        act();
    }
}

fn running_in(dir: &Path) -> bool {
    matches!(
        running(&Paths {
            dir: dir.to_path_buf()
        }),
        Ok(Some(_))
    )
}

/// The router of the running local process when it was started from the
/// `--router` defaults, for a `rayline run --mode local` to load them. Reads its
/// state without creating anything; state it can't read is none.
pub(crate) fn running_from_routing_defaults() -> Option<crate::config::Router> {
    let home = crate::home().ok()?;
    match running(&Paths {
        dir: state_dir(&home),
    }) {
        Ok(Some(Supervisor {
            routing_defaults: true,
            router: Some(router),
            ..
        })) => Some(match router {
            RouterSource::Cloud => crate::config::Router::Cloud,
            RouterSource::Local { .. } => crate::config::Router::Local,
        }),
        _ => None,
    }
}

/// Refuses a run's launch from the `--router` defaults: they serve a
/// run only on the process started from them, which has stopped since the
/// run loaded them.
fn may_launch(settings: &Settings) -> io::Result<()> {
    if settings
        .routing
        .as_ref()
        .is_some_and(|routing| routing.defaults)
    {
        return Err(io::Error::other(format!(
            "{}: `local.routing` is required in local mode",
            settings.file.display()
        )));
    }
    Ok(())
}

/// The home directory and the local process's state directory, created
/// owner-only.
fn local_paths() -> io::Result<(PathBuf, Paths)> {
    require_supported_os()?;
    let home = crate::home().map_err(io::Error::other)?;
    let paths = Paths {
        dir: state_dir(&home),
    };
    private_dir(&paths.dir)?;
    Ok((home, paths))
}

async fn run_harness(
    settings: &Settings,
    harness: &str,
    args: &[OsString],
) -> io::Result<ExitCode> {
    require_local_args(harness, args)?;
    if local_decisions(settings) && !reads_anthropic_messages(harness) {
        return Err(io::Error::other(format!(
            "local ARC decisions read only Claude Code, OpenCode, Pi and omp turns, not \
             {harness}'s; set `local.routing.router` to `cloud` to run {harness}"
        )));
    }
    if harness == "claude"
        && let Some(routing) = &settings.routing
        && routing.classifier_model.is_none()
    {
        return Err(io::Error::other(format!(
            "{}: `local.routing.classifier_model` is required to run Claude Code in local mode: \
             it serves Claude Code's auto-mode safety checks. Set it to a model Rayline \
             Platform serves:\n{}",
            settings.file.display(),
            routing.with_classifier_model().trim_end()
        )));
    }
    if harness == "pi" {
        require_local_pi(&endpoint_key_envs(settings))?;
    }
    let (home, paths) = local_paths()?;
    {
        let _lock = acquire_lock(&paths.lock())?;
        match running(&paths)? {
            Some(running) => {
                running.check_serves(settings)?;
                running.check_runs(harness)?;
            }
            // Progress goes to stderr: the harness owns stdout.
            None => {
                may_launch(settings)?;
                launch(&home, &paths, settings, &|line| eprintln!("{line}")).await?;
            }
        }
    }
    let client = gateway_client()?;
    let state = ready(&paths)?;
    health(&client, &paths, &state).await?;
    let token = fs::read_to_string(paths.token())?;
    let withheld = endpoint_key_envs(settings);
    let mut command = harness_command(
        &home,
        harness,
        args,
        url(&state)?,
        &token,
        &harness_capabilities(model_capabilities(&state)?),
        &withheld,
    )?;
    for name in withheld {
        // A harness may use the same variable name for its local gateway
        // token. Preserve that explicit replacement, never the inherited key.
        let replaced_with_gateway = command
            .get_envs()
            .any(|(key, value)| key == name && value == Some(std::ffi::OsStr::new(&token)));
        if !replaced_with_gateway {
            command.env_remove(name);
        }
    }
    // Here, not in harness_command: preparing may run Hermes, and the
    // probe and repair must carry the launch's final environment.
    if harness == "hermes" {
        crate::harness::hermes::prepare_launch(&command).map_err(io::Error::other)?;
    }
    // In print mode Claude Code runs in Rayline's place, as a cloud launch
    // does: signals reach it directly and its exit is Rayline's.
    #[cfg(unix)]
    if harness == "claude" && crate::claude::print_mode_flag(args).is_some() {
        // `--model rayline-auto` names the model here.
        crate::claude_print::relay_stderr(&mut command, &[crate::VIRTUAL_ROUTER_MODEL]);
        return Ok(crate::exec_or_status(&mut command, "claude"));
    }
    let status = command.status()?;
    Ok(crate::child_exit_code(status))
}

fn gateway_client() -> io::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)
}

fn require_supported_os() -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                "cannot determine macOS version for the local process",
            ));
        }
        let version = String::from_utf8(output.stdout).map_err(io::Error::other)?;
        let major = version
            .trim()
            .split('.')
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(|| io::Error::other("invalid macOS version for the local process"))?;
        if major < 13 {
            return Err(io::Error::other(
                "the local process requires macOS 13 Ventura or later",
            ));
        }
    }
    Ok(())
}

/// Start the local process from `settings` and wait until it is ready.
/// The caller holds the runtime lock and has checked that none is running.
/// `say` reports progress.
async fn launch(
    home: &Path,
    paths: &Paths,
    settings: &Settings,
    say: &dyn Fn(String),
) -> io::Result<()> {
    let config_file = canonical_config(settings)?;
    let core = installed_binary("rayline-core")?;
    let routing = settings
        .routing
        .as_ref()
        .expect("a local-mode load has routing");
    let hosted = crate::status::hosted_environment(settings);
    let config =
        crate::hybrid_profile::core_config(settings, &hosted.base_url).map_err(io::Error::other)?;
    // With `local.routing.router` `local`, the local process runs the ARC
    // encoder and a loopback routes service, and points the core's Decisions
    // at it; the config keeps the router, which the classifier still uses.
    // The local process refuses Decisions anywhere but this router or a
    // loopback address; refuse a router it would refuse here, by name.
    rayline_local_router::decisions_url::check_config(&config, [hosted.base_url.as_str()])
        .map_err(|error| {
            io::Error::other(format!(
                "the router of environment {}: {error}",
                hosted.name()
            ))
        })?;
    // The environment's saved router key, minted on first use after
    // `rayline auth login`. It is the only key the core gets from Rayline;
    // every endpoint's own key comes from its `api_key_env`. Loaded before
    // anything changes state or downloads, so signed out fails first.
    let router_key = crate::status::router_key(&hosted, home)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    // The downloads are blocking HTTP, which may not run on the async runtime.
    let local_arc = {
        let (home, routing) = (home.to_path_buf(), routing.clone());
        tokio::task::spawn_blocking(move || local_arc(&home, &routing))
            .await
            .map_err(io::Error::other)??
    };
    clear_state(paths)?;
    let mut token_counters = Vec::new();
    for (id, repo, deployment) in crate::hybrid_profile::counted_deployments(settings) {
        let hub = rayline_hf::hf_cache_dir(home).map_err(io::Error::other)?;
        let snapshot = tokenizer_snapshot(&hub, repo).await.map_err(|error| {
            io::Error::other(format!(
                "{}: `{}.tokenizer` cannot load {repo}: {error}",
                settings.file.display(),
                crate::config::child(crate::config::MODELS, id)
            ))
        })?;
        if snapshot.cached {
            say(format!(
                "Prompt counter for {id}: {repo}, the cached copy (Hugging Face did not serve the current one)"
            ));
        } else {
            say(format!("Prompt counter for {id}: {repo}"));
        }
        token_counters.push((deployment, snapshot.dir));
    }
    write_private_json(&paths.core_config(), &config, true)?;
    match &local_arc {
        Some(arc) => say(format!(
            "Router: local ARC ({}), {}",
            routing.decisions_model.id, arc.source
        )),
        None => say(format!(
            "Router: cloud ARC ({}) via {}",
            routing.decisions_model.id,
            config["decisions"]["url"]
                .as_str()
                .unwrap_or(&hosted.base_url)
        )),
    }
    say(format!("Routing policy: {}", routing.policy.preset));
    for (id, model) in &settings.models {
        say(format!(
            "Model {id}: {} at {}",
            model.model, settings.endpoints[&model.endpoint].base_url
        ));
    }
    if let Some(classifier) = &routing.classifier_model {
        say(format!(
            "Auto-mode classifier: {classifier} (Claude Code's safety checks)"
        ));
    }
    say(format!("Rayline key: the saved key for {}", hosted.name()));
    // One router call before reporting ready: a refused key or an
    // unreachable router fails here, not on the harness's first turn. Local
    // decisions never ask the router, so its outage must not stop them.
    if local_arc.is_none() {
        probe_decisions(&config, &router_key, settings.env.as_deref(), say).await?;
    }
    let log = private_log(&paths.log())?;
    // Where this start's lines begin in the append-only log.
    let log_start = log.metadata()?.len();
    let mut command = Command::new(current_exe()?);
    command
        .arg(crate::cli::LOCAL_PROCESS)
        .arg("--core")
        .arg(core)
        .arg("--config")
        .arg(paths.core_config())
        .arg("--startup-timeout-secs")
        .arg(STARTUP_TIMEOUT_SECS.to_string())
        .arg("--token-file")
        .arg(paths.token())
        .arg("--ready-file")
        .arg(paths.ready())
        .arg("--router-url")
        .arg(&hosted.base_url)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    if let Some(env_name) = &settings.env {
        command.arg("--env-name").arg(env_name);
    }
    if let Some(classifier) = &routing.classifier_model {
        command.arg("--classifier-model").arg(classifier);
    }
    if let Some(arc) = &local_arc {
        command
            .arg("--arc-encoder")
            .arg(&arc.encoder)
            .arg("--arc-model")
            .arg(&arc.model)
            .arg("--arc-release")
            .arg(&arc.release);
    }
    for (deployment, dir) in &token_counters {
        command.arg("--token-counter-dir").arg(deployment).arg(dir);
    }
    command.env(crate::hybrid_profile::ROUTER_KEY_ENV, &router_key);
    let mut child = crate::detach::spawn_detached(&mut command)?;
    let progress = crate::start_progress::Progress::stderr();
    progress.phase(if local_arc.is_some() {
        "Starting the local process (ARC encoder and execution core)..."
    } else {
        "Starting the local process..."
    });
    let startup = async {
        let supervisor = Supervisor {
            pid: child.id(),
            config: config_file,
            env: settings.env.clone(),
            router: Some(RouterSource::of(settings)),
            decisions_model: settings
                .routing
                .as_ref()
                .map(|routing| routing.decisions_model.id.clone()),
            classifier_model: settings
                .routing
                .as_ref()
                .and_then(|routing| routing.classifier_model.clone()),
            routing_defaults: settings
                .routing
                .as_ref()
                .is_some_and(|routing| routing.defaults),
        };
        write_private_json(
            &paths.supervisor(),
            &serde_json::to_value(&supervisor).map_err(io::Error::other)?,
            false,
        )?;
        let client = gateway_client()?;
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                let exited = format!(
                    "the local process exited ({status}); see {}",
                    paths.log().display()
                );
                return Err(io::Error::other(
                    match exit_reason(&paths.log(), log_start) {
                        Some(reason) => format!("{reason}\n{exited}"),
                        None => exited,
                    },
                ));
            }
            if paths.ready().exists() {
                let state = ready(paths)?;
                if state.get("pid").and_then(Value::as_u64) != Some(u64::from(child.id())) {
                    return Err(io::Error::other("local process startup identity mismatch"));
                }
                health(&client, paths, &state).await?;
                progress.clear();
                for line in skipped_models(&state) {
                    say(line);
                }
                say(format!(
                    "Local process ready at {} (started in {:.1} s)\nLogs: {}",
                    url(&state)?,
                    started.elapsed().as_secs_f64(),
                    paths.log().display()
                ));
                return Ok(());
            }
            if started.elapsed() > Duration::from_secs(STARTUP_TIMEOUT_SECS + 5) {
                return Err(io::Error::other(format!(
                    "the local process did not start within {STARTUP_TIMEOUT_SECS}s; see {}",
                    paths.log().display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    .await;
    if let Err(error) = startup {
        progress.clear();
        stop_spawned(&mut child).await?;
        clear_state(paths)?;
        return Err(error);
    }
    Ok(())
}

/// The snapshot of `repo`'s tokenizer and chat template, fetched from Hugging
/// Face within [`TOKENIZER_FETCH_DEADLINE`], else the cached copy.
async fn tokenizer_snapshot(
    hub: &Path,
    repo: &str,
) -> Result<crate::hybrid_tokenizer::Snapshot, String> {
    // A detached thread, not a blocking task: the runtime waits for blocking
    // tasks at exit, and the downloader retries for far longer than `start`
    // should run.
    let (sender, fetching) = tokio::sync::oneshot::channel();
    std::thread::spawn({
        let hub = hub.to_owned();
        let repo = repo.to_owned();
        move || {
            let _ = sender.send(crate::hybrid_tokenizer::fetch(&hub, &repo));
        }
    });
    match tokio::time::timeout(TOKENIZER_FETCH_DEADLINE, fetching).await {
        Ok(Ok(fetched)) => fetched,
        Ok(Err(_)) => Err("the tokenizer download stopped".to_owned()),
        Err(_) => crate::hybrid_tokenizer::cached_or(
            hub,
            repo,
            format!(
                "Hugging Face did not answer within {}s and nothing is cached",
                TOKENIZER_FETCH_DEADLINE.as_secs()
            ),
        ),
    }
}

/// `GET /v1/models` on the Decisions router with the key the core will use,
/// before the local process starts: a refused key or an unreachable router
/// fails at start, not on the harness's first turn. Skipped for a loopback
/// Decisions URL.
async fn probe_decisions(
    config: &Value,
    key: &str,
    env: Option<&str>,
    say: &dyn Fn(String),
) -> io::Result<()> {
    let env_flag = crate::status::env_flag(env);
    let url = config["decisions"]["url"]
        .as_str()
        .expect("core_config always names the Decisions URL");
    // A loopback Decisions service is a local fixture or development
    // service, not a router this check can describe.
    if reqwest::Url::parse(url).is_ok_and(|url| {
        url.host_str().is_some_and(|host| {
            rayline_local_router::decisions_url::is_loopback(&host.to_ascii_lowercase())
        })
    }) {
        return Ok(());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)?;
    let models = models_url(url);
    let deadline = Instant::now() + Duration::from_secs(STARTUP_TIMEOUT_SECS);
    let mut announced = false;
    loop {
        let outcome = match probe_request(&client, &models, key).send().await {
            Ok(response) => Ok(response.status().as_u16()),
            Err(error) => Err(error),
        };
        match probe_verdict(outcome.as_ref().copied().map_err(|_| ())) {
            ProbeVerdict::Ready => return Ok(()),
            ProbeVerdict::KeyRefused(status) => {
                return Err(io::Error::other(format!(
                    "The router at {models} refused the Rayline key (HTTP {status}). Run `rayline {env_flag}auth login`, then try again."
                )));
            }
            ProbeVerdict::RateLimited => {
                return Err(io::Error::other(format!(
                    "The router at {models} refused the readiness check with HTTP 429: the Rayline key is rate limited or out of quota; check your account, then retry"
                )));
            }
            ProbeVerdict::Rejected(status) => {
                return Err(io::Error::other(format!(
                    "The router at {models} answered HTTP {status} to a readiness check; the local process cannot start"
                )));
            }
            ProbeVerdict::Unavailable => {
                if Instant::now() >= deadline {
                    let detail = match &outcome {
                        Ok(status) => format!("HTTP {status}"),
                        Err(error) => error.to_string(),
                    };
                    return Err(io::Error::other(format!(
                        "The router at {models} is unreachable or not ready ({detail}); check your network, then retry"
                    )));
                }
                if !announced {
                    announced = true;
                    say(format!("Waiting for the router at {models}..."));
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

/// The router's `GET /v1/models` for `decisions_url`, its `/v1/routes`.
fn models_url(decisions_url: &str) -> String {
    let router = decisions_url
        .strip_suffix("/v1/routes")
        .expect("core_config puts Decisions at the router's /v1/routes");
    format!("{router}/v1/models")
}

/// The readiness request: a bodiless `GET` with the router key.
fn probe_request(client: &reqwest::Client, url: &str, key: &str) -> reqwest::RequestBuilder {
    client.get(url).bearer_auth(key)
}

#[derive(Debug, Eq, PartialEq)]
enum ProbeVerdict {
    Ready,
    KeyRefused(u16),
    RateLimited,
    Unavailable,
    Rejected(u16),
}

/// `Err(())`: no HTTP answer (connect error or timeout).
fn probe_verdict(outcome: Result<u16, ()>) -> ProbeVerdict {
    match outcome {
        Err(()) => ProbeVerdict::Unavailable,
        Ok(status) if (200..300).contains(&status) => ProbeVerdict::Ready,
        Ok(status @ (401 | 403)) => ProbeVerdict::KeyRefused(status),
        Ok(429) => ProbeVerdict::RateLimited,
        Ok(500 | 502 | 503 | 504) => ProbeVerdict::Unavailable,
        Ok(status) => ProbeVerdict::Rejected(status),
    }
}

/// This `rayline`, with symlinks resolved: the local process runs from it.
fn current_exe() -> io::Result<PathBuf> {
    fs::canonicalize(std::env::current_exe()?)
}

/// The local ARC files of `local.routing.router` `local`.
struct LocalArc {
    /// Where the decisions come from, for the start report.
    source: String,
    encoder: PathBuf,
    model: PathBuf,
    release: PathBuf,
}

/// The ARC encoder weights `rayline` downloads: the public release's text
/// encoder in GGUF, pinned by commit and checked by its SHA-256.
const ARC_REPO: &str = "rayline-ai/ARC-1.0";
pub(crate) const ARC_COMMIT: &str = "f16e93f2581b3690938e47206881c7926e1ff341";
const ARC_ENCODER_FILE: &str = "gguf/arc-1.0-20261111-encoder-bf16.gguf";
/// The size of [`ARC_ENCODER_FILE`].
const ARC_ENCODER_BYTES: u64 = 1_018_125_184;
const ARC_ENCODER_SHA256: &str = "6aa526ff86148c34977781117a7cf80987583f6797290ee7ea33c4a0fa7a8bb0";

/// The pinned encoder weights in the Hugging Face cache, downloaded on first
/// use and checked against [`ARC_ENCODER_SHA256`].
fn arc_encoder_weights() -> crate::arc_release::Pinned<'static> {
    crate::arc_release::Pinned {
        repo: ARC_REPO,
        revision: ARC_COMMIT,
        path: ARC_ENCODER_FILE.to_owned(),
        max_bytes: ARC_ENCODER_BYTES,
        sha256: ARC_ENCODER_SHA256.to_owned(),
    }
}

fn local_arc(home: &Path, routing: &crate::config::Routing) -> io::Result<Option<LocalArc>> {
    if routing.router == crate::config::Router::Cloud {
        return Ok(None);
    }
    // The platform and the NVIDIA driver are checked before anything is
    // downloaded. `RAYLINE_ARC_ENCODER` names a development encoder, which
    // may run without them.
    let encoder_override = std::env::var_os("RAYLINE_ARC_ENCODER").map(PathBuf::from);
    if encoder_override.is_none() {
        encoder_ships(ENCODER_SHIPS)?;
        #[cfg(target_os = "linux")]
        nvidia_driver("Linux", "libcuda.so.1", load_libcuda())?;
        #[cfg(windows)]
        nvidia_driver("Windows", "nvcuda.dll", find_nvcuda())?;
    }
    // Development overrides: `RAYLINE_ARC_MODEL` and `RAYLINE_ARC_ENCODER`
    // name the encoder weights and binary, `RAYLINE_ARC_RELEASE` a release
    // directory. Without them, the
    // pinned encoder and release are downloaded.
    let release_dir = std::env::var_os("RAYLINE_ARC_RELEASE").map(PathBuf::from);
    let model = std::env::var_os("RAYLINE_ARC_MODEL").map(PathBuf::from);
    // What is downloaded shows as one transfer: the encoder weights are
    // expected up front, and the release's files once its manifest is read.
    let progress = crate::start_progress::Progress::stderr();
    let hub = match (&release_dir, &model) {
        (Some(_), Some(_)) => None,
        _ => {
            progress.status("Checking the ARC release...");
            Some((
                rayline_hf::hf_cache_dir(home).map_err(io::Error::other)?,
                rayline_hf::hf_endpoint().map_err(io::Error::other)?,
            ))
        }
    };
    let weights = arc_encoder_weights();
    // Stamps of the cached files already hashed, in Rayline's own state: the
    // Hugging Face cache may be shared.
    let stamps = home.join(crate::DOT_CONFIG_DIR).join("arc-verified");
    if let (Some((hub, _)), None) = (&hub, &model) {
        crate::arc_release::expect_unless_cached(hub, &weights, &progress);
    }
    // What was downloaded is checked again once it has settled, which
    // records the stamps the next start reads instead of hashing.
    let mut recheck: Vec<Box<dyn FnOnce() -> io::Result<()> + '_>> = Vec::new();
    // The encoder of this version, fetched once, in the same transfer.
    let encoder = match encoder_override {
        Some(path) => path,
        None => {
            let encoder = version_encoder(home, &stamps, &progress)?;
            recheck.push(Box::new(|| encoder_check(home, &stamps).map(drop)));
            encoder
        }
    };
    let (release, source) = match (release_dir, &hub) {
        (Some(dir), _) => {
            let source = format!("release directory {}", dir.display());
            (dir, source)
        }
        (None, Some((hub, hub_url))) => {
            let pin = routing
                .arc_release
                .clone()
                .unwrap_or_else(crate::arc_release::built_in);
            let token = rayline_hf::hf_token(home).map_err(io::Error::other)?;
            let source = format!("release {} at {}", pin.repo, pin.revision);
            let stamps = &stamps;
            let fetch = move |progress: &crate::start_progress::Progress| {
                crate::arc_release::fetch(hub, stamps, hub_url, token.as_deref(), &pin, progress)
                    .map_err(|error| {
                        io::Error::other(format!("cannot load the ARC release: {error}"))
                    })
            };
            let dir = fetch(&progress)?;
            recheck.push(Box::new(move || {
                fetch(&crate::start_progress::Progress::silent()).map(drop)
            }));
            (dir, source)
        }
        (None, None) => unreachable!("a release to download has a hub"),
    };
    // The encoder weights download after the release, which is small and
    // refuses a pin it cannot meet first.
    let model = match (model, &hub) {
        (Some(model), _) => model,
        (None, Some((hub, hub_url))) => {
            let fetch = |progress: &crate::start_progress::Progress| {
                crate::arc_release::fetch_file(hub, &stamps, hub_url, None, &weights, progress)
                    .map_err(|error| {
                        io::Error::other(format!("cannot load the ARC encoder: {error}"))
                    })
            };
            let path = fetch(&progress)?;
            recheck.push(Box::new(move || {
                fetch(&crate::start_progress::Progress::silent()).map(drop)
            }));
            fs::canonicalize(path)?
        }
        (None, None) => unreachable!("weights to download have a hub"),
    };
    if progress.downloaded() {
        crate::arc_release::settle();
        for check in recheck {
            check()?;
        }
    }
    if hub.is_some() {
        progress.release_ready();
    }
    Ok(Some(LocalArc {
        source,
        encoder,
        model,
        // Beside the local process's state, which each start clears.
        release,
    }))
}

/// Whether Rayline releases ship the ARC encoder for this platform.
const ENCODER_SHIPS: bool = cfg!(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_arch = "x86_64")
));

/// Refuses `local` decisions where Rayline releases ship no encoder
/// (`ships` false).
fn encoder_ships(ships: bool) -> io::Result<()> {
    if ships {
        return Ok(());
    }
    let os = match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        os => os,
    };
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "`local` decisions need the ARC encoder, which Rayline releases ship only for \
             macOS arm64, and Linux (x86_64 and aarch64) and Windows x86_64 with an NVIDIA GPU, not for {os} {}; \
             to use the cloud router instead, run `rayline local start --router cloud`",
            std::env::consts::ARCH
        ),
    ))
}

/// This version's ARC encoder in `~/.rayline/arc/<version>/`, downloaded
/// from its release (with the signed SHA256SUMS `rayline update` checks)
/// when it isn't there or doesn't match, reporting to `progress`. Checked by
/// sha256 with `stamps` like the release's files.
fn version_encoder(
    home: &Path,
    stamps: &Path,
    progress: &crate::start_progress::Progress,
) -> io::Result<PathBuf> {
    let platform = crate::update::detect_platform_tag().map_err(io::Error::other)?;
    let version = crate::update::Version::parse(crate::RAYLINE_VERSION)
        .map_err(|_| io::Error::other(format!("invalid version {}", crate::RAYLINE_VERSION)))?;
    let dir = crate::update::encoder_dir(home, &version);
    let keys = crate::MINISIGN_PUBLIC_KEYS;
    let verified = |at: &Path, sha256: &str| crate::arc_release::verified(stamps, at, sha256);
    // No local process runs while one starts: the other versions' encoders
    // are no longer needed.
    crate::update::remove_other_encoders(&dir);
    if let Some(encoder) = crate::update::fetched_encoder(&dir, platform, keys, &verified) {
        return Ok(encoder);
    }
    tokio::runtime::Handle::current()
        .block_on(crate::update::fetch_encoder(
            crate::UPDATE_BASE_URL,
            &version,
            platform,
            &dir,
            keys,
            &crate::update::Report {
                on_len: &|len| progress.expect(len),
                on_bytes: &|bytes| progress.on_bytes(bytes),
                on_done: &|| progress.file_done(),
            },
        ))
        .map_err(|error| {
            io::Error::other(format!(
                "cannot download the ARC encoder of {} {}: {error}",
                crate::DISPLAY_NAME,
                crate::RAYLINE_VERSION
            ))
        })?;
    // Each file matched before it was moved in; `local_arc` checks them
    // again once they have settled.
    Ok(dir.join(crate::update::encoder_file_name()))
}

/// This version's encoder in `~/.rayline/arc/<version>/`, checked against
/// its signed SHA256SUMS with `stamps`.
fn encoder_check(home: &Path, stamps: &Path) -> io::Result<PathBuf> {
    let platform = crate::update::detect_platform_tag().map_err(io::Error::other)?;
    let version = crate::update::Version::parse(crate::RAYLINE_VERSION)
        .map_err(|_| io::Error::other(format!("invalid version {}", crate::RAYLINE_VERSION)))?;
    let dir = crate::update::encoder_dir(home, &version);
    let verified = |at: &Path, sha256: &str| crate::arc_release::verified(stamps, at, sha256);
    crate::update::fetched_encoder(&dir, platform, crate::MINISIGN_PUBLIC_KEYS, &verified)
        .ok_or_else(|| {
            io::Error::other(format!(
                "the ARC encoder downloaded into {} doesn't match its SHA256SUMS",
                dir.display()
            ))
        })
}

/// On Linux the encoder runs on an NVIDIA GPU through the driver's
/// `libcuda.so.1`. Loads it as the encoder will, so a machine without the
/// driver is refused before anything is downloaded.
#[cfg(target_os = "linux")]
fn load_libcuda() -> Result<(), String> {
    let name = c"libcuda.so.1";
    // SAFETY: `name` is a NUL-terminated C string that outlives the call;
    // dlopen returns a handle or null.
    let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    if handle.is_null() {
        // SAFETY: dlerror returns null or a NUL-terminated string that stays
        // valid until the next dl* call on this thread; it is copied at once.
        let error = unsafe { libc::dlerror() };
        return Err(if error.is_null() {
            "not found".to_owned()
        } else {
            // SAFETY: non-null, NUL-terminated, see above.
            unsafe { std::ffi::CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        });
    }
    // SAFETY: `handle` came from the dlopen above and is closed once.
    unsafe { libc::dlclose(handle) };
    Ok(())
}

/// On Windows the NVIDIA driver installs `nvcuda.dll` in System32, where
/// the encoder loads it from.
#[cfg(windows)]
fn find_nvcuda() -> Result<(), String> {
    let root = std::env::var_os("SystemRoot").ok_or("`SystemRoot` is not set")?;
    let path = PathBuf::from(root).join("System32").join("nvcuda.dll");
    if path.is_file() {
        Ok(())
    } else {
        Err(format!("no {}", path.display()))
    }
}

/// Refuses local decisions on `os` when the NVIDIA driver's `library`
/// couldn't be loaded.
#[cfg(any(target_os = "linux", windows, test))]
fn nvidia_driver(os: &str, library: &str, loaded: Result<(), String>) -> io::Result<()> {
    loaded.map_err(|error| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "`local` decisions on {os} need the NVIDIA driver, and its {library} can't be \
                 loaded ({error}); install the NVIDIA driver, or to use the cloud router \
                 instead, run `rayline local start --router cloud`"
            ),
        )
    })
}

/// `name`, installed beside [`current_exe`].
fn installed_binary(name: &str) -> io::Result<PathBuf> {
    let executable = current_exe()?;
    let directory = executable
        .parent()
        .ok_or_else(|| io::Error::other("rayline executable has no parent directory"))?;
    let path = directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    fs::canonicalize(&path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "installed {name} is missing at {}: {error}. Reinstall the complete release with `rayline update --force`",
                path.display()
            ),
        )
    })
}

/// A warning for each `local.inference.models` entry the local process's ARC decisions
/// skip, from its ready file.
fn skipped_models(state: &Value) -> Vec<String> {
    let Some(skipped) = state.get("skipped_models").and_then(Value::as_array) else {
        return Vec::new();
    };
    skipped
        .iter()
        .filter_map(|model| {
            let id = model.get("id")?.as_str()?;
            let why = match model.get("reason")?.as_str()? {
                "not_in_release" => "isn't in the ARC release's catalog",
                _ => "has no price in the file or the ARC release",
            };
            Some(format!(
                "Warning: `local.inference.models` entry {id} {why}; ARC won't route to it"
            ))
        })
        .collect()
}

fn ready(paths: &Paths) -> io::Result<Value> {
    let state: Value =
        serde_json::from_slice(&fs::read(paths.ready())?).map_err(io::Error::other)?;
    if state.get("contract_version") != Some(&json!(1)) || state.get("ready") != Some(&json!(true))
    {
        return Err(io::Error::other("incompatible local process metadata"));
    }
    model_capabilities(&state)?;
    if state
        .get("instance_id")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(io::Error::other("missing execution core instance identity"));
    }
    let endpoint = reqwest::Url::parse(url(&state)?).map_err(io::Error::other)?;
    if endpoint.scheme() != "http"
        || endpoint.host_str() != Some("127.0.0.1")
        || endpoint.port().is_none()
        || endpoint.path() != "/"
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
    {
        return Err(io::Error::other("invalid local process loopback endpoint"));
    }
    Ok(state)
}

/// Read-only connection for top: never creates state or starts a runtime.
/// The client's default credentials are safe only for this validated origin.
/// An error of kind `NotFound` means no local process runs, as `rayline local
/// status` decides it: no `supervisor.json`, or one naming an exited process,
/// whatever other state it left behind. While a process runs, `NotFound`
/// also means its `ready.json` is missing or names no live process.
pub(crate) async fn top_connection(
    state_dir: &Path,
) -> io::Result<(reqwest::Client, String, Value)> {
    let paths = Paths {
        dir: state_dir.to_owned(),
    };
    if !paths.supervisor().try_exists()? || running(&paths)?.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no local process is running",
        ));
    }
    let state = ready(&paths).map_err(|error| {
        io::Error::new(
            match error.kind() {
                io::ErrorKind::NotFound => io::ErrorKind::NotFound,
                _ => io::ErrorKind::Other,
            },
            "local process readiness is unavailable or invalid",
        )
    })?;
    for key in ["pid", "core_pid"] {
        let pid = state
            .get(key)
            .and_then(Value::as_i64)
            .and_then(|pid| i32::try_from(pid).ok())
            .ok_or_else(|| io::Error::other("the local process is not live"))?;
        if !process_exists(pid)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the local process is not live",
            ));
        }
    }
    let token = fs::read_to_string(paths.token())
        .map_err(|_| io::Error::other("local gateway credential is unavailable"))?;
    if token.trim().is_empty() {
        return Err(io::Error::other("local gateway credential is invalid"));
    }
    let mut authorization =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.trim()))
            .map_err(|_| io::Error::other("local gateway credential is invalid"))?;
    authorization.set_sensitive(true);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, authorization);
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .default_headers(headers)
        .build()
        .map_err(|_| io::Error::other("cannot initialize the local gateway connection"))?;
    health(&client, &paths, &state).await.map_err(|_| {
        io::Error::other("local gateway health/authentication or process identity check failed")
    })?;
    let endpoint = url(&state)?.trim_end_matches('/');
    Ok((client, format!("{endpoint}/v1/router/top/snapshot"), state))
}

/// The env var each endpoint's `api_key_env` names, whether or not a `models`
/// entry uses the endpoint (spec 7.2 item 6).
fn endpoint_key_envs(settings: &Settings) -> Vec<&str> {
    settings
        .endpoints
        .values()
        .filter_map(|endpoint| endpoint.api_key_env.as_deref())
        .collect()
}

fn model_capabilities(state: &Value) -> io::Result<ModelCapabilities> {
    let capabilities: ModelCapabilities = serde_json::from_value(
        state
            .get("capabilities")
            .ok_or_else(|| io::Error::other("missing local process model capabilities"))?
            .clone(),
    )
    .map_err(io::Error::other)?;
    capabilities.validate().map_err(io::Error::other)?;
    Ok(capabilities)
}

/// Claude Code's switch between the API's in-turn auto-mode checks and its
/// own classifier requests (code.claude.com auto-mode classifier docs).
const AUTO_MODE_SERVER_ENV: &str = "CLAUDE_CODE_AUTO_MODE_SERVER";

/// Context window every harness is told under hybrid routing. The runtime
/// carries any prompt up to this size: a turn too large for the local model
/// hands off to cloud, and a prompt larger than the cloud window is refused
/// ("prompt is too long") so the harness compacts.
pub(crate) const HARNESS_CONTEXT_WINDOW: u64 = 1_000_000;

/// The core's capabilities with [`HARNESS_CONTEXT_WINDOW`] as the window.
/// Output and modality limits stay the core's admitted ones.
fn harness_capabilities(mut capabilities: ModelCapabilities) -> ModelCapabilities {
    capabilities.context_window = HARNESS_CONTEXT_WINDOW;
    capabilities
}

fn url(state: &Value) -> io::Result<&str> {
    state
        .get("listen_url")
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other("missing gateway URL"))
}

async fn health(client: &reqwest::Client, paths: &Paths, state: &Value) -> io::Result<Value> {
    let health: Value = client
        .get(format!("{}/healthz", url(state)?))
        .bearer_auth(fs::read_to_string(paths.token())?)
        .send()
        .await
        .map_err(io::Error::other)?
        .error_for_status()
        .map_err(io::Error::other)?
        .json()
        .await
        .map_err(io::Error::other)?;
    if health.get("pid") != state.get("pid")
        || health.get("core_pid") != state.get("core_pid")
        || health.get("instance_id") != state.get("instance_id")
        || health.get("ready") != Some(&json!(true))
        || health.get("contract_version") != Some(&json!(1))
        || model_capabilities(&health)? != model_capabilities(state)?
    {
        return Err(io::Error::other("local process health/identity mismatch"));
    }
    Ok(health)
}

/// `program` without the credentials a local-mode harness must not inherit:
/// the router key, the core's token and each endpoint's key (`withheld`).
/// The launch and the harness's version probe both start from it.
fn withheld_command(program: &Path, withheld: &[&str]) -> Command {
    let mut command = Command::new(program);
    crate::codex::withhold_router_key(&mut command);
    command.env_remove("RAYLINE_EXEC_AUTH_TOKEN");
    for name in withheld {
        command.env_remove(name);
    }
    command
}

/// `withheld` names each endpoint's key variable. [`run_harness`] removes
/// them from the launch, except one the harness sets to `token`.
fn harness_command(
    home: &Path,
    name: &str,
    args: &[OsString],
    endpoint: &str,
    token: &str,
    capabilities: &ModelCapabilities,
    withheld: &[&str],
) -> io::Result<Command> {
    let program = crate::harness::launch_program(name, std::env::var_os("PATH"));
    let mut command = withheld_command(&program, withheld);
    match name {
        "claude" => {
            command
                .env("ANTHROPIC_BASE_URL", endpoint)
                .env("ANTHROPIC_AUTH_TOKEN", token)
                // Claude Code otherwise asks for its own output default
                // (32k), which the core refuses.
                .env(
                    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
                    capabilities.max_output_tokens.to_string(),
                )
                // `rayline-auto` is not in Claude Code's model catalog, so it
                // assumes a 200k window and auto-compacts long before the
                // gateway's. State the window the gateway advertises, as
                // `model_context_window` already does for codex.
                .env(
                    crate::claude::MAX_CONTEXT_TOKENS_ENV,
                    capabilities.context_window.to_string(),
                )
                .args(["--model", "rayline-auto"]);
            // Claude Code 2.1.321+ asks the API to judge auto mode's actions
            // inside each turn (`safeguards`, answered in `safeguard_results`).
            // A turn the local model serves carries no verdict, so every
            // checked action is denied. Its own classifier requests, which the
            // gateway sends to the cloud classifier pin, judge them instead.
            // Always `0`: a value in the user's environment is not read.
            command.env(AUTO_MODE_SERVER_ENV, "0");
            let recovery = crate::claude::refusal_recovery_values();
            crate::claude::apply_refusal_recovery_env(&mut command, &recovery);
            crate::claude::remove_claude_env(&mut command);
        }
        "codex" => {
            command.env("RAYLINE_HYBRID_GATEWAY_TOKEN", token);
            for setting in [
                "model_provider=\"rayline-local\"".to_owned(),
                "model=\"rayline-auto\"".to_owned(),
                "model_providers.rayline-local.name=\"Rayline local\"".to_owned(),
                format!("model_providers.rayline-local.base_url=\"{endpoint}/v1\""),
                "model_providers.rayline-local.wire_api=\"responses\"".to_owned(),
                "model_providers.rayline-local.env_key=\"RAYLINE_HYBRID_GATEWAY_TOKEN\"".to_owned(),
                format!("model_context_window={}", capabilities.context_window),
            ] {
                command.arg("-c").arg(setting);
            }
        }
        _ => {
            let harness = crate::harness::Harness::from_command(name).ok_or_else(|| {
                io::Error::other(format!("unsupported local-mode harness {name:?}"))
            })?;
            harness
                .require_version(withheld_command(&program, withheld))
                .map_err(io::Error::other)?;
            let major_version = harness
                .major_version(withheld_command(&program, withheld))
                .map_err(io::Error::other)?;
            let args = harness.hybrid_args(args, major_version);
            let config_dir = harness_dir(home, harness);
            private_dir(&config_dir)?;
            let ctx = crate::harness::RenderContext {
                key: token,
                router_url: endpoint,
                config_dir: &config_dir,
                capabilities: Some(capabilities),
                harness_major_version: major_version,
                models: crate::harness::LaunchModels::hybrid(capabilities),
            };
            let plan = harness.plan(&ctx).map_err(io::Error::other)?;
            crate::harness::write_files(&config_dir, &plan.files)?;
            command = crate::harness::build_command(&program, &plan, &args);
            command.env_remove("RAYLINE_EXEC_AUTH_TOKEN");
            return Ok(command);
        }
    }
    command.args(args);
    Ok(command)
}

/// Refuse a command line local mode can't run, before anything starts.
/// Whether `local.routing.router` is `local`.
fn local_decisions(settings: &Settings) -> bool {
    settings
        .routing
        .as_ref()
        .is_some_and(|routing| routing.router == crate::config::Router::Local)
}

/// Refuse, before anything starts, a pi local mode can't run: none on
/// `PATH`, or one older than [`crate::harness::pi::MIN_LOCAL_VERSION`]. The
/// version probe runs without the router key or `withheld`.
fn require_local_pi(withheld: &[&str]) -> io::Result<()> {
    let pi = crate::harness::Harness::Pi;
    let program = crate::harness::find_on_path(pi.binary()).ok_or_else(|| {
        io::Error::other(format!(
            "`{}` not found on PATH. {}",
            pi.binary(),
            pi.install_hint()
        ))
    })?;
    crate::harness::pi::require_local_version(withheld_command(&program, withheld))
        .map_err(io::Error::other)
}

/// Whether the harness speaks Anthropic Messages, the one request format
/// local ARC decisions read.
fn reads_anthropic_messages(harness: &str) -> bool {
    use crate::harness::Harness;
    match Harness::from_command(harness) {
        Some(Harness::Hermes | Harness::OpenClaw) => false,
        Some(Harness::OpenCode | Harness::Pi | Harness::Omp) => true,
        None => harness != "codex",
    }
}

fn require_local_args(harness: &str, args: &[OsString]) -> io::Result<()> {
    require_fresh_session(harness, args)?;
    match crate::harness::Harness::from_command(harness) {
        Some(harness) => harness.require_hybrid_args(args),
        None => Ok(()),
    }
}

fn require_fresh_session(harness: &str, args: &[OsString]) -> io::Result<()> {
    let codex_resume = harness == "codex"
        && (args
            .first()
            .is_some_and(|arg| arg == "resume" || arg == "fork")
            || (args.first().is_some_and(|arg| arg == "exec")
                && args.get(1).is_some_and(|arg| arg == "resume")));
    let resuming = codex_resume
        || args.iter().take_while(|arg| *arg != "--").any(|arg| {
            let text = arg.to_string_lossy();
            let flag = text.split('=').next().unwrap_or("");
            match harness {
                "claude" => matches!(
                    flag,
                    "--resume"
                        | "--continue"
                        | "--fork-session"
                        | "--resume-session-at"
                        | "-r"
                        | "-c"
                ),
                "pi" => matches!(
                    flag,
                    "--resume"
                        | "--continue"
                        | "--fork"
                        | "--session"
                        | "--session-id"
                        | "-r"
                        | "-c"
                ),
                "opencode" => matches!(flag, "--continue" | "--session" | "--fork" | "-c" | "-s"),
                _ => false,
            }
        });
    if resuming {
        return Err(io::Error::other(
            "local mode requires a fresh harness session; saved sessions do not contain the core's private-append ledger",
        ));
    }
    Ok(())
}

/// The running local process's record, or `None` when none is running: no
/// record, or one whose process has exited.
fn running(paths: &Paths) -> io::Result<Option<Supervisor>> {
    let bytes = match fs::read(paths.supervisor()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if paths.ready().exists() || paths.token().exists() {
                return Err(io::Error::other(format!(
                    "{} holds local process state without {}; inspect it before removing it",
                    paths.dir.display(),
                    paths.supervisor().display()
                )));
            }
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let invalid =
        |error: String| io::Error::other(format!("{}: {error}", paths.supervisor().display()));
    // Liveness first: a record of a process that has exited is stale
    // whatever else it holds.
    let record: Value =
        serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
    let pid = record
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or_else(|| {
            invalid(format!(
                "invalid pid {}",
                record.get("pid").unwrap_or(&Value::Null)
            ))
        })?;
    if !process_exists(pid)? {
        return Ok(None);
    }
    serde_json::from_value(record)
        .map(Some)
        .map_err(|error| invalid(error.to_string()))
}

/// Remove what a local process that is no longer running left behind.
fn clear_state(paths: &Paths) -> io::Result<()> {
    for path in [
        paths.ready(),
        paths.token(),
        paths.routes_token(),
        paths.supervisor(),
    ] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

async fn stop_spawned(child: &mut Child) -> io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    let pid = i32::try_from(child.id()).map_err(io::Error::other)?;
    match request_supervisor_stop(pid) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let deadline = Instant::now() + Duration::from_secs(7);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            force_stop_process_group(pid)?;
            child.wait()?;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// Ask a supervisor we spawned to stop and clean up after itself. Unix sends
/// SIGTERM; Windows sets the stop event the local process waits on. `NotFound` means it
/// is already gone.
fn request_supervisor_stop(pid: i32) -> io::Result<()> {
    #[cfg(unix)]
    {
        terminate_process(pid)
    }
    #[cfg(windows)]
    {
        let pid = u32::try_from(pid).map_err(io::Error::other)?;
        rayline_local_router::windows_state::request_stop(pid)
    }
}

fn private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    // Files created inside inherit the owner-only entry from the start.
    #[cfg(windows)]
    rayline_local_router::windows_state::restrict_to_owner(path, true)?;
    Ok(())
}

/// A composition error, naming the settings file when the settings caused it.
/// Replace `path` atomically with `value` as a 0600 file: a planted symlink
/// or a loosened file at `path` is replaced, never followed or kept.
fn write_private_json(path: &Path, value: &Value, pretty: bool) -> io::Result<()> {
    use std::io::Write;
    let contents = if pretty {
        serde_json::to_vec_pretty(value)
    } else {
        serde_json::to_vec(value)
    }
    .map_err(io::Error::other)?;
    // Into a new 0600 file beside `path`, fsynced, then renamed over it and
    // the directory fsynced.
    let dir = path.parent().unwrap_or(Path::new("."));
    private_dir(dir)?;
    let name = path
        .file_name()
        .map(std::ffi::OsStr::to_string_lossy)
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&tmp)?;
    let written = file.write_all(&contents).and_then(|()| file.sync_all());
    if let Err(error) = written.and_then(|()| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    #[cfg(windows)]
    rayline_local_router::windows_state::restrict_to_owner(path, false)?;
    Ok(())
}

/// The error the local process printed before it exited: the last
/// `Error: ` line in the log at or after `start`, read from at most its last
/// 64 KiB. `None` when it printed none, or the log can't be read.
fn exit_reason(log: &Path, start: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 64 * 1024;
    let mut file = fs::File::open(log).ok()?;
    let end = file.metadata().ok()?.len();
    let from = start.max(end.saturating_sub(TAIL));
    // A cut after `start` reads from the byte before it and drops through the
    // first newline, so it never takes part of a line for a whole one.
    let cut = from > start;
    file.seek(SeekFrom::Start(if cut { from - 1 } else { from }))
        .ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let lines = if cut {
        let newline = tail.iter().position(|&byte| byte == b'\n')?;
        &tail[newline + 1..]
    } else {
        &tail[..]
    };
    String::from_utf8_lossy(lines)
        .lines()
        .rev()
        .find_map(|line| line.trim_end().strip_prefix("Error: "))
        .filter(|reason| !reason.is_empty())
        .map(str::to_owned)
}

/// Open the append-only runtime log owner-only: never through a symlink, and
/// narrowed to 0600 even when an older file was looser.
fn private_log(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        // Open a planted link itself, not its target, so it is refused below.
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    rayline_local_router::windows_state::restrict_to_owner(path, false)?;
    Ok(file)
}

#[cfg(unix)]
fn process_exists(pid: i32) -> io::Result<bool> {
    if pid <= 0 {
        return Ok(false);
    }
    // Mirrors POSIX kill(pid, 0): no signal is delivered; errno tells
    // us whether the process is absent or merely owned by another user.
    // SAFETY: kill with signal 0 only checks the pid; it touches no memory.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return Ok(!process_is_zombie(pid));
    }
    Ok(std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[cfg(unix)]
fn process_is_zombie(pid: i32) -> bool {
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .ok()
        .and_then(|output| output.status.success().then_some(output.stdout))
        .map(|stdout| String::from_utf8_lossy(&stdout).contains('Z'))
        .unwrap_or(false)
}

#[cfg(unix)]
fn terminate_process(pid: i32) -> io::Result<()> {
    signal_process(pid, libc::SIGTERM)
}

#[cfg(unix)]
fn force_stop_process_group(pid: i32) -> io::Result<()> {
    // SAFETY: killpg takes plain integers and touches no memory.
    let result = unsafe { libc::killpg(pid, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error)
}

#[cfg(unix)]
fn signal_process(pid: i32, signal: i32) -> io::Result<()> {
    // SAFETY: kill takes plain integers and touches no memory.
    let result = unsafe { libc::kill(pid, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Err(io::Error::new(io::ErrorKind::NotFound, error));
    }
    Err(error)
}

/// Whether `pid` is running, asked of Windows directly: no subprocess, no
/// PATH. A check that fails is an error naming the pid, never an answer.
#[cfg(windows)]
pub(crate) fn process_exists(pid: i32) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    let Ok(id) = u32::try_from(pid) else {
        return Ok(false);
    };
    if id == 0 {
        return Ok(false);
    }
    // Waiting on a process handle needs SYNCHRONIZE.
    // SAFETY: OpenProcess takes plain values and returns a new handle or null.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            id,
        )
    };
    if handle.is_null() {
        return open_failure_answer(pid, io::Error::last_os_error());
    }
    // A zero timeout only reads the handle's state: signalled once the
    // process has exited.
    // SAFETY: `handle` is the open process handle from above.
    let wait = unsafe { WaitForSingleObject(handle, 0) };
    let wait_error = io::Error::last_os_error();
    // SAFETY: `handle` is open and is closed exactly once, here.
    unsafe { CloseHandle(handle) };
    match wait {
        WAIT_TIMEOUT => Ok(true),
        WAIT_OBJECT_0 => Ok(false),
        WAIT_FAILED => Err(probe_failed(pid, &wait_error)),
        other => Err(probe_failed(
            pid,
            &format!("unexpected wait result {other:#x}"),
        )),
    }
}

/// What a failed `OpenProcess` says: no such process, a process this user may
/// not open (so it exists), or an error naming the pid.
#[cfg(windows)]
fn open_failure_answer(pid: i32, error: io::Error) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};
    match error
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
    {
        Some(ERROR_INVALID_PARAMETER) => Ok(false),
        Some(ERROR_ACCESS_DENIED) => Ok(true),
        _ => Err(probe_failed(pid, &error)),
    }
}

#[cfg(windows)]
fn probe_failed(pid: i32, reason: &dyn std::fmt::Display) -> io::Error {
    io::Error::other(format!(
        "could not check whether process {pid} is running: {reason}"
    ))
}

#[cfg(windows)]
fn force_stop_process_group(pid: i32) -> io::Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "taskkill /T /F failed with status {status}"
        )))
    }
}

/// Acquire an exclusive advisory lock on `lock_path`, blocking until it is
/// available.  The lock is automatically released when the returned `File`
/// is dropped (i.e. when the guard goes out of scope or the process dies).
///
/// Unix uses `flock`; Windows uses `LockFileEx`, which blocks the same way and
/// is released when the handle closes.
#[cfg(unix)]
fn acquire_lock(lock_path: &Path) -> io::Result<std::fs::File> {
    lock_file(lock_path, true)?.ok_or_else(|| io::Error::other("the runtime lock is held"))
}

/// The lock file at `lock_path`, locked: waiting for it when `wait`,
/// otherwise `None` when another process holds it.
#[cfg(unix)]
fn lock_file(lock_path: &Path, wait: bool) -> io::Result<Option<std::fs::File>> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    let operation = if wait {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    // SAFETY: a valid file descriptor owned by `file` for this call.
    let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
    if result != 0 {
        let error = io::Error::last_os_error();
        if !wait && error.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error);
    }
    Ok(Some(file))
}

#[cfg(windows)]
fn acquire_lock(lock_path: &Path) -> io::Result<std::fs::File> {
    lock_file(lock_path, true)?.ok_or_else(|| io::Error::other("the runtime lock is held"))
}

/// The lock file at `lock_path`, locked: waiting for it when `wait`,
/// otherwise `None` when another process holds it.
#[cfg(windows)]
fn lock_file(lock_path: &Path, wait: bool) -> io::Result<Option<std::fs::File>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    // Lock the whole range; nothing reads or writes the lock file's bytes.
    // SAFETY: a valid file handle and a zeroed OVERLAPPED (offset 0), used
    // synchronously because the file was not opened for overlapped I/O.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            if wait {
                LOCKFILE_EXCLUSIVE_LOCK
            } else {
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY
            },
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if locked == 0 {
        let error = io::Error::last_os_error();
        // ERROR_LOCK_VIOLATION: another process holds the lock.
        if !wait && error.raw_os_error() == Some(33) {
            return Ok(None);
        }
        return Err(error);
    }
    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_start_shows_the_error_the_local_process_printed() {
        let log = temp_dir("exit-reason").join("runtime.log");
        fs::write(&log, "Error: an earlier start failed\n").unwrap();
        let start = fs::metadata(&log).unwrap().len();
        let mut file = fs::OpenOptions::new().append(true).open(&log).unwrap();
        std::io::Write::write_all(
            &mut file,
            b"\x1b[2m2026-11-16T13:00:00Z\x1b[0m INFO starting\r\nError: the ARC encoder found no GPU it can use\r\n",
        )
        .unwrap();
        assert_eq!(
            exit_reason(&log, start).as_deref(),
            Some("the ARC encoder found no GPU it can use")
        );
        // Only this start's lines count.
        fs::write(&log, "Error: an earlier start failed\nINFO starting\n").unwrap();
        assert_eq!(exit_reason(&log, start), None);
        // Past 64 KiB, a line the cut lands in is dropped, not read from the cut.
        let line = "INFO quoted: Error: not a reason\n";
        let filler = "x".repeat(64 * 1024 - line.len() + "INFO quoted: ".len() - 1);
        fs::write(&log, format!("{line}{filler}\n")).unwrap();
        assert_eq!(exit_reason(&log, 0), None);
        // A line that starts exactly at the cut is kept whole.
        let after = "x".repeat(64 * 1024 - "Error: the real one\n".len() - 1);
        fs::write(&log, format!("INFO a\nError: the real one\n{after}\n")).unwrap();
        assert_eq!(exit_reason(&log, 0).as_deref(), Some("the real one"));
    }

    #[test]
    fn a_windows_verbatim_path_reads_as_the_user_wrote_it() {
        assert_eq!(
            plain_windows_path(r"\\?\C:\Users\rltest\.config\rayline\settings.json").as_deref(),
            Some(r"C:\Users\rltest\.config\rayline\settings.json")
        );
        assert_eq!(
            plain_windows_path(r"\\?\UNC\host\share\settings.json").as_deref(),
            Some(r"\\host\share\settings.json")
        );
        assert_eq!(plain_windows_path(r"C:\already\plain"), None);
        assert_eq!(plain_windows_path(r"\\?\GLOBALROOT\Device\x"), None);
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        assert_eq!(plain_windows_path(&long), None);
        // Length is in UTF-16 units, as MAX_PATH counts.
        let wide = format!(r"\\?\C:\{}", "設".repeat(200));
        assert!(plain_windows_path(&wide).is_some());
        // A plain path would drop the trailing dot or space.
        assert_eq!(plain_windows_path(r"\\?\C:\cfg\settings.json."), None);
        assert_eq!(plain_windows_path(r"\\?\C:\cfg \settings.json"), None);
    }

    #[test]
    fn the_ready_file_s_skipped_models_become_one_warning_each() {
        let state = json!({"skipped_models": [
            {"id": "x/other", "reason": "not_in_release"},
            {"id": "x/known", "reason": "no_price"},
        ]});
        assert_eq!(
            skipped_models(&state),
            vec![
                "Warning: `local.inference.models` entry x/other isn't in the ARC release's catalog; ARC won't route to it",
                "Warning: `local.inference.models` entry x/known has no price in the file or the ARC release; ARC won't route to it",
            ]
        );
        assert!(skipped_models(&json!({"ready": true})).is_empty());
    }

    #[test]
    fn a_missing_installed_binary_is_an_error_naming_its_path() {
        let name = "rayline-no-such-binary";
        let path = fs::canonicalize(std::env::current_exe().unwrap())
            .unwrap()
            .with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        let error = installed_binary(name).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let message = error.to_string();
        assert!(
            message.starts_with(&format!(
                "installed {name} is missing at {}: ",
                path.display()
            )),
            "{message}"
        );
        assert!(
            message.ends_with("Reinstall the complete release with `rayline update --force`"),
            "{message}"
        );
    }

    #[test]
    fn cleanup_waits_for_no_local_process_to_run_or_start() {
        let home = temp_dir("idle");
        let ran = std::cell::Cell::new(0);
        // No local process state at all.
        while_no_local_process(&home, || ran.set(ran.get() + 1));
        assert_eq!(ran.get(), 1);
        assert!(!state_dir(&home).exists());
        let paths = Paths {
            dir: state_dir(&home),
        };
        fs::create_dir_all(&paths.dir).unwrap();
        while_no_local_process(&home, || ran.set(ran.get() + 1));
        assert_eq!(ran.get(), 2);
        // A start holds the runtime lock: nothing runs, and nothing waits.
        let held = acquire_lock(&paths.lock()).unwrap();
        while_no_local_process(&home, || ran.set(ran.get() + 1));
        assert_eq!(ran.get(), 2);
        drop(held);
        // A running process.
        fs::write(
            paths.supervisor(),
            json!({"pid": std::process::id(), "config": "/c.json"}).to_string(),
        )
        .unwrap();
        while_no_local_process(&home, || ran.set(ran.get() + 1));
        assert_eq!(ran.get(), 2);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn local_decisions_need_a_platform_releases_ship_the_encoder_for() {
        assert!(encoder_ships(true).is_ok());
        let unsupported = encoder_ships(false).unwrap_err();
        assert_eq!(unsupported.kind(), io::ErrorKind::Unsupported);
        let message = unsupported.to_string();
        assert!(
            message.contains(
                "ship only for macOS arm64, and Linux (x86_64 and aarch64) and Windows x86_64 with an NVIDIA GPU, not for "
            ),
            "{message}"
        );
        assert!(
            message.ends_with(
                "; to use the cloud router instead, run `rayline local start --router cloud`"
            ),
            "{message}"
        );
    }

    #[test]
    fn without_the_nvidia_driver_local_decisions_are_refused() {
        assert!(nvidia_driver("Linux", "libcuda.so.1", Ok(())).is_ok());
        let error = nvidia_driver(
            "Linux",
            "libcuda.so.1",
            Err("libcuda.so.1: cannot open shared object file".to_owned()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            error.to_string(),
            "`local` decisions on Linux need the NVIDIA driver, and its libcuda.so.1 can't be \
             loaded (libcuda.so.1: cannot open shared object file); install the NVIDIA driver, \
             or to use the cloud router instead, run `rayline local start --router cloud`"
        );
    }

    // The built-in prod takes no `--env`: `--env prod` selects a configured
    // `environments.prod` instead.
    #[test]
    fn commands_for_an_environment_name_its_env() {
        assert_eq!(crate::status::env_flag(None), "");
        assert_eq!(crate::status::env_flag(Some("dev")), "--env dev ");
        assert_eq!(crate::status::env_flag(Some("-dev")), "--env=-dev ");
    }

    /// Serializes the tests that set a process-wide env var.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Puts an `opencode` that reports version 2 first on `PATH`, so a hybrid
    /// OpenCode launch can read its version without OpenCode installed. Each
    /// run writes the names (not the values) of its environment to
    /// `env-names.txt` beside it. The caller holds ENV_LOCK; Drop restores
    /// `PATH`.
    struct FakeOpenCode {
        dir: PathBuf,
        prior: Option<std::ffi::OsString>,
    }

    impl FakeOpenCode {
        fn install() -> Self {
            let dir = temp_dir("fake-opencode");
            if cfg!(windows) {
                fs::write(
                    dir.join("opencode.cmd"),
                    "@(for /f \"delims==\" %%v in ('set') do @echo %%v) > \"%~dp0env-names.txt\"\r\n\
                     @echo opencode v2.0.25\r\n",
                )
                .unwrap();
            } else {
                let binary = dir.join("opencode");
                fs::write(
                    &binary,
                    "#!/bin/sh\nenv | cut -d= -f1 > \"$(dirname \"$0\")/env-names.txt\"\n\
                     echo 'opencode v2.0.25'\n",
                )
                .unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
            // An OpenClaw new enough for the config Rayline writes.
            if cfg!(windows) {
                fs::write(dir.join("openclaw.cmd"), "@echo OpenClaw 2026.11.3\r\n").unwrap();
            } else {
                let binary = dir.join("openclaw");
                fs::write(&binary, "#!/bin/sh\necho 'OpenClaw 2026.11.3'\n").unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
            let prior = std::env::var_os("PATH");
            let mut dirs = vec![dir.clone()];
            dirs.extend(prior.iter().flat_map(std::env::split_paths));
            // SAFETY: serialized by ENV_LOCK; restored by Drop.
            unsafe { std::env::set_var("PATH", std::env::join_paths(dirs).unwrap()) };
            Self { dir, prior }
        }

        /// The environment names of the last run.
        fn env_names(&self) -> Vec<String> {
            fs::read_to_string(self.dir.join("env-names.txt"))
                .unwrap()
                .lines()
                .map(|line| line.trim().to_owned())
                .collect()
        }
    }

    impl Drop for FakeOpenCode {
        fn drop(&mut self) {
            // SAFETY: serialized as above; ENV_LOCK is still held by the caller.
            unsafe {
                match &self.prior {
                    Some(prior) => std::env::set_var("PATH", prior),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "rayline-local-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn state_files_replace_a_planted_symlink_without_writing_through_it() {
        let dir = temp_dir("symlink");
        let state = dir.join("state");
        private_dir(&state).unwrap();
        let victim = dir.join("victim.json");
        fs::write(&victim, "keep").unwrap();
        for target in ["core-config.json", "supervisor.json"] {
            let path = state.join(target);
            std::os::unix::fs::symlink(&victim, &path).unwrap();
            write_private_json(&path, &json!({"ok": true}), true).unwrap();
            assert!(
                !fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            let written: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(written, json!({"ok": true}));
        }
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        // The append-only log refuses a planted symlink outright.
        std::os::unix::fs::symlink(&victim, state.join("runtime.log")).unwrap();
        assert!(private_log(&state.join("runtime.log")).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn state_files_end_up_owner_only_whatever_was_there() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let path = dir.join("core-config.json");
        fs::write(
            &path,
            "{\"stale\": true, \"padding\": \"longer than the new file\"}",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_private_json(&path, &json!({"fresh": 1}), false).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"fresh\":1}");
        let log = dir.join("runtime.log");
        fs::write(&log, "old\n").unwrap();
        fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).unwrap();
        drop(private_log(&log).unwrap());
        assert_eq!(
            fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_to_string(&log).unwrap(), "old\n");
        // No temporary file is left behind.
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn state_files_end_up_owner_only_on_windows() {
        use rayline_local_router::windows_state::is_owner_only;
        let dir = temp_dir("acl");
        let state = dir.join("state");
        private_dir(&state).unwrap();
        assert!(is_owner_only(&state).unwrap());
        let path = state.join("core-config.json");
        fs::write(&path, "{\"stale\": true}").unwrap();
        write_private_json(&path, &json!({"fresh": 1}), false).unwrap();
        assert!(is_owner_only(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"fresh\":1}");
        let log = state.join("runtime.log");
        fs::write(
            &log, "old
",
        )
        .unwrap();
        drop(private_log(&log).unwrap());
        assert!(is_owner_only(&log).unwrap());
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "old
"
        );
        // A folder is not a log file.
        fs::create_dir(state.join("folder.log")).unwrap();
        assert!(private_log(&state.join("folder.log")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn second_lock_holder_waits_for_the_first() {
        let dir = temp_dir("lock");
        let path = dir.join("runtime.lock");
        let first = acquire_lock(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || {
                let _second = acquire_lock(&path).unwrap();
                tx.send(()).unwrap();
            })
        };
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        drop(first);
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        waiter.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Every file under `dir`, with its bytes.
    fn files_under(dir: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let mut files = std::collections::BTreeMap::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(files_under(&path));
            } else {
                files.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
        files
    }

    /// Each mode writes its own endpoint, key and model list (spec 8.2), so a
    /// launch in one mode never rewrites what the other's harness reads.
    #[test]
    fn cloud_and_local_launches_write_apart() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _opencode = FakeOpenCode::install();
        let capabilities = ModelCapabilities {
            context_window: 16384,
            max_output_tokens: 8192,
            input_modalities: vec!["text".into()],
        };
        for harness in crate::harness::Harness::ALL {
            let home = temp_dir(harness.name());
            let cloud_dir = crate::harness::config_dir(&home, harness);
            let local_dir = harness_dir(&home, harness);
            assert!(
                !cloud_dir.starts_with(&local_dir) && !local_dir.starts_with(&cloud_dir),
                "{harness:?}"
            );
            let cloud = || {
                let ctx = crate::harness::tests::render_context(&cloud_dir);
                let plan = harness.plan(&ctx).unwrap();
                crate::harness::write_files(&cloud_dir, &plan.files).unwrap();
            };
            let local = || {
                harness_command(
                    &home,
                    harness.name(),
                    &[],
                    "http://127.0.0.1:1",
                    "local-token",
                    &capabilities,
                    &[],
                )
                .unwrap();
            };
            cloud();
            let cloud_files = files_under(&cloud_dir);
            local();
            let local_files = files_under(&local_dir);
            assert!(!local_files.is_empty(), "{harness:?}");
            assert_eq!(files_under(&cloud_dir), cloud_files, "{harness:?}");
            cloud();
            assert_eq!(files_under(&local_dir), local_files, "{harness:?}");
            fs::remove_dir_all(&home).unwrap();
        }
    }

    /// OpenCode's `--version` probe runs before the launch, from the same
    /// environment: no endpoint key, router key or core token reaches it.
    /// The endpoint keys come from settings, so the key of an endpoint that
    /// no `models` entry uses is withheld too.
    #[test]
    fn the_opencode_version_probe_withholds_what_the_launch_withholds() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let opencode = FakeOpenCode::install();
        let home = temp_dir("probe-env");
        let file = home.join("settings.json");
        fs::write(
            &file,
            r#"{"mode": "local", "local": {
              "routing": {"router": "cloud", "decisions_model": {"id": "arc-swift-1.0"},
                "classifier_model": "anthropic/claude-haiku-5.5",
                "policy": {"preset": "arc-policy.hybrid-default.v1"}},
              "inference": {
                "endpoints": {
                  "box": {"base_url": "http://127.0.0.1:1", "protocols": ["openai_chat"],
                    "api_key_env": "RAYLINE_TEST_USED_ENDPOINT_KEY"},
                  "spare": {"base_url": "http://127.0.0.1:2", "protocols": ["openai_chat"],
                    "api_key_env": "RAYLINE_TEST_UNUSED_ENDPOINT_KEY"}},
                "models": {"local/box-model": {"endpoint": "box", "model": "box-model",
                  "context_window": 16384, "max_output_tokens": 8192}}}}}"#,
        )
        .unwrap();
        let settings = crate::config::load(&crate::config::LoadOptions {
            command: crate::config::Command::LocalStart {
                config: Some(&file),
            },
            env: None,
            harness_protocol: None,
            router: None,
            routing_defaults: None,
        })
        .unwrap();
        let withheld = [
            "RAYLINE_TEST_USED_ENDPOINT_KEY",
            "RAYLINE_TEST_UNUSED_ENDPOINT_KEY",
            "RAYLINE_ROUTER_API_KEY",
            "RAYLINE_EXEC_AUTH_TOKEN",
        ];
        let kept = "RAYLINE_TEST_KEPT";
        let vars: Vec<_> = withheld.iter().chain([&kept]).copied().collect();
        let prior: Vec<_> = vars.iter().map(std::env::var_os).collect();
        for var in &vars {
            // SAFETY: serialized by ENV_LOCK, and tests run with
            // --test-threads=1 (AGENTS.md); restored below.
            unsafe { std::env::set_var(var, "test-value-not-a-real-key") };
        }
        let command = harness_command(
            &home,
            "opencode",
            &[],
            "http://127.0.0.1:1",
            "local-token",
            &ModelCapabilities {
                context_window: 16384,
                max_output_tokens: 8192,
                input_modalities: vec!["text".into()],
            },
            &endpoint_key_envs(&settings),
        );
        for (var, value) in vars.iter().zip(prior) {
            // SAFETY: as above.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(var, value),
                    None => std::env::remove_var(var),
                }
            }
        }
        command.unwrap();
        let names = opencode.env_names();
        assert!(names.iter().any(|name| name == kept), "{names:?}");
        for name in withheld {
            assert!(
                !names.iter().any(|seen| seen == name),
                "{name} reached the probe"
            );
        }
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn cloud_credentials_are_not_exported_to_harnesses() {
        for name in ["claude", "codex"] {
            let command = harness_command(
                Path::new("/unused"),
                name,
                &[],
                "http://127.0.0.1:20815",
                "local-token",
                &ModelCapabilities {
                    context_window: 16384,
                    max_output_tokens: 8192,
                    input_modalities: vec!["text".into()],
                },
                &[],
            )
            .unwrap();
            let env: std::collections::HashMap<_, _> = command.get_envs().collect();
            assert_eq!(
                env.get(std::ffi::OsStr::new("RAYLINE_ROUTER_API_KEY")),
                Some(&None)
            );
            assert_eq!(
                env.get(std::ffi::OsStr::new("RAYLINE_EXEC_AUTH_TOKEN")),
                Some(&None)
            );
        }
    }

    /// Every hybrid harness advertises the 1M harness window, the core's
    /// output cap and text-only input, never the hosted 32k output / image
    /// defaults the core refuses (#123).
    #[test]
    fn every_harness_advertises_the_admitted_limits() {
        // The launcher reads the fallback override from the process env.
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let core = ModelCapabilities {
            context_window: 98_304,
            max_output_tokens: 4096,
            input_modalities: vec!["text".into()],
        };
        let capabilities = harness_capabilities(core);
        assert_eq!(capabilities.context_window, 1_000_000);
        let _opencode = FakeOpenCode::install();
        let home = temp_dir("limits");
        let read = |relative: &str| fs::read_to_string(state_dir(&home).join(relative)).unwrap();
        for name in ["pi", "opencode", "omp", "hermes", "openclaw"] {
            harness_command(
                &home,
                name,
                &[],
                "http://127.0.0.1:20815",
                "local-token",
                &capabilities,
                &[],
            )
            .unwrap();
        }
        let json = |relative: &str| -> Value { serde_json::from_str(&read(relative)).unwrap() };
        let pi = &json("harness/pi/models.json")["providers"]["rayline"]["models"][0];
        assert_eq!(
            (&pi["contextWindow"], &pi["maxTokens"], &pi["input"]),
            (&json!(1_000_000), &json!(4096), &json!(["text"]))
        );
        let opencode = &json("harness/opencode/opencode.json")["provider"]["rayline"]["models"]["rayline-auto"];
        assert_eq!(
            opencode["limit"],
            json!({"context": 1_000_000, "output": 4096})
        );
        assert_eq!(opencode["modalities"]["input"], json!(["text"]));
        let openclaw = json("harness/openclaw/openclaw.json");
        for provider in openclaw["models"]["providers"]
            .as_object()
            .unwrap()
            .values()
        {
            let model = &provider["models"][0];
            assert_eq!(
                (
                    &model["contextWindow"],
                    &model["maxTokens"],
                    &model["input"]
                ),
                (&json!(1_000_000), &json!(4096), &json!(["text"]))
            );
        }
        let omp = read("harness/omp/models.yml");
        for line in [
            "contextWindow: 1000000",
            "maxTokens: 4096",
            "input: [\"text\"]",
        ] {
            assert!(omp.contains(line), "{omp}");
        }
        let hermes = read("harness/hermes/config.yaml");
        assert!(hermes.contains("context_length: 1000000"), "{hermes}");
        assert!(
            hermes.contains("provider: \"custom:rayline-resp\""),
            "{hermes}"
        );
        let claude = harness_command(
            &home,
            "claude",
            &[],
            "http://127.0.0.1:20815",
            "local-token",
            &capabilities,
            &[],
        )
        .unwrap();
        assert!(
            claude
                .get_envs()
                .any(|(key, value)| key == "CLAUDE_CODE_MAX_OUTPUT_TOKENS"
                    && value == Some(std::ffi::OsStr::new("4096")))
        );
        // `rayline-auto` is not in Claude Code's catalog, so the window it
        // may budget against is stated rather than assumed (200k).
        assert!(
            claude
                .get_envs()
                .any(|(key, value)| key == "CLAUDE_CODE_MAX_CONTEXT_TOKENS"
                    && value == Some(std::ffi::OsStr::new("1000000")))
        );
        // Auto mode's checks come from Claude Code's own classifier requests,
        // not from the API inside a turn the local model may serve, so the
        // child always gets `0`.
        let claude_env: std::collections::BTreeMap<_, _> = claude.get_envs().collect();
        assert_eq!(
            claude_env.get(std::ffi::OsStr::new(AUTO_MODE_SERVER_ENV)),
            Some(&Some(std::ffi::OsStr::new("0")))
        );
        // The same removals as a cloud launch.
        for name in [
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            assert_eq!(
                claude_env.get(std::ffi::OsStr::new(name)),
                Some(&None),
                "{name}"
            );
        }
        let codex = harness_command(
            &home,
            "codex",
            &[],
            "http://127.0.0.1:20815",
            "local-token",
            &capabilities,
            &[],
        )
        .unwrap();
        let args: Vec<_> = codex.get_args().map(|arg| arg.to_string_lossy()).collect();
        assert!(
            args.iter().any(|arg| arg == "model_context_window=1000000"),
            "{args:?}"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn decisions_probe_verdicts() {
        assert_eq!(probe_verdict(Ok(200)), ProbeVerdict::Ready);
        assert_eq!(probe_verdict(Ok(204)), ProbeVerdict::Ready);
        assert_eq!(probe_verdict(Ok(400)), ProbeVerdict::Rejected(400));
        assert_eq!(probe_verdict(Ok(409)), ProbeVerdict::Rejected(409));
        assert_eq!(probe_verdict(Ok(422)), ProbeVerdict::Rejected(422));
        assert_eq!(probe_verdict(Ok(401)), ProbeVerdict::KeyRefused(401));
        assert_eq!(probe_verdict(Ok(403)), ProbeVerdict::KeyRefused(403));
        for status in [500, 502, 503, 504] {
            assert_eq!(probe_verdict(Ok(status)), ProbeVerdict::Unavailable);
        }
        assert_eq!(probe_verdict(Err(())), ProbeVerdict::Unavailable);
        assert_eq!(probe_verdict(Ok(402)), ProbeVerdict::Rejected(402));
        assert_eq!(probe_verdict(Ok(404)), ProbeVerdict::Rejected(404));
        assert_eq!(probe_verdict(Ok(429)), ProbeVerdict::RateLimited);
    }

    #[test]
    fn the_probe_lists_models_on_the_decisions_router() {
        assert_eq!(
            models_url("https://api.rayline.ai/v1/routes"),
            "https://api.rayline.ai/v1/models"
        );
        assert_eq!(
            models_url("https://router.stage.example:8443/v1/routes"),
            "https://router.stage.example:8443/v1/models"
        );
    }

    /// The probe on the wire: a bodiless `GET /v1/models` with the key as a
    /// bearer token.
    #[tokio::test]
    async fn the_probe_is_a_bodiless_get_with_the_router_key() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/models", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            while !request.windows(4).any(|end| end == b"\r\n\r\n") {
                let read = stream.read(&mut buf).await.unwrap();
                assert!(read > 0, "the request ended before its headers");
                request.extend_from_slice(&buf[..read]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });
        let client = reqwest::Client::new();
        let status = probe_request(&client, &url, "rlk-probe-test")
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status.as_u16(), 200);
        let request = server.await.unwrap();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        let mut lines = head.lines();
        assert_eq!(lines.next(), Some("GET /v1/models HTTP/1.1"));
        let headers: Vec<String> = lines.map(str::to_ascii_lowercase).collect();
        assert!(headers.contains(&"authorization: bearer rlk-probe-test".to_owned()));
        assert!(
            !headers
                .iter()
                .any(|header| header.starts_with("content-length:")
                    || header.starts_with("content-type:")
                    || header.starts_with("transfer-encoding:")),
            "{head}"
        );
        assert_eq!(body, "");
    }

    /// The config file `name` in `dir`, with `local.routing` and an
    /// `environments.dev` entry, loaded in local mode for `env`.
    fn local_settings(dir: &Path, name: &str, env: Option<&str>) -> Settings {
        let file = dir.join(name);
        fs::write(
            &file,
            r#"{"mode": "local",
              "local": {"routing": {"router": "cloud", "decisions_model": {"id": "arc-swift-1.0"},
                "classifier_model": "anthropic/claude-haiku-5.5",
                "policy": {"preset": "arc-policy.hybrid-default.v1"}}},
              "environments": {"dev": {"base_url": "https://router.example.test",
                "login_url": "https://router.example.test/cli-auth"}}}"#,
        )
        .unwrap();
        crate::config::load(&crate::config::LoadOptions {
            command: crate::config::Command::LocalStart {
                config: Some(&file),
            },
            env,
            harness_protocol: None,
            router: None,
            routing_defaults: None,
        })
        .unwrap()
    }

    #[test]
    fn status_json_names_the_routes_url_and_token_file_with_local_decisions_only() {
        let paths = Paths {
            dir: PathBuf::from("/home/u/.rayline/local"),
        };
        let mut running = Supervisor {
            pid: 7,
            config: PathBuf::from("/home/u/.config/rayline/settings.json"),
            env: None,
            router: Some(RouterSource::Local {
                repo: "rayline-ai/ARC-1.0".to_owned(),
                revision: "0".repeat(40),
                manifest_sha256: "0".repeat(64),
                dir: None,
            }),
            decisions_model: Some("arc-swift-1.0".to_owned()),
            classifier_model: None,
            routing_defaults: false,
        };
        let state = json!({"listen_url": "http://127.0.0.1:5000",
                           "routes_url": "http://127.0.0.1:5001/v1/routes"});
        assert_eq!(
            status_json(&running, &state, &paths).unwrap(),
            json!({
                "running": true,
                "pid": 7,
                "url": "http://127.0.0.1:5000",
                "config": "/home/u/.config/rayline/settings.json",
                "environment": "prod",
                "router": "local",
                "decisions_model": "arc-swift-1.0",
                "routes": {
                    "url": "http://127.0.0.1:5001/v1/routes",
                    "token_file": paths.routes_token(),
                },
            })
        );
        running.router = Some(RouterSource::Cloud);
        let cloud = status_json(
            &running,
            &json!({"listen_url": "http://127.0.0.1:5000"}),
            &paths,
        )
        .unwrap();
        assert_eq!(cloud["router"], "cloud");
        assert!(cloud.get("routes").is_none());
    }

    #[test]
    fn claude_code_runs_only_on_a_process_started_with_a_classifier_model() {
        let mut running = Supervisor {
            pid: 7,
            config: PathBuf::from("/c.json"),
            env: None,
            router: Some(RouterSource::Cloud),
            decisions_model: None,
            classifier_model: None,
            routing_defaults: false,
        };
        let error = running.check_runs("claude").unwrap_err().to_string();
        assert!(error.contains("earlier Rayline"), "{error}");
        running.decisions_model = Some("arc-swift-1.0".to_owned());
        let error = running.check_runs("claude").unwrap_err().to_string();
        assert!(error.contains("routing.classifier_model"), "{error}");
        running.check_runs("pi").unwrap();
        running.classifier_model = Some("anthropic/claude-haiku-5.5".to_owned());
        running.check_runs("claude").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_absent_default_file_has_the_path_it_gets_once_created() {
        let dir = temp_dir("absent-config");
        let real = dir.join("real");
        fs::create_dir_all(&real).unwrap();
        let home = dir.join("home");
        std::os::unix::fs::symlink(&real, &home).unwrap();
        let mut settings = local_settings(&dir, "a.json", None);
        settings.file = home.join(".config/rayline/settings.json");
        let before = canonical_config(&settings).unwrap();
        fs::create_dir_all(home.join(".config/rayline")).unwrap();
        fs::write(&settings.file, "{}").unwrap();
        assert_eq!(canonical_config(&settings).unwrap(), before);
        assert!(
            before.starts_with(fs::canonicalize(&real).unwrap()),
            "{before:?}"
        );
    }

    #[tokio::test]
    async fn claude_code_needs_a_classifier_model_before_anything_starts() {
        let dir = temp_dir("no-classifier");
        let mut settings = local_settings(&dir, "a.json", None);
        if let Some(routing) = settings.routing.as_mut() {
            routing.classifier_model = None;
        }
        let error = run_harness(&settings, "claude", &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("routing.classifier_model"), "{error}");
        assert!(
            error.contains(r#""classifier_model": "<model id>""#),
            "{error}"
        );
    }

    #[test]
    fn a_run_never_starts_a_process_from_the_router_defaults() {
        let dir = temp_dir("defaults-gone");
        let mut settings = local_settings(&dir, "a.json", None);
        may_launch(&settings).unwrap();
        if let Some(routing) = settings.routing.as_mut() {
            routing.defaults = true;
        }
        let error = may_launch(&settings).unwrap_err().to_string();
        assert!(
            error.ends_with("`local.routing` is required in local mode"),
            "{error}"
        );
    }

    #[test]
    fn a_process_started_from_the_router_defaults_says_so_in_its_record() {
        let mut record = Supervisor {
            pid: 1,
            config: PathBuf::from("/c.json"),
            env: None,
            router: Some(RouterSource::Cloud),
            decisions_model: Some("d".to_owned()),
            classifier_model: None,
            routing_defaults: false,
        };
        // Other records are written as before.
        let written = serde_json::to_value(&record).unwrap();
        assert!(written.get("routing_defaults").is_none(), "{written}");
        assert_eq!(
            serde_json::from_value::<Supervisor>(written).unwrap(),
            record
        );
        record.routing_defaults = true;
        let written = serde_json::to_value(&record).unwrap();
        assert_eq!(written["routing_defaults"], true);
        assert_eq!(
            serde_json::from_value::<Supervisor>(written).unwrap(),
            record
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_process_check_answers_from_win32() {
        let own = i32::try_from(std::process::id()).unwrap();
        assert!(process_exists(own).unwrap());
        let mut child = Command::new("cmd.exe")
            .args(["/D", "/C", "exit 0"])
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        child.wait().unwrap();
        // Exited, while this handle still keeps the process object alive.
        assert!(!process_exists(pid).unwrap());
        drop(child);
        // Gone: no process has that pid.
        assert!(!process_exists(pid).unwrap());
        assert!(!process_exists(0).unwrap());
        assert!(!process_exists(-1).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn a_failed_windows_process_check_is_an_error_not_an_answer() {
        use windows_sys::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_NOT_ENOUGH_MEMORY,
        };
        let os = |code: u32| io::Error::from_raw_os_error(i32::try_from(code).unwrap());
        assert!(!open_failure_answer(42, os(ERROR_INVALID_PARAMETER)).unwrap());
        assert!(open_failure_answer(42, os(ERROR_ACCESS_DENIED)).unwrap());
        let error = open_failure_answer(42, os(ERROR_NOT_ENOUGH_MEMORY))
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("could not check whether process 42 is running: "),
            "{error}"
        );
    }

    #[test]
    fn local_decisions_read_only_anthropic_messages_harnesses() {
        for harness in ["claude", "opencode", "pi", "omp"] {
            assert!(reads_anthropic_messages(harness), "{harness}");
        }
        for harness in ["codex", "hermes", "openclaw"] {
            assert!(!reads_anthropic_messages(harness), "{harness}");
        }
    }

    #[tokio::test]
    async fn local_decisions_refuse_an_openai_format_harness_before_anything_starts() {
        let dir = temp_dir("openai-local");
        let mut settings = local_settings(&dir, "a.json", None);
        if let Some(routing) = settings.routing.as_mut() {
            routing.router = crate::config::Router::Local;
        }
        for harness in ["codex", "hermes", "openclaw"] {
            let error = run_harness(&settings, harness, &[])
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("`local.routing.router` to `cloud`"),
                "{error}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_on_another_config_or_env_is_refused() {
        let dir = temp_dir("serves");
        let started = local_settings(&dir, "a.json", None);
        let supervisor = Supervisor {
            pid: 7,
            config: canonical_config(&started).unwrap(),
            env: None,
            router: Some(RouterSource::of(&started)),
            decisions_model: None,
            classifier_model: None,
            routing_defaults: false,
        };
        supervisor.check_serves(&started).unwrap();
        assert_eq!(supervisor.router, Some(RouterSource::Cloud));

        // A record from a Rayline that didn't record decisions serves no run.
        let earlier: Supervisor =
            serde_json::from_str(r#"{"pid": 7, "config": "/c.json", "env": null}"#).unwrap();
        assert_eq!(earlier.router, None);
        let earlier = Supervisor {
            config: supervisor.config.clone(),
            ..earlier
        };
        assert!(
            earlier
                .check_serves(&started)
                .unwrap_err()
                .to_string()
                .contains("decisions it didn't record")
        );

        let mut local = local_settings(&dir, "a.json", None);
        if let Some(routing) = local.routing.as_mut() {
            routing.router = crate::config::Router::Local;
        }
        let built_in = crate::arc_release::built_in();
        assert_eq!(
            supervisor.check_serves(&local).unwrap_err().to_string(),
            format!(
                "the local process (pid 7) was started with cloud decisions, and this run has \
                 local decisions from {} at {} in runtime (manifest sha256 {}); stop it with \
                 `rayline local stop` first",
                built_in.repo, built_in.revision, built_in.manifest_sha256
            )
        );
        // Another release is another process.
        let mut pinned = local.clone();
        if let Some(routing) = pinned.routing.as_mut() {
            routing.arc_release = Some(crate::config::ArcRelease {
                revision: "1".repeat(40),
                ..built_in
            });
        }
        assert_ne!(RouterSource::of(&pinned), RouterSource::of(&local));

        let other = local_settings(&dir, "b.json", None);
        let error = supervisor.check_serves(&other).unwrap_err().to_string();
        for part in [
            "the local process (pid 7) runs from ",
            "a.json, not ",
            "b.json; stop it with `rayline local stop` first",
        ] {
            assert!(error.contains(part), "{error}");
        }

        let dev = local_settings(&dir, "a.json", Some("dev"));
        assert_eq!(
            supervisor.check_serves(&dev).unwrap_err().to_string(),
            "the local process (pid 7) was started with no `--env`, and this run has \
             `--env dev`; stop it with `rayline local stop` first"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_state_removes_a_stale_routes_token() {
        let dir = temp_dir("stale-routes-token");
        let paths = Paths { dir: dir.clone() };
        fs::write(paths.routes_token(), "stale").unwrap();
        fs::write(paths.ready(), "{}").unwrap();
        clear_state(&paths).unwrap();
        assert!(!paths.routes_token().exists());
        assert!(!paths.ready().exists());
        // Nothing to clear is not an error.
        clear_state(&paths).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cloud_run_notes_a_running_local_process_only_when_one_runs() {
        let dir = temp_dir("note");
        assert!(!running_in(&dir.join("missing")));
        assert!(!running_in(&dir));
        let paths = Paths { dir: dir.clone() };
        // State it can't make sense of is no local process, not an error.
        fs::write(paths.ready(), "{}").unwrap();
        assert!(!running_in(&dir));
        fs::remove_file(paths.ready()).unwrap();
        fs::write(
            paths.supervisor(),
            json!({"pid": std::process::id(), "config": "/c.json"}).to_string(),
        )
        .unwrap();
        assert!(running_in(&dir));
    }

    #[test]
    fn running_reads_the_record_of_a_live_process_only() {
        let dir = temp_dir("running");
        let paths = Paths { dir: dir.clone() };
        assert_eq!(running(&paths).unwrap(), None);

        fs::write(paths.ready(), "{}").unwrap();
        let error = running(&paths).unwrap_err().to_string();
        assert!(error.contains("supervisor.json"), "{error}");
        fs::remove_file(paths.ready()).unwrap();

        let record = |pid: u64| {
            fs::write(
                paths.supervisor(),
                json!({"pid": pid, "config": "/c.json", "env": "dev",
                       "router": {"mode": "cloud"}})
                .to_string(),
            )
            .unwrap();
            running(&paths)
        };
        assert_eq!(
            record(u64::from(std::process::id())).unwrap(),
            Some(Supervisor {
                pid: std::process::id(),
                config: PathBuf::from("/c.json"),
                env: Some("dev".to_owned()),
                router: Some(RouterSource::Cloud),
                decisions_model: None,
                classifier_model: None,
                routing_defaults: false,
            })
        );
        let error = record(u64::from(u32::MAX)).unwrap_err().to_string();
        assert!(error.contains("invalid pid"), "{error}");
        #[cfg(unix)]
        {
            let mut child = Command::new("true").spawn().unwrap();
            child.wait().unwrap();
            assert_eq!(record(u64::from(child.id())).unwrap(), None);
        }

        // A live process's record with an unknown field is an error.
        fs::write(
            paths.supervisor(),
            json!({"pid": std::process::id(), "config": "/c.json", "env": null, "port": 9})
                .to_string(),
        )
        .unwrap();
        let error = running(&paths).unwrap_err().to_string();
        assert!(error.contains("supervisor.json"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn historical_sessions_require_an_explicit_restore_contract() {
        for (harness, arg) in [
            ("claude", "--resume=saved"),
            ("claude", "--continue"),
            ("claude", "-r"),
            ("codex", "resume"),
            ("codex", "fork"),
            ("pi", "--fork"),
            ("pi", "--session=saved"),
            ("opencode", "-c"),
            ("opencode", "--continue"),
            ("opencode", "-s"),
            ("opencode", "--session=ses_1"),
            ("opencode", "--fork"),
        ] {
            assert!(require_fresh_session(harness, &[OsString::from(arg)]).is_err());
        }
        assert!(
            require_fresh_session(
                "claude",
                &[
                    OsString::from("-p"),
                    OsString::from("Explain resume support")
                ]
            )
            .is_ok()
        );
    }
}
