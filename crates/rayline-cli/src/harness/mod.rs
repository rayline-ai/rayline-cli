//! `rayline run opencode|pi|omp|hermes|openclaw`: launch a third-party coding harness
//! against the hosted Rayline router.
//!
//! Each launcher resolves the environment's router URL and `rlk-` key the same
//! way `rayline run claude` does, writes a Rayline-owned config under
//! `~/.rayline/harness/<name>/`, points the harness at it through
//! the harness's own config env var, and execs the harness with the remaining
//! args. The key lives only in that config (0600, in a 0700 dir), never in the
//! harness's environment: every tool subprocess the harness spawns inherits its
//! environment, so a key there would reach a model-visible `env` call. For the
//! same reason the launch removes `RAYLINE_ROUTER_API_KEY` from the child's
//! environment. The config dir is per harness (it holds the harness's
//! sessions), so two concurrent launches of one harness with different keys
//! share the last-written key.
//!
//! In cloud mode every config defaults to Anthropic Messages, except OpenClaw,
//! whose default provider speaks OpenAI Chat Completions; Hermes and OpenClaw
//! also get an OpenAI Responses provider beside the default. In local mode each
//! config sets up only its harness's one protocol (spec section 5.2): Hermes
//! Responses, OpenClaw Chat Completions. The models, their names and their
//! limits are the ones the router's `GET /v1/models` lists for the key, read
//! once per launch ([`fetch_hosted_models`]): the main agent defaults to
//! `cloud.model.id` and, where the harness has a separate small/background
//! model role, that role to `cloud.smol_model.id`, both as `settings.json`
//! writes them. Rayline doesn't check those ids against the listing: one it
//! doesn't have is offered by its id alone, with no name or limits.
//! Under a local-mode `rayline run <harness>` every renderer keeps `rayline-auto` (the execution
//! core's virtual model) and advertises the hybrid runtime's admitted limits
//! instead ([`RenderContext::capabilities`]): a harness that asks for more
//! output than the execution core admits has every turn refused.
//!
//! Episode keying: the router keys a routing episode on each harness's own
//! session id where it reads them ([`SESSION_AFFINITY_HEADER`]). opencode
//! sends it on its own; omp sends `X-Claude-Code-Session-Id` (apiKey mode from
//! omp 18.1.10); pi (on Messages) and OpenClaw (on Chat Completions) send it
//! once their config sets `compat.sendSessionAffinityHeaders`; Hermes (on
//! Messages) once its provider entry names the header in
//! `session_affinity_header` (from Hermes v2026.9.21). On Responses, pi's,
//! OpenClaw's and Hermes' `prompt_cache_key` keys the session instead. A harness session
//! is one episode, so `--resume` / `-c` / `--session-id` in a later launch
//! keeps the episode. These launchers talk to the hosted router directly.

pub(crate) mod hermes;
mod omp;
mod openclaw;
mod opencode;
pub(crate) mod pi;
mod yaml_merge;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::Value;

use crate::config::Cloud;

/// The router key a parent launch hands its child processes. Removed from the
/// harness's environment so it never reaches the harness's tools.
const ROUTER_KEY_ENV: &str = "RAYLINE_ROUTER_API_KEY";
/// Header a harness's own session id rides on, which the gateway keys the
/// episode on. Most harnesses send it under this name on their own; Hermes
/// needs the name written into its config ([`hermes::SESSION_AFFINITY_KEY`]).
pub const SESSION_AFFINITY_HEADER: &str = "x-session-affinity";
/// The execution core's virtual model under a local-mode `rayline run <harness>`.
pub const ROUTER_MODEL: &str = crate::VIRTUAL_ROUTER_MODEL;

/// A model's price in USD per million tokens, from the router's
/// `GET /v1/arc/prices` card.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// A model a rendered config offers.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LaunchModel {
    pub id: String,
    /// Its name and limits, which a config advertises. `None` for a
    /// `cloud` model id the listing doesn't have: a config offers that one by
    /// its id alone, and the harness falls back on its own defaults.
    pub details: Option<ModelDetails>,
    /// The ARC product the model runs (`product` in `GET /v1/models`), the
    /// same for one product in every data mode. `None` for a model that is
    /// no ARC product (hybrid's virtual model, or an unlisted id).
    pub product: Option<String>,
    /// Its row on the price card ([`LaunchModels::apply_arc_prices`]), for a
    /// harness that shows costs (omp); `None` when not read or not on it.
    pub price: Option<ModelPrice>,
}

/// What a config advertises about a model: from the listing in cloud mode,
/// from the hybrid runtime's admitted limits in local mode.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModelDetails {
    pub display_name: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    /// The input it accepts: `["text", "image"]` for a hosted model whose
    /// listing has `capabilities.vision: true`, `["text"]` when false; the
    /// core's admitted modalities in local mode.
    pub input_modalities: Vec<String>,
}

impl LaunchModel {
    /// A `cloud` model id the listing doesn't have, offered by its id alone.
    fn unlisted(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            details: None,
            product: None,
            price: None,
        }
    }
}

/// Add a model's name, input modalities and limits to a pi, omp or OpenClaw
/// model entry. An unlisted model ([`LaunchModel::unlisted`]) gets none.
fn add_details(entry: &mut Value, model: &LaunchModel) {
    if let Some(details) = &model.details {
        entry["name"] = serde_json::json!(details.display_name);
        entry["input"] = serde_json::json!(details.input_modalities);
        entry["contextWindow"] = serde_json::json!(details.context_window);
        entry["maxTokens"] = serde_json::json!(details.max_output_tokens);
    }
}

/// The models a rendered config selects and offers.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LaunchModels {
    /// The main agent's default.
    pub main: String,
    /// The default for a harness's small/background or subagent model role.
    pub background: String,
    /// Every model the provider entry lists, default first.
    pub offered: Vec<LaunchModel>,
}

impl LaunchModels {
    /// A cloud-mode launch: every model the router's `GET /v1/models` lists
    /// for the key ([`Self::listed_models`]), the main agent on `cloud.model.id`
    /// and the background role on `cloud.smol_model.id`, as written.
    #[cfg(test)]
    pub(crate) fn from_listing(body: &Value, cloud: &Cloud) -> Result<Self, String> {
        Ok(Self::for_cloud(Self::listed_models(body)?, cloud))
    }

    /// `offered` with the models `cloud` names selected, the main one first.
    /// Rayline doesn't check those ids: one the listing doesn't have is added
    /// by its id alone ([`LaunchModel::unlisted`]).
    fn for_cloud(mut offered: Vec<LaunchModel>, cloud: &Cloud) -> Self {
        let main = cloud.model.id.clone();
        let background = cloud.smol_model.id.clone();
        for id in [&main, &background] {
            if !offered.iter().any(|model| &model.id == id) {
                offered.push(LaunchModel::unlisted(id));
            }
        }
        offered.sort_by_key(|model| model.id != main);
        Self {
            main,
            background,
            offered,
        }
    }

    /// Every model the router's `GET /v1/models` lists for the key, with its
    /// limits.
    fn listed_models(body: &Value) -> Result<Vec<LaunchModel>, String> {
        let data = body
            .get("data")
            .and_then(Value::as_array)
            .ok_or("the response has no `data` list")?;
        data.iter()
            .map(|entry| {
                let id = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("a listed model has no id")?;
                let limit = |field: &str| {
                    entry
                        .get(field)
                        .and_then(Value::as_u64)
                        .ok_or_else(|| format!("{id} has no {field}"))
                };
                let vision = entry
                    .get("capabilities")
                    .and_then(|capabilities| capabilities.get("vision"))
                    .and_then(Value::as_bool)
                    .ok_or_else(|| format!("{id} has no capabilities.vision"))?;
                Ok(LaunchModel {
                    id: id.to_owned(),
                    details: Some(ModelDetails {
                        display_name: entry
                            .get("display_name")
                            .and_then(Value::as_str)
                            .ok_or_else(|| format!("{id} has no display_name"))?
                            .to_owned(),
                        context_window: limit("context_length")?,
                        max_output_tokens: limit("max_output_tokens")?,
                        input_modalities: if vision {
                            vec!["text".to_owned(), "image".to_owned()]
                        } else {
                            vec!["text".to_owned()]
                        },
                    }),
                    product: entry
                        .get("product")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    price: None,
                })
            })
            .collect()
    }

    /// A local-mode `rayline run <harness>`: the execution core serves one virtual model, at
    /// the limits the hybrid runtime admits.
    pub(crate) fn hybrid(capabilities: &rayline_local_router::ModelCapabilities) -> Self {
        Self {
            main: ROUTER_MODEL.to_owned(),
            background: ROUTER_MODEL.to_owned(),
            offered: vec![LaunchModel {
                id: ROUTER_MODEL.to_owned(),
                details: Some(ModelDetails {
                    display_name: "Rayline router".to_owned(),
                    context_window: capabilities.context_window,
                    max_output_tokens: capabilities.max_output_tokens,
                    input_modalities: capabilities.input_modalities.clone(),
                }),
                product: None,
                price: None,
            }],
        }
    }

    /// Read a `GET /v1/arc/prices` answer: each offered model's price by its
    /// own id, since every data mode has its own row. The card does not price
    /// every model, so a model without a row gets no price; a row missing a
    /// field is an error naming the model and the field.
    pub(crate) fn apply_arc_prices(&mut self, prices: &Value) -> Result<(), String> {
        let rows = prices
            .get("card")
            .and_then(|card| card.get("prices"))
            .and_then(Value::as_object)
            .ok_or("the response has no `card.prices` object")?;
        for model in &mut self.offered {
            let Some(row) = rows.get(&model.id) else {
                model.price = None;
                continue;
            };
            let field = |name: &str| {
                row.get(name)
                    .and_then(Value::as_f64)
                    .ok_or_else(|| format!("the price row for {} has no {name}", model.id))
            };
            model.price = Some(ModelPrice {
                input: field("input")?,
                output: field("output")?,
                cache_read: field("cache_read")?,
                cache_write: field("cache_write")?,
            });
        }
        Ok(())
    }

    /// The main agent's model.
    pub(crate) fn main_model(&self) -> &LaunchModel {
        &self.offered[0]
    }
}

/// `GET /v1/models` with the launch's key: the models a cloud-mode launch
/// offers, with `cloud`'s selected ([`LaunchModels::for_cloud`]). Fails the
/// launch when the router cannot say.
pub(crate) async fn fetch_hosted_models(
    router_url: &str,
    key: &str,
    cloud: &Cloud,
) -> Result<LaunchModels, String> {
    let url = format!("{router_url}/v1/models");
    let offered = get_json(&url, Some(key))
        .await
        .and_then(|body| LaunchModels::listed_models(&body))
        .map_err(|error| format!("couldn't read this key's models from {url}: {error}"))?;
    Ok(LaunchModels::for_cloud(offered, cloud))
}

/// The router's public ARC price card, `GET /v1/arc/prices`, applied to
/// `models` ([`LaunchModels::apply_arc_prices`]). Fails the launch when the
/// card cannot be read, as [`fetch_hosted_models`] does for the listing.
pub(crate) async fn apply_hosted_prices(
    router_url: &str,
    models: &mut LaunchModels,
) -> Result<(), String> {
    let url = format!("{router_url}/v1/arc/prices");
    get_json(&url, None)
        .await
        .and_then(|prices| models.apply_arc_prices(&prices))
        .map_err(|error| format!("couldn't read the ARC prices from {url}: {error}"))
}

async fn get_json(url: &str, bearer: Option<&str>) -> Result<Value, String> {
    let mut request = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|error| error.to_string())?
        .get(url);
    if let Some(bearer) = bearer {
        request = request.bearer_auth(bearer);
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("HTTP {status}: {error}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}"));
    }
    Ok(body)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Harness {
    OpenCode,
    Pi,
    Omp,
    Hermes,
    OpenClaw,
}

impl Harness {
    pub const ALL: [Harness; 5] = [
        Harness::OpenCode,
        Harness::Pi,
        Harness::Omp,
        Harness::Hermes,
        Harness::OpenClaw,
    ];

    pub fn from_command(command: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|harness| harness.name() == command)
    }

    /// The `rayline run <name>` harness name and the config directory name.
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
            Self::Omp => "omp",
            Self::Hermes => "hermes",
            Self::OpenClaw => "openclaw",
        }
    }

    /// The executable looked up on PATH.
    pub fn binary(self) -> &'static str {
        self.name()
    }

    pub(crate) fn install_hint(self) -> &'static str {
        match self {
            Self::OpenCode => "Install opencode: https://opencode.ai/docs/",
            Self::Pi => "Install pi: npm install -g @earendil-works/pi-coding-agent",
            Self::Omp => "Install oh-my-pi (omp): https://github.com/can1357/oh-my-pi",
            Self::Hermes => "Install Hermes Agent: https://github.com/NousResearch/hermes-agent",
            Self::OpenClaw => "Install OpenClaw: npm install -g openclaw",
        }
    }

    pub(crate) fn plan(self, ctx: &RenderContext<'_>) -> Result<Plan, String> {
        Ok(match self {
            Self::OpenCode => opencode::plan(ctx),
            Self::Pi => pi::plan(ctx)?,
            Self::Omp => omp::plan(ctx)?,
            Self::Hermes => hermes::plan(ctx)?,
            Self::OpenClaw => openclaw::plan(ctx),
        })
    }

    /// The installed harness's major version, where a renderer or the launch
    /// args depend on it: `Some` for OpenCode ([`opencode::major_version`]),
    /// run as `probe`, the harness with the launch's environment; `None` for
    /// every other harness.
    pub(crate) fn major_version(self, probe: Command) -> Result<Option<u64>, String> {
        match self {
            Self::OpenCode => opencode::major_version(probe).map(Some),
            _ => Ok(None),
        }
    }

    /// Refuse an installed harness too old for the config Rayline writes:
    /// OpenClaw older than [`openclaw::MIN_VERSION`]. `probe` is the harness
    /// with the launch's environment.
    pub(crate) fn require_version(self, probe: Command) -> Result<(), String> {
        match self {
            Self::OpenClaw => openclaw::require_version(probe),
            _ => Ok(()),
        }
    }

    /// Refuse `args` local mode can't run: OpenClaw's container mode.
    pub(crate) fn require_hybrid_args(self, args: &[OsString]) -> io::Result<()> {
        let env_container =
            std::env::var_os("OPENCLAW_CONTAINER").is_some_and(|value| !value.is_empty());
        if self == Self::OpenClaw && (env_container || openclaw::uses_container(args)) {
            return Err(io::Error::other(
                "OpenClaw container mode cannot use the hybrid gateway configuration; run an agent locally",
            ));
        }
        Ok(())
    }

    /// The launch args for `args`, which [`Self::require_hybrid_args`] accepted.
    pub(crate) fn hybrid_args(
        self,
        args: &[OsString],
        major_version: Option<u64>,
    ) -> Vec<OsString> {
        match (self, major_version) {
            (Self::OpenClaw, _) => openclaw::launch_args(args),
            (Self::OpenCode, Some(major_version)) => opencode_args(args, major_version),
            _ => args.to_vec(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    pub harness: Harness,
    /// The hosted environment the loaded config selects.
    pub hosted: crate::status::HostedEnvironment,
    /// `cloud`: the models the launch gives the harness.
    pub cloud: crate::config::Cloud,
    /// Args passed through to the harness unchanged.
    pub args: Vec<OsString>,
}

/// What a harness renderer needs.
pub(crate) struct RenderContext<'a> {
    /// The router key, written into the Rayline-owned config.
    pub key: &'a str,
    /// Router origin with no trailing `/` and no `/v1`
    /// (e.g. `https://router.example.test`).
    pub router_url: &'a str,
    pub config_dir: &'a Path,
    /// The limits a local-mode `rayline run <harness>` admits (context window the harness may
    /// budget against, the core's output cap, input modalities). `None`: the
    /// hosted router's defaults.
    pub capabilities: Option<&'a rayline_local_router::ModelCapabilities>,
    /// The installed harness's major version ([`Harness::major_version`]).
    pub harness_major_version: Option<u64>,
    /// The models this launch selects and offers: the router's listing for a
    /// hosted launch, [`LaunchModels::hybrid`] under a local-mode `rayline run <harness>`.
    pub models: LaunchModels,
}

impl RenderContext<'_> {
    /// Base URL for clients that append `/v1/messages` themselves.
    pub fn messages_base(&self) -> String {
        self.router_url.to_owned()
    }

    /// Base URL for clients that expect the `/v1` prefix in the base.
    pub fn v1_base(&self) -> String {
        format!("{}/v1", self.router_url)
    }
}

#[derive(Debug)]
pub(crate) struct ConfigFile {
    pub name: &'static str,
    /// Rayline rewrites it on every launch (it holds the env's URL, so an env
    /// change must take effect), owner-only (it holds the key). A harness whose
    /// file also holds user settings merges them into `contents` first
    /// (Hermes, pi, omp; see [`merged_file`]).
    pub contents: String,
    /// The file's text that `contents` was merged from. If the file no longer
    /// reads exactly this when it is replaced (the harness saved a setting
    /// meanwhile), the write is refused rather than losing that edit.
    pub merged_from: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct Plan {
    pub files: Vec<ConfigFile>,
    pub env_set: Vec<(&'static str, OsString)>,
    pub env_remove: Vec<&'static str>,
}

pub async fn run(request: RunRequest) -> ExitCode {
    let harness = request.harness;
    let mut command = match prepare(request).await {
        Ok(command) => command,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };
    exec_or_status(&mut command, harness)
}

async fn prepare(request: RunRequest) -> Result<Command, String> {
    let home = crate::home().map_err(|error| error.to_string())?;
    let harness = request.harness;
    let binary = find_on_path(harness.binary()).ok_or_else(|| {
        format!(
            "`{}` not found on PATH. {}",
            harness.binary(),
            harness.install_hint()
        )
    })?;

    // Same env -> router URL resolution as `rayline run claude`: built-in prod, or
    // settings.json `environments.<env>.base_url` for `--env`.
    let hosted = &request.hosted;
    let env_name = hosted.name().to_owned();
    let router_url = hosted.base_url.clone();
    // The stored `rlk-` key, signing in first when signed out, like `rayline run claude`.
    let key = crate::status::router_key(hosted, &home)
        .await
        .map_err(|error| error.to_string())?;

    if harness == Harness::Omp {
        omp::warn_if_old(&binary);
    }

    let mut models = fetch_hosted_models(&router_url, &key, &request.cloud).await?;
    // omp shows each model's cost, from the price card.
    if harness == Harness::Omp {
        apply_hosted_prices(&router_url, &mut models).await?;
    }
    harness.require_version(cloud_probe(&binary))?;
    let config_dir = config_dir(&home, harness);
    let ctx = RenderContext {
        key: &key,
        router_url: &router_url,
        config_dir: &config_dir,
        capabilities: None,
        harness_major_version: harness.major_version(cloud_probe(&binary))?,
        models,
    };
    let plan = harness.plan(&ctx)?;
    write_files(&config_dir, &plan.files).map_err(|error| {
        format!(
            "failed to write {} config in {}: {error}",
            harness.name(),
            config_dir.display()
        )
    })?;

    eprintln!(
        "{}: {} -> {router_url} (env {env_name})\n  config: {}",
        crate::DISPLAY_NAME,
        harness.name(),
        config_dir.display()
    );
    let args = match (harness, ctx.harness_major_version) {
        (Harness::OpenClaw, _) => {
            let env_container =
                std::env::var_os("OPENCLAW_CONTAINER").is_some_and(|value| !value.is_empty());
            if env_container || openclaw::uses_container(&request.args) {
                eprintln!(
                    "  warning: OpenClaw container mode runs inside the container, which does \
                     not see the Rayline config or key"
                );
            }
            let args = if env_container {
                request.args.clone()
            } else {
                openclaw::launch_args(&request.args)
            };
            if args.len() != request.args.len() {
                eprintln!(
                    "  added --local so this agent turn uses the Rayline config; with a \
                     Gateway running, stop it (openclaw gateway stop) or use `openclaw agent exec`"
                );
            }
            args
        }
        (Harness::OpenCode, Some(major_version)) => opencode_args(&request.args, major_version),
        _ => request.args,
    };
    let command = build_command(&binary, &plan, &args);
    if harness == Harness::Hermes {
        hermes::prepare_cloud_launch(&command)?;
    }
    Ok(command)
}

/// OpenCode's args with `--standalone` where OpenCode 2 would attach to its
/// background service ([`opencode::launch_args`]).
fn opencode_args(args: &[OsString], major_version: u64) -> Vec<OsString> {
    let launched = opencode::launch_args(args, major_version);
    if launched.len() != args.len() {
        eprintln!(
            "  added --standalone so OpenCode uses this config (its background service does not see it)"
        );
    }
    launched
}

/// `~/.rayline/harness/<name>/`, cloud mode's harness folder.
pub(crate) fn config_dir(home: &Path, harness: Harness) -> PathBuf {
    home.join(crate::DOT_CONFIG_DIR)
        .join("harness")
        .join(harness.name())
}

pub(crate) fn write_files(dir: &Path, files: &[ConfigFile]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    rayline_local_router::windows_state::restrict_to_owner(dir, true)?;
    for file in files {
        let path = dir.join(file.name);
        let tmp = dir.join(format!(".{}.tmp-{}", file.name, std::process::id()));
        write_private(&tmp, &file.contents)?;
        if let Some(base) = &file.merged_from
            && fs::read_to_string(&path).ok().as_ref() != Some(base)
        {
            let _ = fs::remove_file(&tmp);
            return Err(io::Error::other(format!(
                "{} changed while Rayline was updating it; run the command again",
                path.display()
            )));
        }
        fs::rename(&tmp, &path)?;
    }
    Ok(())
}

/// Write `contents` to a new file at `path` that only the owner can read (mode
/// 0600 on Unix, an owner-only DACL on Windows, set before anything is
/// written): the Rayline-owned configs hold the router key. A stale file is
/// removed first, since an open of an existing file keeps its mode.
/// A harness home the user exported (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`):
/// `None` when unset, the path as given otherwise. An empty value is an error
/// naming the variable.
pub(crate) fn exported_dir(var: &str) -> io::Result<Option<PathBuf>> {
    match std::env::var_os(var) {
        None => Ok(None),
        Some(value) if value.is_empty() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{var} is set but empty: unset it or set it to a directory"),
        )),
        Some(value) => Ok(Some(PathBuf::from(value))),
    }
}

pub(crate) fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    use std::io::Write as _;
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(windows)]
    rayline_local_router::windows_state::restrict_to_owner(path, false)?;
    file.write_all(contents.as_bytes())
}

/// `binary` without the router key, as [`build_command`] launches it, for a
/// probe that runs before the plan exists.
fn cloud_probe(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    command.env_remove(ROUTER_KEY_ENV);
    command
}

/// The version `command --version` prints on stdout. A harness that cannot
/// run, exits non-zero or prints no version is an error naming the probe.
pub(crate) fn installed_version(mut command: Command) -> Result<(u64, u64, u64), String> {
    let shown = format!("`{} --version`", Path::new(command.get_program()).display());
    let output = command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("failed to run {shown}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{shown} failed: {}", output.status));
    }
    let printed = String::from_utf8_lossy(&output.stdout);
    omp::parse_version(&printed)
        .ok_or_else(|| format!("{shown} printed no version: {:?}", printed.trim()))
}

pub(crate) fn build_command(binary: &Path, plan: &Plan, args: &[OsString]) -> Command {
    let mut command = Command::new(binary);
    command.args(args);
    // The key reaches the harness through its config only (see the module docs).
    command.env_remove(ROUTER_KEY_ENV);
    for name in &plan.env_remove {
        command.env_remove(name);
    }
    for (name, value) in &plan.env_set {
        command.env(name, value);
    }
    command
}

pub(crate) fn json_file(name: &'static str, value: &Value) -> ConfigFile {
    let mut contents = serde_json::to_string_pretty(value).expect("config serializes");
    contents.push('\n');
    ConfigFile {
        name,
        contents,
        merged_from: None,
    }
}

/// `name` in `dir`, a file the harness also writes its own settings to:
/// `fresh` when it doesn't exist, else what `merge` makes of its text, which
/// sets Rayline's keys and keeps everything else. A file `merge` refuses is an error that names it.
pub(crate) fn merged_file(
    dir: &Path,
    name: &'static str,
    fresh: String,
    merge: impl FnOnce(&str) -> Result<String, String>,
) -> Result<ConfigFile, String> {
    let path = dir.join(name);
    let (contents, merged_from) = match fs::read_to_string(&path) {
        Ok(existing) => {
            let merged = merge(&existing).map_err(|error| {
                format!(
                    "cannot update {} ({error}); fix or remove the file",
                    path.display()
                )
            })?;
            (merged, Some(existing))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (fresh, None),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    Ok(ConfigFile {
        name,
        contents,
        merged_from,
    })
}

pub(crate) fn yaml_file(name: &'static str, value: &Value) -> ConfigFile {
    ConfigFile {
        name,
        contents: to_yaml(value),
        merged_from: None,
    }
}

/// Minimal YAML writer for the configs above: block mappings and sequences,
/// scalars as JSON literals (a JSON string is a valid YAML double-quoted
/// scalar), and scalar-only sequences in flow style.
pub(crate) fn to_yaml(value: &Value) -> String {
    let mut out = String::new();
    write_yaml(value, 0, &mut out);
    out
}

fn write_yaml(value: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                out.push_str(&pad);
                out.push_str(&yaml_key(key));
                out.push(':');
                write_yaml_child(child, indent, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                out.push_str(&pad);
                out.push('-');
                match item {
                    Value::Object(map) if !map.is_empty() => {
                        // `- key: value` with the remaining keys aligned under it.
                        let mut nested = String::new();
                        write_yaml(item, indent + 2, &mut nested);
                        out.push(' ');
                        out.push_str(nested.trim_start());
                    }
                    _ => write_yaml_child(item, indent, out),
                }
            }
        }
        scalar => {
            out.push_str(&pad);
            out.push_str(&scalar.to_string());
            out.push('\n');
        }
    }
}

fn write_yaml_child(child: &Value, indent: usize, out: &mut String) {
    match child {
        Value::Object(map) if map.is_empty() => out.push_str(" {}\n"),
        Value::Object(_) => {
            out.push('\n');
            write_yaml(child, indent + 2, out);
        }
        Value::Array(items)
            if items
                .iter()
                .all(|item| !item.is_object() && !item.is_array()) =>
        {
            let rendered: Vec<String> = items.iter().map(Value::to_string).collect();
            out.push_str(&format!(" [{}]\n", rendered.join(", ")));
        }
        Value::Array(_) => {
            out.push('\n');
            write_yaml(child, indent + 2, out);
        }
        scalar => {
            out.push(' ');
            out.push_str(&scalar.to_string());
            out.push('\n');
        }
    }
}

fn yaml_key(key: &str) -> String {
    let plain = !key.is_empty()
        && key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'));
    if plain {
        key.to_owned()
    } else {
        Value::String(key.to_owned()).to_string()
    }
}

pub(crate) fn find_on_path(binary_name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names = executable_names(binary_name, cfg!(windows));
    std::env::split_paths(&path).find_map(|dir| find_in_dir(&dir, &names))
}

/// The first of `names` in `dir` that is a runnable file. On Unix that means
/// an execute bit is set, as a shell's lookup requires.
pub(crate) fn find_in_dir(dir: &Path, names: &[String]) -> Option<PathBuf> {
    names
        .iter()
        .map(|name| dir.join(name))
        .find(|candidate| is_runnable(candidate))
}

#[cfg(unix)]
fn is_runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_runnable(path: &Path) -> bool {
    path.is_file()
}

/// File names to probe in each PATH dir, in order. On Windows only names with
/// a runnable extension count: npm puts an extensionless sh shim next to the
/// `.cmd` one, and CreateProcess cannot run the shim.
pub(crate) fn executable_names(binary_name: &str, windows: bool) -> Vec<String> {
    if windows {
        ["exe", "cmd", "bat", "com"]
            .iter()
            .map(|ext| format!("{binary_name}.{ext}"))
            .collect()
    } else {
        vec![binary_name.to_owned()]
    }
}

/// The program to launch for harness `name`, given the `PATH` to search.
/// Windows process creation does not apply PATHEXT, so npm's `.cmd` launchers
/// (codex, pi) are resolved the way a shell would; Unix keeps the bare name
/// and its usual PATH lookup.
pub(crate) fn launch_program(name: &str, path: Option<std::ffi::OsString>) -> PathBuf {
    if cfg!(windows) {
        let names = executable_names(name, true);
        if let Some(found) = path
            .and_then(|path| std::env::split_paths(&path).find_map(|dir| find_in_dir(&dir, &names)))
        {
            return found;
        }
    }
    PathBuf::from(name)
}

#[cfg(unix)]
fn exec_or_status(command: &mut Command, harness: Harness) -> ExitCode {
    use std::os::unix::process::CommandExt as _;

    let error = command.exec();
    eprintln!("rayline: failed to exec {}: {error}", harness.binary());
    ExitCode::from(127)
}

#[cfg(not(unix))]
fn exec_or_status(command: &mut Command, harness: Harness) -> ExitCode {
    match command.status() {
        Ok(status) => crate::child_exit_code(status),
        Err(error) => {
            eprintln!("rayline: failed to run {}: {error}", harness.binary());
            ExitCode::from(127)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) const TEST_URL: &str = "https://router.example.test";
    pub(crate) const TEST_KEY: &str = "rlk-test";

    pub(crate) fn temp_dir(label: &str) -> PathBuf {
        // A counter, not the clock: parallel tests can read the same time.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rayline-harness-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Asserts that Rayline wrote files under `home` and that each one is
    /// under `~/.rayline/harness/<name>/`, so none is under `~/.config/rayline`.
    /// `inputs` are paths under `home` the test or the harness itself owns.
    pub(crate) fn assert_only_harness_files(home: &Path, name: &str, inputs: &[&str]) {
        fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if fs::symlink_metadata(&path).unwrap().is_dir() {
                    walk(&path, files);
                } else {
                    files.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(home, &mut files);
        files.retain(|file| {
            !inputs
                .iter()
                .any(|input| file.starts_with(home.join(input)))
        });
        assert!(!files.is_empty(), "{name} wrote nothing");
        let root = home.join(".rayline/harness").join(name);
        for file in files {
            assert!(
                file.starts_with(&root),
                "{name} wrote {} outside {}",
                file.display(),
                root.display()
            );
        }
    }

    /// A `GET /v1/models` answer for a key in `mode` (`None`: standard),
    /// listing Swift before Sage as the router does.
    pub(crate) fn listing(mode: Option<&str>) -> Value {
        let entry = |product: &str, name: &str| {
            let id = mode.map_or_else(
                || product.to_owned(),
                |mode| product.replace("-1.0", &format!("-{mode}-1.0")),
            );
            json!({
                "id": id, "object": "model", "owned_by": "rayline",
                "display_name": name, "product": product,
                "context_length": 1_000_000, "max_output_tokens": 128_000,
                "capabilities": {"vision": true, "tools": true, "reasoning": true}
            })
        };
        json!({
            "object": "list",
            "data": [
                entry("arc-swift-1.0", "ARC Swift"),
                entry("arc-sage-1.0", "ARC Sage")
            ]
        })
    }

    /// A `cloud` section naming the Sage and Swift ids [`listing`] has for
    /// `mode`.
    pub(crate) fn cloud(mode: Option<&str>) -> Cloud {
        let id = |product: &str| crate::config::CloudModel {
            id: mode.map_or_else(
                || product.to_owned(),
                |mode| product.replace("-1.0", &format!("-{mode}-1.0")),
            ),
        };
        Cloud {
            model: id("arc-sage-1.0"),
            smol_model: id("arc-swift-1.0"),
        }
    }

    /// The models a standard key's cloud-mode launch offers.
    pub(crate) fn hosted_models() -> LaunchModels {
        LaunchModels::from_listing(&listing(None), &cloud(None)).unwrap()
    }

    #[test]
    fn a_launch_runs_the_cloud_ids_as_written_with_their_listed_limits() {
        let mut body = listing(Some("contributor"));
        body["data"][0]["context_length"] = json!(200_000);
        body["data"].as_array_mut().unwrap().push(json!({
            "id": "other/model", "object": "model", "owned_by": "other",
            "display_name": "Other", "context_length": 32_000, "max_output_tokens": 4_096,
            "capabilities": {"vision": false, "tools": true, "reasoning": true}
        }));
        let models = LaunchModels::from_listing(&body, &cloud(Some("contributor"))).unwrap();
        assert_eq!(models.main, "arc-sage-contributor-1.0");
        assert_eq!(models.background, "arc-swift-contributor-1.0");
        let ids: Vec<_> = models
            .offered
            .iter()
            .map(|model| model.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "arc-sage-contributor-1.0",
                "arc-swift-contributor-1.0",
                "other/model"
            ]
        );
        let details = |index: usize| models.offered[index].details.as_ref().unwrap();
        assert_eq!(details(1).context_window, 200_000);
        assert_eq!(details(2).display_name, "Other");
        assert_eq!(models.offered[2].product, None);
        assert_eq!(details(0).input_modalities, ["text", "image"]);
        assert_eq!(details(2).input_modalities, ["text"]);
    }

    #[test]
    fn the_main_and_background_models_are_any_listed_ids() {
        let mut body = listing(None);
        body["data"].as_array_mut().unwrap().push(json!({
            "id": "other/model", "object": "model", "owned_by": "other",
            "display_name": "Other", "context_length": 32_000, "max_output_tokens": 4_096,
            "capabilities": {"vision": false, "tools": true, "reasoning": true}
        }));
        let other = crate::config::CloudModel {
            id: "other/model".to_owned(),
        };
        let swift = crate::config::CloudModel {
            id: "arc-swift-1.0".to_owned(),
        };
        let models = LaunchModels::from_listing(
            &body,
            &Cloud {
                model: other.clone(),
                smol_model: other,
            },
        )
        .unwrap();
        assert_eq!(models.main, "other/model");
        assert_eq!(models.background, "other/model");
        assert_eq!(models.offered[0].id, "other/model");
        let models = LaunchModels::from_listing(
            &body,
            &Cloud {
                model: swift.clone(),
                smol_model: swift,
            },
        )
        .unwrap();
        assert_eq!(models.main, "arc-swift-1.0");
        assert_eq!(models.offered.len(), 3);
    }

    /// Rayline doesn't check a `cloud` id against the listing: one it doesn't
    /// have runs as written, offered by its id alone.
    #[test]
    fn a_cloud_id_the_listing_does_not_have_is_offered_by_its_id_alone() {
        let mut unlisted = cloud(None);
        unlisted.model.id = "acme/unlisted-1".to_owned();
        let models = LaunchModels::from_listing(&listing(None), &unlisted).unwrap();
        assert_eq!(models.main, "acme/unlisted-1");
        assert_eq!(models.background, "arc-swift-1.0");
        assert_eq!(models.offered.len(), 3);
        assert_eq!(
            models.main_model(),
            &LaunchModel::unlisted("acme/unlisted-1")
        );
        assert!(
            models.offered[1..]
                .iter()
                .all(|model| model.details.is_some())
        );

        let mut unlisted = cloud(None);
        unlisted.smol_model.id = "acme/unlisted-small".to_owned();
        let models = LaunchModels::from_listing(&listing(None), &unlisted).unwrap();
        assert_eq!(models.main, "arc-sage-1.0");
        assert_eq!(models.background, "acme/unlisted-small");
        assert_eq!(
            models.offered.last().unwrap(),
            &LaunchModel::unlisted("acme/unlisted-small")
        );
    }

    /// Each harness config lists an unlisted id with no name and no limits,
    /// so the harness uses its own defaults for it.
    #[test]
    fn every_harness_config_lists_an_unlisted_id_without_limits() {
        let unlisted = Cloud {
            model: crate::config::CloudModel {
                id: "acme/unlisted-1".to_owned(),
            },
            smol_model: crate::config::CloudModel {
                id: "acme/unlisted-small".to_owned(),
            },
        };
        let models = LaunchModels::from_listing(&json!({ "data": [] }), &unlisted).unwrap();
        for harness in Harness::ALL {
            let home = temp_dir(harness.name());
            let dir = config_dir(&home, harness);
            let ctx = RenderContext {
                key: TEST_KEY,
                router_url: TEST_URL,
                config_dir: &dir,
                capabilities: None,
                harness_major_version: None,
                models: models.clone(),
            };
            let plan = harness.plan(&ctx).unwrap();
            let contents: String = plan
                .files
                .iter()
                .map(|file| file.contents.as_str())
                .collect();
            assert!(contents.contains("acme/unlisted-1"), "{}", harness.name());
            assert!(
                contents.contains("acme/unlisted-small"),
                "{}",
                harness.name()
            );
            for limit in [
                "context",
                "maxTokens",
                "\"limit\"",
                "modalities",
                "\"input\"",
            ] {
                assert!(
                    !contents.contains(limit),
                    "{} writes {limit}: {contents}",
                    harness.name()
                );
            }
            fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn a_listing_without_limits_is_an_error() {
        let mut body = listing(None);
        body["data"][1]
            .as_object_mut()
            .unwrap()
            .remove("max_output_tokens");
        assert_eq!(
            LaunchModels::from_listing(&body, &cloud(None)).unwrap_err(),
            "arc-sage-1.0 has no max_output_tokens"
        );
        let mut body = listing(None);
        body["data"][0]
            .as_object_mut()
            .unwrap()
            .remove("display_name");
        assert_eq!(
            LaunchModels::from_listing(&body, &cloud(None)).unwrap_err(),
            "arc-swift-1.0 has no display_name"
        );
        let mut body = listing(None);
        body["data"][1]["capabilities"]
            .as_object_mut()
            .unwrap()
            .remove("vision");
        assert_eq!(
            LaunchModels::from_listing(&body, &cloud(None)).unwrap_err(),
            "arc-sage-1.0 has no capabilities.vision"
        );
    }

    #[test]
    fn prices_come_from_each_ids_card_row() {
        let mut models =
            LaunchModels::from_listing(&listing(Some("contributor")), &cloud(Some("contributor")))
                .unwrap();
        models
            .apply_arc_prices(&json!({"card": {"prices": {
                "arc-sage-contributor-1.0": {
                    "input": 0.5, "output": 2.5, "cache_read": 0.025, "cache_write": 0.625
                },
                "arc-sage-1.0": {"input": 9.0, "output": 9.0, "cache_read": 9.0, "cache_write": 9.0}
            }}}))
            .unwrap();
        assert_eq!(
            models.main_model().price,
            Some(ModelPrice {
                input: 0.5,
                output: 2.5,
                cache_read: 0.025,
                cache_write: 0.625
            })
        );
        // No row for this id: no price, never another id's.
        assert_eq!(models.offered[1].price, None);
    }

    #[test]
    fn an_incomplete_price_row_or_card_is_an_error() {
        let mut models = hosted_models();
        assert_eq!(
            models
                .apply_arc_prices(&json!({"card": {"prices": {
                    "arc-sage-1.0": {"input": 0.5, "output": 2.5, "cache_read": 0.025}
                }}}))
                .unwrap_err(),
            "the price row for arc-sage-1.0 has no cache_write"
        );
        assert_eq!(
            models.apply_arc_prices(&json!({"card": {}})).unwrap_err(),
            "the response has no `card.prices` object"
        );
    }

    /// A hosted dev context writing into `dir`.
    pub(crate) fn render_context(dir: &Path) -> RenderContext<'_> {
        RenderContext {
            key: TEST_KEY,
            router_url: TEST_URL,
            config_dir: dir,
            capabilities: None,
            harness_major_version: None,
            models: hosted_models(),
        }
    }

    /// Render `harness` into a temp dir and return (dir, plan).
    pub(crate) fn render(harness: Harness) -> (PathBuf, Plan) {
        let home = temp_dir(harness.name());
        let dir = config_dir(&home, harness);
        let ctx = RenderContext {
            key: TEST_KEY,
            router_url: TEST_URL,
            config_dir: &dir,
            capabilities: None,
            harness_major_version: None,
            models: hosted_models(),
        };
        let plan = harness.plan(&ctx).unwrap();
        write_files(&dir, &plan.files).unwrap();
        (dir, plan)
    }

    pub(crate) fn env_value<'a>(plan: &'a Plan, name: &str) -> Option<&'a OsString> {
        plan.env_set
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    }

    #[test]
    fn config_dir_is_per_harness() {
        let home = Path::new("/home/u");
        assert_eq!(
            config_dir(home, Harness::Pi),
            PathBuf::from("/home/u/.rayline/harness/pi")
        );
    }

    #[test]
    fn every_harness_writes_only_under_its_harness_folder() {
        for harness in Harness::ALL {
            let (dir, _) = render(harness);
            let home = dir.ancestors().nth(3).unwrap();
            assert_only_harness_files(home, harness.name(), &[]);
            fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn an_empty_exported_dir_is_an_error_naming_the_var() {
        const VAR: &str = "RAYLINE_TEST_EXPORTED_DIR";
        // SAFETY: the variable is this test's own; no other code reads it.
        unsafe { std::env::remove_var(VAR) };
        assert_eq!(exported_dir(VAR).unwrap(), None);
        unsafe { std::env::set_var(VAR, "") };
        let error = exported_dir(VAR).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains(VAR), "{error}");
        unsafe { std::env::set_var(VAR, "~/x") };
        assert_eq!(exported_dir(VAR).unwrap(), Some(PathBuf::from("~/x")));
        unsafe { std::env::remove_var(VAR) };
    }

    #[test]
    fn harness_names_round_trip() {
        for harness in Harness::ALL {
            assert_eq!(Harness::from_command(harness.name()), Some(harness));
        }
        assert_eq!(Harness::from_command("claude"), None);
    }

    #[cfg(windows)]
    #[test]
    fn write_files_restricts_the_dir_and_files_to_the_owner() {
        use rayline_local_router::windows_state::is_owner_only;
        let dir = temp_dir("write-acl").join("cfg");
        let files = [ConfigFile {
            name: "config.toml",
            contents: "secret = 1\n".to_owned(),
            merged_from: None,
        }];
        write_files(&dir, &files).unwrap();
        assert!(is_owner_only(&dir).unwrap());
        assert!(is_owner_only(&dir.join("config.toml")).unwrap());
    }

    #[test]
    fn write_files_rewrites_each_file() {
        let dir = temp_dir("write").join("cfg");
        let files = |text: &str| {
            vec![ConfigFile {
                name: "owned.json",
                contents: text.to_owned(),
                merged_from: None,
            }]
        };
        write_files(&dir, &files("one")).unwrap();
        write_files(&dir, &files("two")).unwrap();
        assert_eq!(fs::read_to_string(dir.join("owned.json")).unwrap(), "two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[test]
    fn command_sets_the_plan_env_and_removes_keys() {
        let plan = Plan {
            files: Vec::new(),
            env_set: vec![("OPENCODE_CONFIG", OsString::from("/x/opencode.json"))],
            env_remove: vec!["CLAUDE_CONFIG_DIR"],
        };
        let command = build_command(
            Path::new("/bin/true"),
            &plan,
            &[OsString::from("run"), OsString::from("hi")],
        );
        let envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(envs.contains(&("RAYLINE_ROUTER_API_KEY".into(), None)));
        assert!(
            !envs
                .iter()
                .any(|(_, value)| value.as_deref() == Some(TEST_KEY))
        );
        assert!(envs.contains(&("OPENCODE_CONFIG".into(), Some("/x/opencode.json".into()))));
        assert!(envs.contains(&("CLAUDE_CONFIG_DIR".into(), None)));
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["run", "hi"]);
    }

    /// The key reaches each harness through an owner-only Rayline config file,
    /// never through an env var a tool subprocess would inherit.
    #[test]
    fn key_is_only_in_owner_only_config_files() {
        for harness in Harness::ALL {
            let (dir, plan) = render(harness);
            let mut holds_key = false;
            for file in &plan.files {
                let path = dir.join(file.name);
                let text = fs::read_to_string(&path).unwrap();
                holds_key |= text.contains(TEST_KEY);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = fs::metadata(&path).unwrap().permissions().mode();
                    assert_eq!(mode & 0o777, 0o600, "{}: {}", harness.name(), file.name);
                }
            }
            assert!(holds_key, "{} config must carry the key", harness.name());
            assert!(
                plan.env_set
                    .iter()
                    .all(|(_, value)| !value.to_string_lossy().contains(TEST_KEY)),
                "{}",
                harness.name()
            );
        }
    }

    /// #1 (0.2.11): an exported key, or one a parent launch set, must not reach
    /// the harness, whose tool subprocesses inherit its environment.
    #[cfg(unix)]
    #[test]
    fn harness_child_never_inherits_a_router_key() {
        let vars = [ROUTER_KEY_ENV];
        let previous: Vec<_> = vars.iter().map(std::env::var_os).collect();
        for var in vars {
            // SAFETY: tests run with --test-threads=1 (AGENTS.md), so no other
            // thread reads the environment concurrently.
            unsafe { std::env::set_var(var, "rlk-test-not-a-real-key") };
        }
        let output = build_command(Path::new("/usr/bin/env"), &Plan::default(), &[])
            .output()
            .expect("run env");
        for (var, value) in vars.iter().zip(previous) {
            // SAFETY: as above.
            match value {
                Some(value) => unsafe { std::env::set_var(var, value) },
                None => unsafe { std::env::remove_var(var) },
            }
        }
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success());
        assert!(
            !printed.contains("rlk-test-not-a-real-key"),
            "the harness environment still carries a router key"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_files_replaces_a_readable_file_with_an_owner_only_one() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir("mode").join("cfg");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("owned.json");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let files = [ConfigFile {
            name: "owned.json",
            contents: "new".to_owned(),
            merged_from: None,
        }];
        write_files(&dir, &files).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn windows_lookup_skips_the_extensionless_npm_shim() {
        // npm's global bin dir holds `pi` (a sh script), `pi.cmd` and
        // `pi.ps1`; only `pi.cmd` runs under Windows' CreateProcess.
        let dir = temp_dir("pathext");
        for name in ["pi", "pi.cmd", "pi.ps1"] {
            fs::write(dir.join(name), "").unwrap();
        }
        let found = executable_names("pi", true)
            .into_iter()
            .map(|name| dir.join(name))
            .find(|candidate| candidate.is_file());
        assert_eq!(found, Some(dir.join("pi.cmd")));
        assert_eq!(executable_names("pi", false), ["pi"]);
    }

    #[test]
    fn launch_program_resolves_windows_launchers_only_on_windows() {
        let dir = temp_dir("launch-program");
        // npm installs an extensionless sh shim beside the `.cmd` launcher.
        fs::write(dir.join("codex"), "#!/bin/sh\n").unwrap();
        fs::write(dir.join("codex.cmd"), "@echo off\r\n").unwrap();
        let path = Some(std::env::join_paths([&dir]).unwrap());
        let program = launch_program("codex", path.clone());
        if cfg!(windows) {
            assert_eq!(program, dir.join("codex.cmd"));
        } else {
            assert_eq!(program, PathBuf::from("codex"));
        }
        assert_eq!(launch_program("pi", path), PathBuf::from("pi"));
        assert_eq!(launch_program("codex", None), PathBuf::from("codex"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn path_lookup_skips_a_non_executable_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let stale = temp_dir("noexec");
        let good = temp_dir("exec");
        fs::write(stale.join("pi"), "").unwrap();
        fs::set_permissions(stale.join("pi"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(good.join("pi"), "").unwrap();
        fs::set_permissions(good.join("pi"), fs::Permissions::from_mode(0o755)).unwrap();
        let names = executable_names("pi", false);
        assert_eq!(find_in_dir(&stale, &names), None);
        assert_eq!(find_in_dir(&good, &names), Some(good.join("pi")));
    }

    #[test]
    fn yaml_writer_shapes() {
        let value = json!({
            "a": {"b": "x", "list": ["text", "image"], "empty": {}},
            "seq": [{"id": "m", "n": 1}],
            "x-y": true
        });
        assert_eq!(
            to_yaml(&value),
            "a:\n  b: \"x\"\n  empty: {}\n  list: [\"text\", \"image\"]\nseq:\n  - id: \"m\"\n    n: 1\nx-y: true\n"
        );
    }
}
