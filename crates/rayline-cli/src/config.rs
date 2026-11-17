//! `settings.json`: the typed config, its one load, env overrides and load
//! errors (docs/spec.md section 5).
//!
//! [`load`] reads `~/.config/rayline/settings.json`, or the file passed with
//! `--config`, applies the `RAYLINE_*` env overrides and validates the result
//! once. A [`Settings`] value is valid by construction: code that holds one
//! trusts it. Before it reads the default file, load writes it when it
//! doesn't exist and converts it when it is in the earlier shape
//! ([`prepare_default_file`]). [`check_local_start`] runs the checks the
//! local process makes before it serves anything.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use url::Url;

/// The file `rayline` writes when `~/.config/rayline/settings.json` doesn't
/// exist (spec section 5). Its `cloud` section is also the one the
/// conversion from the earlier shape adds.
pub(crate) const DEFAULT_FILE: &str = include_str!("default-settings.json");

/// A wire protocol an endpoint serves and a harness speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    AnthropicMessages,
    OpenaiChat,
    OpenaiResponses,
}

impl Protocol {
    pub const ALL: [Protocol; 3] = [
        Protocol::AnthropicMessages,
        Protocol::OpenaiChat,
        Protocol::OpenaiResponses,
    ];

    /// The name used in `settings.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::AnthropicMessages => "anthropic_messages",
            Protocol::OpenaiChat => "openai_chat",
            Protocol::OpenaiResponses => "openai_responses",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|protocol| protocol.as_str() == value)
    }
}

/// `mode`: where a `rayline run` sends the harness's requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Straight to Rayline Platform, with the models in `cloud`.
    Cloud,
    /// Through the local process, set up by `local`.
    Local,
}

impl Mode {
    pub const ALL: [Mode; 2] = [Mode::Cloud, Mode::Local];

    /// The name used in `settings.json`, `RAYLINE_MODE` and `--mode`.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Cloud => "cloud",
            Mode::Local => "local",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == value)
    }
}

/// A model object in `cloud`: `{"id": "<ARC model id>"}`. The id goes to
/// the API as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudModel {
    pub id: String,
}

/// `cloud`: the models a cloud-mode run gives the harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cloud {
    pub model: CloudModel,
    /// `cloud.smol_model`, else `cloud.model`.
    pub smol_model: CloudModel,
}

/// `local.routing.decisions_model`: `{"id": "<ARC model id>", "v_value":
/// <number>}`. Both go to the API as written.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionsModel {
    pub id: String,
    /// A number >= 0.
    pub v_value: Option<f64>,
}

/// `local.inference.endpoints.<id>`: one of the user's own inference
/// servers.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub base_url: Url,
    /// Non-empty, without repeats.
    pub protocols: Vec<Protocol>,
    /// Name of the env var holding the server's key.
    pub api_key_env: Option<String>,
}

/// `local.inference.models.<id>.price`, overriding ARC's price for that
/// model.
#[derive(Debug, Clone, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// `local.inference.models.<id>`: a model ARC can pick that one of
/// `local.inference.endpoints` serves.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// A key of [`Settings::endpoints`].
    pub endpoint: String,
    /// The name the endpoint knows the model by.
    pub model: String,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    /// Hugging Face tokenizer repo.
    pub tokenizer: Option<String>,
    pub price: Option<Price>,
}

/// `local.routing.policy`.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// ARC's preset id, sent to ARC unchanged.
    pub preset: String,
    /// ARC's `routing.policy.overrides`, sent to ARC unchanged and not
    /// validated here.
    pub overrides: Option<Map<String, Value>>,
}

/// `local.routing.router`: where the local process gets its routing
/// decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Router {
    /// The environment's Rayline router decides, in Rayline Platform.
    Cloud,
    /// ARC runs in the local process, from the release it downloads.
    Local,
}

impl Router {
    pub const ALL: [Router; 2] = [Router::Cloud, Router::Local];

    /// The name used in `settings.json`, `RAYLINE_LOCAL_ROUTING_ROUTER` and
    /// `--router`.
    pub fn as_str(self) -> &'static str {
        match self {
            Router::Cloud => "cloud",
            Router::Local => "local",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|router| router.as_str() == value)
    }
}

/// `local.routing.arc_release`: the Hugging Face repo, commit and manifest
/// digest of the ARC runtime release local decisions download instead of the
/// built-in one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArcRelease {
    /// `<owner>/<name>`.
    pub repo: String,
    /// A full commit id: 40 lowercase hex digits.
    pub revision: String,
    /// The sha256 of the release's `manifest.json`: 64 lowercase hex digits.
    pub manifest_sha256: String,
    /// The repo directory that holds the release; `None` is the repo root.
    pub dir: Option<String>,
}

/// `local.routing`.
#[derive(Debug, Clone, PartialEq)]
pub struct Routing {
    pub decisions_model: DecisionsModel,
    /// Required only when a Claude Code harness runs.
    pub classifier_model: Option<String>,
    pub policy: Policy,
    pub router: Router,
    pub arc_release: Option<ArcRelease>,
    /// The file has no `local.routing`: this is the `--router` defaults.
    pub defaults: bool,
}

impl Routing {
    /// The `settings.json` that runs Claude Code with this routing: it with
    /// `classifier_model`, which has no default.
    pub fn with_classifier_model(&self) -> String {
        let string = |value: &str| Value::from(value).to_string();
        let decisions_model = match self.decisions_model.v_value {
            Some(v_value) => format!(
                "{{\"id\": {}, \"v_value\": {}}}",
                string(&self.decisions_model.id),
                Value::from(v_value)
            ),
            None => format!("{{\"id\": {}}}", string(&self.decisions_model.id)),
        };
        [
            "{".to_owned(),
            "  \"mode\": \"local\",".to_owned(),
            "  \"local\": {".to_owned(),
            "    \"routing\": {".to_owned(),
            format!("      \"router\": {},", string(self.router.as_str())),
            format!("      \"decisions_model\": {decisions_model},"),
            format!(
                "      \"policy\": {{\"preset\": {}}},",
                string(&self.policy.preset)
            ),
            "      \"classifier_model\": \"<model id>\"".to_owned(),
            "    }".to_owned(),
            "  }".to_owned(),
            "}".to_owned(),
        ]
        .iter()
        .map(|line| format!("  {line}\n"))
        .collect()
    }
}

/// `environments.<name>`: an internal environment selected with `--env`.
#[derive(Debug, Clone, PartialEq)]
pub struct Environment {
    pub base_url: Url,
    pub login_url: Url,
}

/// `update.channel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateChannel {
    Prod,
    Dev,
    Main,
}

impl UpdateChannel {
    pub const ALL: [UpdateChannel; 3] =
        [UpdateChannel::Prod, UpdateChannel::Dev, UpdateChannel::Main];

    /// The name used in `settings.json` and `RAYLINE_UPDATE_CHANNEL`.
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateChannel::Prod => "prod",
            UpdateChannel::Dev => "dev",
            UpdateChannel::Main => "main",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|channel| channel.as_str() == value)
    }
}

/// `update`.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub channel: UpdateChannel,
}

/// The loaded and validated config.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The file the config was read from.
    pub file: PathBuf,
    /// `cloud`. Present in a cloud-mode run.
    pub cloud: Option<Cloud>,
    /// `local.inference.endpoints`.
    pub endpoints: BTreeMap<String, Endpoint>,
    /// `local.inference.models`.
    pub models: BTreeMap<String, Model>,
    /// `local.routing`. Present when the command runs in local mode.
    pub routing: Option<Routing>,
    pub environments: BTreeMap<String, Environment>,
    pub update: Update,
    /// The command runs in local mode: a `rayline run` in mode `local`, or
    /// `rayline local start`.
    pub local: bool,
    /// The `--env` name, always a key of `environments`. `None` is prod.
    pub env: Option<String>,
}

/// The command that loads the config: the file it reads and which of
/// `cloud` and `local` it uses (section 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command<'a> {
    /// `rayline run`: `--config <file>` (`None` is the default file), and
    /// `--mode`, over the file's `mode` and `RAYLINE_MODE`.
    Run {
        config: Option<&'a Path>,
        mode: Option<Mode>,
    },
    /// `rayline local start`: `--config <file>`. It uses `local` whatever
    /// `mode` says.
    LocalStart { config: Option<&'a Path> },
    /// `auth`, `top`, `update`, `local status` and `local stop`: the default
    /// file. They use neither `cloud` nor `local`.
    Other,
}

/// What [`load`] reads and what it must refuse.
#[derive(Debug, Clone, Copy)]
pub struct LoadOptions<'a> {
    pub command: Command<'a>,
    /// `--env <name>`. `None` is prod.
    pub env: Option<&'a str>,
    /// The protocol of the harness being run. `None` when no harness is.
    pub harness_protocol: Option<Protocol>,
    /// `rayline local start --router`: `local.routing.router`, over the
    /// file's and the env's.
    pub router: Option<Router>,
    /// A local-mode `rayline run` while a local process started from the
    /// `--router` defaults runs: that process's router, whose defaults a
    /// file without `local.routing` uses.
    pub routing_defaults: Option<Router>,
}

/// `rayline local start --router` with no `local.routing` in the file: the
/// routing it starts with.
pub const DEFAULT_DECISIONS_MODEL: &str = "arc-swift-1.0";
pub const DEFAULT_POLICY_PRESET: &str = "arc-policy.prefer-local.v1";
/// Where local mode is documented.
const LOCAL_MODE_DOCS: &str = "https://docs.rayline.ai/rayline-cli/local-mode";

/// A config that Rayline refuses to run with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The home directory, which holds the default file, is unknown.
    NoHome(crate::HomeError),
    /// The file could not be read.
    Read { file: PathBuf, error: String },
    /// The default file, or its backup, could not be written.
    Write { file: PathBuf, error: String },
    /// The file is not a JSON object.
    Parse { file: PathBuf, error: String },
    /// A key in the file is wrong.
    Key {
        file: PathBuf,
        key: String,
        problem: String,
    },
    /// An env override is wrong.
    Env {
        var: &'static str,
        key: &'static str,
        problem: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NoHome(error) => error.fmt(f),
            ConfigError::Read { file, error }
            | ConfigError::Write { file, error }
            | ConfigError::Parse { file, error } => write!(f, "{}: {error}", file.display()),
            ConfigError::Key { file, key, problem } => {
                write!(f, "{}: `{key}` {problem}", file.display())
            }
            ConfigError::Env { var, key, problem } => {
                write!(f, "`{var}` (overrides `{key}`) {problem}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// An env override of section 5.7: (env var, key).
type EnvVar = (&'static str, &'static str);

const MODE: EnvVar = ("RAYLINE_MODE", "mode");
const CLOUD_MODEL_ID: EnvVar = ("RAYLINE_CLOUD_MODEL_ID", "cloud.model.id");
const CLOUD_SMOL_MODEL_ID: EnvVar = ("RAYLINE_CLOUD_SMOL_MODEL_ID", "cloud.smol_model.id");
const LOCAL_ROUTING_ROUTER: EnvVar = ("RAYLINE_LOCAL_ROUTING_ROUTER", "local.routing.router");
const LOCAL_ROUTING_DECISIONS_MODEL_ID: EnvVar = (
    "RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_ID",
    "local.routing.decisions_model.id",
);
const LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE: EnvVar = (
    "RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE",
    "local.routing.decisions_model.v_value",
);
const LOCAL_ROUTING_CLASSIFIER_MODEL: EnvVar = (
    "RAYLINE_LOCAL_ROUTING_CLASSIFIER_MODEL",
    "local.routing.classifier_model",
);
const LOCAL_ROUTING_POLICY_PRESET: EnvVar = (
    "RAYLINE_LOCAL_ROUTING_POLICY_PRESET",
    "local.routing.policy.preset",
);
const UPDATE_CHANNEL: EnvVar = ("RAYLINE_UPDATE_CHANNEL", "update.channel");

/// The overrides of keys in `cloud`.
const CLOUD_OVERRIDES: [EnvVar; 2] = [CLOUD_MODEL_ID, CLOUD_SMOL_MODEL_ID];
/// The overrides of keys in `local.routing`.
const ROUTING_OVERRIDES: [EnvVar; 5] = [
    LOCAL_ROUTING_ROUTER,
    LOCAL_ROUTING_DECISIONS_MODEL_ID,
    LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE,
    LOCAL_ROUTING_CLASSIFIER_MODEL,
    LOCAL_ROUTING_POLICY_PRESET,
];

/// Loads the config. Precedence, lowest to highest: built-in defaults, the
/// file, env overrides, flags. A line about writing the default file goes to
/// stderr.
pub fn load(options: &LoadOptions<'_>) -> Result<Settings, ConfigError> {
    let home = crate::home().map_err(ConfigError::NoHome)?;
    load_in(&home, options)
}

/// [`load`] with the default file under `home`.
pub(crate) fn load_in(home: &Path, options: &LoadOptions<'_>) -> Result<Settings, ConfigError> {
    load_from(home, options, &|name| std::env::var_os(name), &mut |line| {
        eprintln!("{line}")
    })
}

/// Refuses what the local process must refuse before it serves anything:
/// an `api_key_env` naming an env var that isn't set, and a model ARC doesn't
/// know (`arc_knows` returns false for its id) without `context_window` or
/// `max_output_tokens`.
pub fn check_local_start(
    settings: &Settings,
    arc_knows: impl Fn(&str) -> bool,
) -> Result<(), ConfigError> {
    check_local_start_with(settings, &|name| std::env::var_os(name), &arc_knows)
}

type EnvLookup<'a> = dyn Fn(&str) -> Option<OsString> + 'a;

/// `~/.config/rayline/settings.json` under `home`.
pub(crate) fn settings_file(home: &Path) -> PathBuf {
    home.join(".config")
        .join(crate::CONFIG_DIR)
        .join("settings.json")
}

fn load_from(
    home: &Path,
    options: &LoadOptions<'_>,
    env: &EnvLookup<'_>,
    say: &mut dyn FnMut(String),
) -> Result<Settings, ConfigError> {
    let (file, text) = match options.command {
        Command::Run {
            config: Some(file), ..
        }
        | Command::LocalStart { config: Some(file) } => {
            let text = fs::read_to_string(file).map_err(|error| ConfigError::Read {
                file: file.to_path_buf(),
                error: error.to_string(),
            })?;
            (file.to_path_buf(), text)
        }
        Command::Run { config: None, .. }
        | Command::LocalStart { config: None }
        | Command::Other => {
            let file = settings_file(home);
            let text = prepare_default_file(&file, say)?;
            (file, text)
        }
    };
    let root = parse_json(&text).map_err(|failure| match failure {
        JsonFailure::Syntax(error) => ConfigError::Parse {
            file: file.clone(),
            error: format!("invalid JSON: {error}"),
        },
        JsonFailure::DuplicateKey(key) => ConfigError::Key {
            file: file.clone(),
            key,
            problem: "appears more than once".to_owned(),
        },
    })?;

    let doc = Doc { file: &file };
    let raw = doc.root(&root)?;
    let mode = match env_string(env, MODE)? {
        Some(value) => Mode::parse(&value).ok_or_else(|| ConfigError::Env {
            var: MODE.0,
            key: MODE.1,
            problem: one_of(Mode::ALL.map(Mode::as_str)),
        })?,
        None => raw.mode,
    };
    // Every section's shape is checked above, and the required keys of the
    // sections the file has here, for every command: an env override
    // replaces a value, it never gives a missing one.
    doc.check_required(
        raw.cloud.as_ref(),
        raw.local.as_ref().and_then(|local| local.routing.as_ref()),
    )?;
    // Which of `cloud` and `local` the command uses: the sections it needs,
    // and the only ones whose env overrides it reads.
    let (uses_cloud, uses_local) = match options.command {
        Command::Run { mode: flag, .. } => {
            let local = flag.unwrap_or(mode) == Mode::Local;
            (!local, local)
        }
        Command::LocalStart { .. } => (false, true),
        Command::Other => (false, false),
    };

    let cloud = match raw.cloud {
        _ if !uses_cloud => None,
        Some(mut cloud) => {
            apply_model_overrides(&mut cloud.model, env, CLOUD_MODEL_ID, None)?;
            apply_model_overrides(&mut cloud.smol_model, env, CLOUD_SMOL_MODEL_ID, None)?;
            Some(doc.finish_cloud(cloud)?)
        }
        None => {
            refuse_overrides(env, &CLOUD_OVERRIDES, "cloud")?;
            return Err(doc.err("cloud", "is required in cloud mode"));
        }
    };

    let has_local = raw.local.is_some();
    let RawLocal {
        endpoints,
        models,
        routing,
    } = raw.local.unwrap_or_default();
    // `--router` without `local.routing` starts from its defaults, which
    // env overrides refine like a file's; the flag itself wins over both. A
    // run on a local process started that way uses them too.
    let routing = match (routing, options.router.or(options.routing_defaults)) {
        (None, Some(router)) if uses_local => Some(RawRouting {
            decisions_model: Some(RawModel {
                id: Some(DEFAULT_DECISIONS_MODEL.to_owned()),
                v_value: None,
            }),
            preset: Some(DEFAULT_POLICY_PRESET.to_owned()),
            router: Some(router),
            defaults: true,
            ..RawRouting::default()
        }),
        (routing, _) => routing,
    };
    let routing = match routing {
        Some(routing) if !uses_local => {
            doc.check_router_local(&routing)?;
            None
        }
        None if !uses_local => None,
        Some(mut routing) => {
            apply_routing_overrides(&mut routing, env)?;
            if let Some(router) = options.router {
                routing.router = Some(router);
            }
            Some(doc.finish_routing(routing)?)
        }
        None => {
            refuse_overrides(env, &ROUTING_OVERRIDES, "local.routing")?;
            return Err(if has_local {
                doc.err("local.routing", "is required in local mode")
            } else {
                doc.err(
                    "local",
                    format!("is required in local mode. See {LOCAL_MODE_DOCS}"),
                )
            });
        }
    };
    let update = apply_update_override(raw.update, env)?;

    doc.check_model_endpoints(&endpoints, &models)?;
    let local = uses_local;
    if let Some(protocol) = options.harness_protocol.filter(|_| local) {
        for (id, endpoint) in &endpoints {
            if !endpoint.protocols.contains(&protocol) {
                return Err(doc.err(
                    format!("{}.protocols", child(ENDPOINTS, id)),
                    format!(
                        "does not list `{}`, the protocol of the harness being run",
                        protocol.as_str()
                    ),
                ));
            }
        }
    }
    if let Some(name) = options.env
        && !raw.environments.contains_key(name)
    {
        return Err(doc.err(
            child("environments", name),
            format!("is not defined, and `--env {name}` selects it"),
        ));
    }

    Ok(Settings {
        file,
        cloud,
        endpoints,
        models,
        routing,
        environments: raw.environments,
        update,
        local,
        env: options.env.map(str::to_owned),
    })
}

/// The key of `local.inference.endpoints`.
pub(crate) const ENDPOINTS: &str = "local.inference.endpoints";
/// The key of `local.inference.models`.
pub(crate) const MODELS: &str = "local.inference.models";

/// The default file's text, after writing the default file when it doesn't
/// exist and converting it when it is in the earlier shape. Each prints one line
/// with `say`.
fn prepare_default_file(file: &Path, say: &mut dyn FnMut(String)) -> Result<String, ConfigError> {
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            replace_file(file, DEFAULT_FILE.as_bytes())?;
            say(format!(
                "Created {} with the default settings (`mode` `cloud`)",
                file.display()
            ));
            return Ok(DEFAULT_FILE.to_owned());
        }
        Err(error) => {
            return Err(ConfigError::Read {
                file: file.to_path_buf(),
                error: error.to_string(),
            });
        }
    };
    // Text that is not a JSON object, or that has `mode`, is loaded as it
    // is.
    let root = match parse_json(&text) {
        Ok(Value::Object(root)) if !root.contains_key("mode") => root,
        _ => return Ok(text),
    };
    let (converted, mode) = convert_earlier(file, root)?;
    let backup = file.with_file_name("settings.json.bak");
    fs::write(&backup, &text).map_err(|error| ConfigError::Write {
        file: backup.clone(),
        error: format!("can't write: {error}"),
    })?;
    replace_file(file, converted.as_bytes())?;
    say(format!(
        "Converted {} from the earlier shape (`mode` `{}`); the original is {}",
        file.display(),
        mode.as_str(),
        backup.display()
    ));
    Ok(converted)
}

/// A file without `mode`, which is read as the earlier shape, in this shape,
/// and its `mode`: `local` when it has `routing`, else `cloud`. A file that
/// is not valid in the earlier shape is an error naming its key there, and
/// nothing is converted.
///
/// | Earlier shape | This shape |
/// |---|---|
/// | `endpoints`, `models` | `local.inference.endpoints`, `local.inference.models` |
/// | `routing` | `local.routing` |
/// | `routing.router` `hosted`, or none | `local.routing.router` `cloud` |
/// | `routing.decisions_model` `"<id>"` | `local.routing.decisions_model` `{"id": "<id>"}` |
/// | `environments`, `update` | the same |
/// | – | `cloud` of [`DEFAULT_FILE`], and `mode` |
fn convert_earlier(
    file: &Path,
    mut root: Map<String, Value>,
) -> Result<(String, Mode), ConfigError> {
    let err = |key: &str, problem: &str| ConfigError::Key {
        file: file.to_path_buf(),
        key: key.to_owned(),
        problem: format!("{problem} (the file has no `mode`, so it is read as the earlier shape)"),
    };
    const EARLIER_KEYS: [&str; 5] = ["endpoints", "models", "routing", "environments", "update"];
    if let Some(key) = root
        .keys()
        .find(|key| !EARLIER_KEYS.contains(&key.as_str()))
    {
        return Err(err(key, "is not a known key"));
    }
    if let Some(Value::Object(routing)) = root.get("routing") {
        if let Some(router) = routing.get("router")
            && !matches!(router.as_str(), Some("local" | "hosted"))
        {
            return Err(err("routing.router", "must be one of `local`, `hosted`"));
        }
        if let Some(model) = routing.get("decisions_model")
            && !model.is_string()
        {
            return Err(err("routing.decisions_model", "must be a string"));
        }
        if let (Some("local"), Some(id)) = (
            routing.get("router").and_then(Value::as_str),
            routing.get("decisions_model").and_then(Value::as_str),
        ) && !rayline_arc_routes::serves_model(id)
        {
            return Err(err(
                "routing.decisions_model",
                &format!(
                    "is `{id}`, which local decisions don't make: they make `arc-swift-1.0` \
                     and `arc-sage-1.0`"
                ),
            ));
        }
    }

    let mode = if root.contains_key("routing") {
        Mode::Local
    } else {
        Mode::Cloud
    };
    let mut inference = Map::new();
    for key in ["endpoints", "models"] {
        if let Some(value) = root.remove(key) {
            inference.insert(key.to_owned(), value);
        }
    }
    let mut local = Map::new();
    if !inference.is_empty() {
        local.insert("inference".to_owned(), Value::Object(inference));
    }
    if let Some(mut routing) = root.remove("routing") {
        if let Value::Object(routing) = &mut routing {
            // The earlier shape decided in the cloud when `router` was absent.
            if routing.get("router").and_then(Value::as_str) != Some("local") {
                routing.insert("router".to_owned(), Value::from(Router::Cloud.as_str()));
            }
            if let Some(Value::String(id)) = routing.get("decisions_model") {
                let model = serde_json::json!({ "id": id });
                routing.insert("decisions_model".to_owned(), model);
            }
        }
        local.insert("routing".to_owned(), routing);
    }
    let default = parse_json(DEFAULT_FILE)
        .ok()
        .and_then(|mut default| default.get_mut("cloud").map(Value::take))
        .expect("the default file has `cloud`");
    let mut entries = vec![
        ("mode".to_owned(), Value::from(mode.as_str())),
        ("cloud".to_owned(), default),
    ];
    if !local.is_empty() {
        entries.push(("local".to_owned(), Value::Object(local)));
    }
    entries.extend(root);

    // The rest of the earlier shape's rules are this shape's, under other keys.
    let doc = Doc { file };
    let converted = Value::Object(entries.iter().cloned().collect());
    doc.root(&converted)
        .and_then(|raw| {
            let local = raw.local.unwrap_or_default();
            doc.check_model_endpoints(&local.endpoints, &local.models)
        })
        .map_err(|error| match error {
            ConfigError::Key { key, problem, .. } => {
                err(&earlier_key(&key), &earlier_key(&problem))
            }
            error => error,
        })?;
    Ok((render(&entries), mode))
}

/// `text` with the keys of this shape named as in the earlier shape.
fn earlier_key(text: &str) -> String {
    text.replace("local.inference.", "")
        .replace("local.routing", "routing")
}

/// A JSON object with `entries` in their order, two-space indented.
fn render(entries: &[(String, Value)]) -> String {
    let mut text = String::from("{\n");
    for (index, (key, value)) in entries.iter().enumerate() {
        let value = serde_json::to_string_pretty(value)
            .expect("a JSON value serializes")
            .replace('\n', "\n  ");
        let comma = if index + 1 < entries.len() { "," } else { "" };
        text.push_str(&format!(
            "  {}: {value}{comma}\n",
            Value::from(key.as_str())
        ));
    }
    text.push_str("}\n");
    text
}

/// Writes `contents` to `file` through a file beside it, renamed over it, so
/// a reader sees the whole old file or the whole new one.
fn replace_file(file: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    let dir = file.parent().expect("the settings file is in a directory");
    let tmp = dir.join(format!(".settings.json.{}.tmp", std::process::id()));
    let written = fs::create_dir_all(dir)
        .and_then(|()| {
            let mut out = fs::File::create(&tmp)?;
            out.write_all(contents)?;
            out.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, file));
    written.map_err(|error| {
        let _ = fs::remove_file(&tmp);
        ConfigError::Write {
            file: file.to_path_buf(),
            error: format!("can't write: {error}"),
        }
    })
}

fn check_local_start_with(
    settings: &Settings,
    env: &EnvLookup<'_>,
    arc_knows: &dyn Fn(&str) -> bool,
) -> Result<(), ConfigError> {
    let doc = Doc {
        file: &settings.file,
    };
    for (id, endpoint) in &settings.endpoints {
        if let Some(var) = &endpoint.api_key_env
            && env(var).is_none()
        {
            return Err(doc.err(
                format!("{}.api_key_env", child(ENDPOINTS, id)),
                format!("names `{var}`, which is not set"),
            ));
        }
    }
    for (id, model) in &settings.models {
        if arc_knows(id) {
            continue;
        }
        for (key, value) in [
            ("context_window", model.context_window),
            ("max_output_tokens", model.max_output_tokens),
        ] {
            if value.is_none() {
                return Err(doc.err(
                    format!("{}.{key}", child(MODELS, id)),
                    "is required: ARC doesn't know this model",
                ));
            }
        }
    }
    Ok(())
}

/// The file after each section's own checks, before env overrides and the
/// checks across sections.
struct RawSettings {
    mode: Mode,
    cloud: Option<RawCloud>,
    local: Option<RawLocal>,
    environments: BTreeMap<String, Environment>,
    update: Option<UpdateChannel>,
}

/// A model object as written.
#[derive(Default)]
struct RawModel {
    id: Option<String>,
    v_value: Option<f64>,
}

/// `cloud` as written.
struct RawCloud {
    model: Option<RawModel>,
    smol_model: Option<RawModel>,
}

/// `local` as written.
#[derive(Default)]
struct RawLocal {
    endpoints: BTreeMap<String, Endpoint>,
    models: BTreeMap<String, Model>,
    routing: Option<RawRouting>,
}

/// `local.routing` as written, or the `--router` defaults.
#[derive(Default)]
struct RawRouting {
    decisions_model: Option<RawModel>,
    classifier_model: Option<String>,
    preset: Option<String>,
    overrides: Option<Map<String, Value>>,
    router: Option<Router>,
    arc_release: Option<ArcRelease>,
    defaults: bool,
}

/// The `id` override of a model object, and its `v_value` override where it
/// has one. Either one creates the object when the file doesn't have it,
/// which only an optional one (`cloud.smol_model`) can be.
fn apply_model_overrides(
    model: &mut Option<RawModel>,
    env: &EnvLookup<'_>,
    id_var: EnvVar,
    v_value_var: Option<EnvVar>,
) -> Result<(), ConfigError> {
    let id = env_string(env, id_var)?;
    let v_value = match v_value_var {
        Some(var) => env_string(env, var)?.map(|value| (var, value)),
        None => None,
    };
    let v_value = v_value
        .map(|(v_value_var, value)| {
            value
                .parse::<f64>()
                .ok()
                .filter(|number| number.is_finite() && *number >= 0.0)
                .ok_or_else(|| ConfigError::Env {
                    var: v_value_var.0,
                    key: v_value_var.1,
                    problem: "must be a number >= 0".to_owned(),
                })
        })
        .transpose()?;
    if id.is_none() && v_value.is_none() {
        return Ok(());
    }
    let model = model.get_or_insert_with(RawModel::default);
    if id.is_some() {
        model.id = id;
    }
    if v_value.is_some() {
        model.v_value = v_value;
    }
    Ok(())
}

fn apply_routing_overrides(
    routing: &mut RawRouting,
    env: &EnvLookup<'_>,
) -> Result<(), ConfigError> {
    apply_model_overrides(
        &mut routing.decisions_model,
        env,
        LOCAL_ROUTING_DECISIONS_MODEL_ID,
        Some(LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE),
    )?;
    if let Some(value) = env_string(env, LOCAL_ROUTING_CLASSIFIER_MODEL)? {
        routing.classifier_model = Some(value);
    }
    if let Some(value) = env_string(env, LOCAL_ROUTING_POLICY_PRESET)? {
        routing.preset = Some(value);
    }
    if let Some(value) = env_string(env, LOCAL_ROUTING_ROUTER)? {
        let (var, key) = LOCAL_ROUTING_ROUTER;
        routing.router = Some(Router::parse(&value).ok_or_else(|| ConfigError::Env {
            var,
            key,
            problem: one_of(Router::ALL.map(Router::as_str)),
        })?);
    }
    Ok(())
}

/// Refuses an override of a key in `section`, which the file doesn't have.
fn refuse_overrides(
    env: &EnvLookup<'_>,
    overrides: &[EnvVar],
    section: &str,
) -> Result<(), ConfigError> {
    match overrides.iter().find(|(var, _)| env(var).is_some()) {
        Some(&(var, key)) => Err(ConfigError::Env {
            var,
            key,
            problem: format!("is set, but the config file has no `{section}`"),
        }),
        None => Ok(()),
    }
}

fn apply_update_override(
    channel: Option<UpdateChannel>,
    env: &EnvLookup<'_>,
) -> Result<Update, ConfigError> {
    let (var, key) = UPDATE_CHANNEL;
    let channel = match env_string(env, UPDATE_CHANNEL)? {
        Some(value) => UpdateChannel::parse(&value).ok_or_else(|| ConfigError::Env {
            var,
            key,
            problem: one_of(UpdateChannel::ALL.map(UpdateChannel::as_str)),
        })?,
        None => channel.unwrap_or(UpdateChannel::Prod),
    };
    Ok(Update { channel })
}

/// An override's value, validated like a non-empty string in the file.
fn env_string(env: &EnvLookup<'_>, (var, key): EnvVar) -> Result<Option<String>, ConfigError> {
    let Some(value) = env(var) else {
        return Ok(None);
    };
    let problem = match value.into_string() {
        Ok(value) if !value.is_empty() => return Ok(Some(value)),
        Ok(_) => "must not be empty",
        Err(_) => "must be valid UTF-8",
    };
    Err(ConfigError::Env {
        var,
        key,
        problem: problem.to_owned(),
    })
}

fn one_of<const N: usize>(names: [&str; N]) -> String {
    let names = names.map(|name| format!("`{name}`"));
    format!("must be one of {}", names.join(", "))
}

/// Parses one entry of a section keyed by id: (entry, its key).
type ParseEntry<'a, T> = fn(&Doc<'a>, &Map<String, Value>, &str) -> Result<T, ConfigError>;

/// Parses and checks the file, naming it in every error.
struct Doc<'a> {
    file: &'a Path,
}

impl<'a> Doc<'a> {
    fn err(&self, key: impl Into<String>, problem: impl Into<String>) -> ConfigError {
        ConfigError::Key {
            file: self.file.to_path_buf(),
            key: key.into(),
            problem: problem.into(),
        }
    }

    fn root(&self, root: &Value) -> Result<RawSettings, ConfigError> {
        let Value::Object(root) = root else {
            return Err(ConfigError::Parse {
                file: self.file.to_path_buf(),
                error: "the top level must be an object".to_owned(),
            });
        };
        self.known_keys(
            root,
            "",
            &["mode", "cloud", "local", "environments", "update"],
        )?;
        let mode = self
            .required(root, "", "mode")?
            .as_str()
            .and_then(Mode::parse)
            .ok_or_else(|| self.err("mode", one_of(Mode::ALL.map(Mode::as_str))))?;
        Ok(RawSettings {
            mode,
            cloud: root
                .get("cloud")
                .map(|value| self.cloud(value))
                .transpose()?,
            local: root
                .get("local")
                .map(|value| self.local(value))
                .transpose()?,
            environments: self.environments(root)?,
            update: root
                .get("update")
                .map(|value| self.update(value))
                .transpose()?
                .flatten(),
        })
    }

    fn cloud(&self, value: &Value) -> Result<RawCloud, ConfigError> {
        let key = "cloud";
        let cloud = self.object(value, key)?;
        self.known_keys(cloud, key, &["model", "smol_model"])?;
        Ok(RawCloud {
            model: self.optional_model(cloud, key, "model", &["id"])?,
            smol_model: self.optional_model(cloud, key, "smol_model", &["id"])?,
        })
    }

    /// A model object at `<key>.<name>` with the keys `known`, if the object
    /// has one.
    fn optional_model(
        &self,
        object: &Map<String, Value>,
        key: &str,
        name: &str,
        known: &[&str],
    ) -> Result<Option<RawModel>, ConfigError> {
        let Some(value) = object.get(name) else {
            return Ok(None);
        };
        let key = child(key, name);
        let model = self.object(value, &key)?;
        self.known_keys(model, &key, known)?;
        let v_value = model
            .get("v_value")
            .map(|value| {
                value
                    .as_f64()
                    .filter(|number| *number >= 0.0)
                    .ok_or_else(|| self.err(child(&key, "v_value"), "must be a number >= 0"))
            })
            .transpose()?;
        Ok(Some(RawModel {
            id: self.optional_string(model, &key, "id")?,
            v_value,
        }))
    }

    fn local(&self, value: &Value) -> Result<RawLocal, ConfigError> {
        let key = "local";
        let local = self.object(value, key)?;
        self.known_keys(local, key, &["inference", "routing"])?;
        let (endpoints, models) = match local.get("inference") {
            Some(value) => {
                let key = "local.inference";
                let inference = self.object(value, key)?;
                self.known_keys(inference, key, &["endpoints", "models"])?;
                (
                    self.entries(inference, ENDPOINTS, Self::endpoint)?,
                    self.entries(inference, MODELS, Self::model)?,
                )
            }
            None => (BTreeMap::new(), BTreeMap::new()),
        };
        Ok(RawLocal {
            endpoints,
            models,
            routing: local
                .get("routing")
                .map(|value| self.routing(value))
                .transpose()?,
        })
    }

    /// The section at `section` (a key whose last name is in `parent`),
    /// keyed by id. A missing section is empty.
    fn entries<T>(
        &self,
        parent: &Map<String, Value>,
        section: &str,
        parse: ParseEntry<'a, T>,
    ) -> Result<BTreeMap<String, T>, ConfigError> {
        let name = section.rsplit('.').next().expect("a non-empty key");
        let Some(value) = parent.get(name) else {
            return Ok(BTreeMap::new());
        };
        let entries = self.object(value, section)?;
        let mut parsed = BTreeMap::new();
        for (id, value) in entries {
            let key = child(section, id);
            if id.is_empty() {
                return Err(self.err(key, "has an empty id"));
            }
            let entry = self.object(value, &key)?;
            parsed.insert(id.clone(), parse(self, entry, &key)?);
        }
        Ok(parsed)
    }

    fn endpoint(&self, entry: &Map<String, Value>, key: &str) -> Result<Endpoint, ConfigError> {
        self.known_keys(entry, key, &["base_url", "protocols", "api_key_env"])?;
        let base_url = self.url(
            self.required(entry, key, "base_url")?,
            &child(key, "base_url"),
        )?;
        let protocols_key = child(key, "protocols");
        let Value::Array(values) = self.required(entry, key, "protocols")? else {
            return Err(self.err(protocols_key, "must be a list"));
        };
        if values.is_empty() {
            return Err(self.err(protocols_key, "must not be empty"));
        }
        let mut protocols = Vec::with_capacity(values.len());
        for value in values {
            let protocol = value.as_str().and_then(Protocol::parse).ok_or_else(|| {
                self.err(
                    &protocols_key,
                    format!("entries {}", one_of(Protocol::ALL.map(Protocol::as_str))),
                )
            })?;
            if protocols.contains(&protocol) {
                return Err(self.err(
                    &protocols_key,
                    format!("lists `{}` more than once", protocol.as_str()),
                ));
            }
            protocols.push(protocol);
        }
        Ok(Endpoint {
            base_url,
            protocols,
            api_key_env: self.optional_string(entry, key, "api_key_env")?,
        })
    }

    fn model(&self, entry: &Map<String, Value>, key: &str) -> Result<Model, ConfigError> {
        self.known_keys(
            entry,
            key,
            &[
                "endpoint",
                "model",
                "context_window",
                "max_output_tokens",
                "tokenizer",
                "price",
            ],
        )?;
        Ok(Model {
            endpoint: self.string(
                self.required(entry, key, "endpoint")?,
                &child(key, "endpoint"),
            )?,
            model: self.string(self.required(entry, key, "model")?, &child(key, "model"))?,
            context_window: self.optional_count(entry, key, "context_window")?,
            max_output_tokens: self.optional_count(entry, key, "max_output_tokens")?,
            tokenizer: self.optional_string(entry, key, "tokenizer")?,
            price: entry
                .get("price")
                .map(|value| self.price(value, &child(key, "price")))
                .transpose()?,
        })
    }

    fn price(&self, value: &Value, key: &str) -> Result<Price, ConfigError> {
        let price = self.object(value, key)?;
        self.known_keys(
            price,
            key,
            &["input", "output", "cache_read", "cache_write"],
        )?;
        let number = |name: &str| {
            self.required(price, key, name)?
                .as_f64()
                .filter(|number| *number >= 0.0)
                .ok_or_else(|| self.err(child(key, name), "must be a number >= 0"))
        };
        Ok(Price {
            input: number("input")?,
            output: number("output")?,
            cache_read: number("cache_read")?,
            cache_write: number("cache_write")?,
        })
    }

    fn routing(&self, value: &Value) -> Result<RawRouting, ConfigError> {
        let key = "local.routing";
        let routing = self.object(value, key)?;
        self.known_keys(
            routing,
            key,
            &[
                "decisions_model",
                "classifier_model",
                "policy",
                "router",
                "arc_release",
            ],
        )?;
        let mut raw = RawRouting {
            decisions_model: self.optional_model(
                routing,
                key,
                "decisions_model",
                &["id", "v_value"],
            )?,
            classifier_model: self.optional_string(routing, key, "classifier_model")?,
            router: routing
                .get("router")
                .map(|value| {
                    value.as_str().and_then(Router::parse).ok_or_else(|| {
                        self.err(
                            "local.routing.router",
                            one_of(Router::ALL.map(Router::as_str)),
                        )
                    })
                })
                .transpose()?,
            arc_release: routing
                .get("arc_release")
                .map(|value| self.arc_release(value))
                .transpose()?,
            ..RawRouting::default()
        };
        if let Some(value) = routing.get("policy") {
            let key = "local.routing.policy";
            let policy = self.object(value, key)?;
            self.known_keys(policy, key, &["preset", "overrides"])?;
            raw.preset = self.optional_string(policy, key, "preset")?;
            raw.overrides = policy
                .get("overrides")
                .map(|value| {
                    self.object(value, "local.routing.policy.overrides")
                        .cloned()
                })
                .transpose()?;
        }
        Ok(raw)
    }

    fn arc_release(&self, value: &Value) -> Result<ArcRelease, ConfigError> {
        let key = "local.routing.arc_release";
        let release = self.object(value, key)?;
        self.known_keys(
            release,
            key,
            &["repo", "revision", "manifest_sha256", "dir"],
        )?;
        let field = |name: &str| -> Result<String, ConfigError> {
            self.string(self.required(release, key, name)?, &child(key, name))
        };
        let hex = |name: &str, digits: usize| -> Result<String, ConfigError> {
            let value = field(name)?;
            if value.len() != digits
                || !value
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Err(self.err(
                    child(key, name),
                    format!("must be {digits} lowercase hex digits"),
                ));
            }
            Ok(value)
        };
        let repo = field("repo")?;
        let valid_repo = repo.split_once('/').is_some_and(|(owner, name)| {
            let part = |part: &str| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
                    && part != "."
                    && part != ".."
            };
            part(owner) && part(name)
        });
        if !valid_repo {
            return Err(self.err(
                child(key, "repo"),
                "must be a Hugging Face repo `<owner>/<name>`",
            ));
        }
        let dir = self.optional_string(release, key, "dir")?;
        let plain = |part: &str| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        };
        if dir.as_deref().is_some_and(|dir| !dir.split('/').all(plain)) {
            return Err(self.err(
                child(key, "dir"),
                "must be a relative path of plain names joined by `/`",
            ));
        }
        Ok(ArcRelease {
            repo,
            revision: hex("revision", 40)?,
            manifest_sha256: hex("manifest_sha256", 64)?,
            dir,
        })
    }

    /// A model object with its required `id`, and its `v_value`.
    fn finish_model(
        &self,
        model: Option<RawModel>,
        key: &str,
    ) -> Result<(String, Option<f64>), ConfigError> {
        let model = model.ok_or_else(|| self.err(key, "is required"))?;
        let id = model
            .id
            .ok_or_else(|| self.err(child(key, "id"), "is required"))?;
        Ok((id, model.v_value))
    }

    /// The required keys of the `cloud` and `local.routing` the file has, as
    /// written: `cloud.model` and its `id`, a `cloud.smol_model`'s `id`, and
    /// `local.routing`'s `decisions_model` with its `id`, `router` and
    /// `policy.preset`.
    fn check_required(
        &self,
        cloud: Option<&RawCloud>,
        routing: Option<&RawRouting>,
    ) -> Result<(), ConfigError> {
        let model = |model: Option<&RawModel>, key: &str| match model {
            None => Err(self.err(key, "is required")),
            Some(RawModel { id: None, .. }) => Err(self.err(child(key, "id"), "is required")),
            Some(_) => Ok(()),
        };
        if let Some(cloud) = cloud {
            model(cloud.model.as_ref(), "cloud.model")?;
            if let Some(smol_model) = &cloud.smol_model {
                model(Some(smol_model), "cloud.smol_model")?;
            }
        }
        if let Some(routing) = routing {
            model(
                routing.decisions_model.as_ref(),
                "local.routing.decisions_model",
            )?;
            if routing.router.is_none() {
                return Err(self.err("local.routing.router", "is required"));
            }
            if routing.preset.is_none() {
                return Err(self.err("local.routing.policy.preset", "is required"));
            }
        }
        Ok(())
    }

    /// `cloud` with its required keys present.
    fn finish_cloud(&self, cloud: RawCloud) -> Result<Cloud, ConfigError> {
        let cloud_model = |model, key| {
            self.finish_model(model, key)
                .map(|(id, _)| CloudModel { id })
        };
        let model = cloud_model(cloud.model, "cloud.model")?;
        let smol_model = match cloud.smol_model {
            Some(smol_model) => cloud_model(Some(smol_model), "cloud.smol_model")?,
            None => model.clone(),
        };
        Ok(Cloud { model, smol_model })
    }

    /// With `router` `local`: a `decisions_model` local decisions make, and
    /// no `v_value`.
    fn check_router_local(&self, routing: &RawRouting) -> Result<(), ConfigError> {
        if routing.router != Some(Router::Local) {
            return Ok(());
        }
        let Some(model) = &routing.decisions_model else {
            return Ok(());
        };
        if let Some(id) = &model.id
            && !rayline_arc_routes::serves_model(id)
        {
            return Err(self.err(
                "local.routing.decisions_model.id",
                format!(
                    "is `{id}`, which local decisions don't make: they make \
                     `arc-swift-1.0` and `arc-sage-1.0`. Use one of those, or set \
                     `local.routing.router` to `cloud`"
                ),
            ));
        }
        if model.v_value.is_some() {
            return Err(self.err(
                "local.routing.decisions_model.v_value",
                "must not be set with `local.routing.router` `local`: local decisions \
                 take no `v_value`",
            ));
        }
        Ok(())
    }

    /// `local.routing` with its required keys present.
    fn finish_routing(&self, routing: RawRouting) -> Result<Routing, ConfigError> {
        self.check_router_local(&routing)?;
        let required =
            |value: Option<String>, key: &str| value.ok_or_else(|| self.err(key, "is required"));
        let (id, v_value) =
            self.finish_model(routing.decisions_model, "local.routing.decisions_model")?;
        let decisions_model = DecisionsModel { id, v_value };
        let router = routing
            .router
            .ok_or_else(|| self.err("local.routing.router", "is required"))?;
        Ok(Routing {
            decisions_model,
            classifier_model: routing.classifier_model,
            policy: Policy {
                preset: required(routing.preset, "local.routing.policy.preset")?,
                overrides: routing.overrides,
            },
            router,
            arc_release: routing.arc_release,
            defaults: routing.defaults,
        })
    }

    /// Every model's `endpoint` names one of `local.inference.endpoints`.
    fn check_model_endpoints(
        &self,
        endpoints: &BTreeMap<String, Endpoint>,
        models: &BTreeMap<String, Model>,
    ) -> Result<(), ConfigError> {
        match models
            .iter()
            .find(|(_, model)| !endpoints.contains_key(&model.endpoint))
        {
            Some((id, model)) => Err(self.err(
                format!("{}.endpoint", child(MODELS, id)),
                format!("names `{}`, which is not in `{ENDPOINTS}`", model.endpoint),
            )),
            None => Ok(()),
        }
    }

    /// `environments`, whose names follow the `--env` name rule.
    fn environments(
        &self,
        root: &Map<String, Value>,
    ) -> Result<BTreeMap<String, Environment>, ConfigError> {
        let environments = self.entries(root, "environments", Self::environment)?;
        if let Some(name) = environments
            .keys()
            .find(|name| !crate::status::is_valid_root_env(name))
        {
            return Err(self.err(
                child("environments", name),
                "is not a valid environment name (use letters, digits, `-` and `_`)",
            ));
        }
        Ok(environments)
    }

    fn environment(
        &self,
        entry: &Map<String, Value>,
        key: &str,
    ) -> Result<Environment, ConfigError> {
        self.known_keys(entry, key, &["base_url", "login_url"])?;
        let base_url_key = child(key, "base_url");
        let base_url = self.url(self.required(entry, key, "base_url")?, &base_url_key)?;
        // Decisions go only to an environment's router, so a router URL they
        // could not go to is an error here rather than a refusal later.
        if let Some(problem) = rayline_local_router::decisions_url::router_url_problem(&base_url) {
            return Err(self.err(
                base_url_key,
                format!(
                    "{problem}, not {}",
                    rayline_local_router::decisions_url::without_credentials(&base_url)
                ),
            ));
        }
        Ok(Environment {
            base_url,
            login_url: self.url(
                self.required(entry, key, "login_url")?,
                &child(key, "login_url"),
            )?,
        })
    }

    fn update(&self, value: &Value) -> Result<Option<UpdateChannel>, ConfigError> {
        let key = "update";
        let update = self.object(value, key)?;
        self.known_keys(update, key, &["channel"])?;
        update
            .get("channel")
            .map(|value| {
                value
                    .as_str()
                    .and_then(UpdateChannel::parse)
                    .ok_or_else(|| {
                        self.err(
                            "update.channel",
                            one_of(UpdateChannel::ALL.map(UpdateChannel::as_str)),
                        )
                    })
            })
            .transpose()
    }

    fn known_keys(
        &self,
        object: &Map<String, Value>,
        key: &str,
        known: &[&str],
    ) -> Result<(), ConfigError> {
        match object.keys().find(|name| !known.contains(&name.as_str())) {
            Some(name) => Err(self.err(child(key, name), "is not a known key")),
            None => Ok(()),
        }
    }

    fn object<'v>(
        &self,
        value: &'v Value,
        key: &str,
    ) -> Result<&'v Map<String, Value>, ConfigError> {
        value
            .as_object()
            .ok_or_else(|| self.err(key, "must be an object"))
    }

    fn required<'v>(
        &self,
        object: &'v Map<String, Value>,
        key: &str,
        name: &str,
    ) -> Result<&'v Value, ConfigError> {
        object
            .get(name)
            .ok_or_else(|| self.err(child(key, name), "is required"))
    }

    fn string(&self, value: &Value, key: &str) -> Result<String, ConfigError> {
        match value.as_str() {
            Some("") => Err(self.err(key, "must not be empty")),
            Some(value) => Ok(value.to_owned()),
            None => Err(self.err(key, "must be a string")),
        }
    }

    fn optional_string(
        &self,
        object: &Map<String, Value>,
        key: &str,
        name: &str,
    ) -> Result<Option<String>, ConfigError> {
        object
            .get(name)
            .map(|value| self.string(value, &child(key, name)))
            .transpose()
    }

    /// An absolute `http` or `https` URL.
    fn url(&self, value: &Value, key: &str) -> Result<Url, ConfigError> {
        let text = self.string(value, key)?;
        match Url::parse(&text) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => Ok(url),
            _ => Err(self.err(key, "must be an http or https URL")),
        }
    }

    /// An optional integer > 0.
    fn optional_count(
        &self,
        object: &Map<String, Value>,
        key: &str,
        name: &str,
    ) -> Result<Option<u64>, ConfigError> {
        object
            .get(name)
            .map(|value| {
                value
                    .as_u64()
                    .filter(|count| *count > 0)
                    .ok_or_else(|| self.err(child(key, name), "must be an integer > 0"))
            })
            .transpose()
    }
}

/// The key of `name` inside `key`, e.g. `endpoints.gpu-box`. A name that is
/// not a plain word is quoted: `models."z-ai/glm-5.3-flash"`.
pub(crate) fn child(key: &str, name: &str) -> String {
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let name = if plain {
        name.to_owned()
    } else {
        Value::String(name.to_owned()).to_string()
    };
    if key.is_empty() {
        name
    } else {
        format!("{key}.{name}")
    }
}

/// Why a file's text is not a JSON document Rayline accepts.
pub(crate) enum JsonFailure {
    Syntax(serde_json::Error),
    /// An object repeats a key: the repeated key.
    DuplicateKey(String),
}

/// Parses JSON text, refusing any object that repeats a key.
pub(crate) fn parse_json(text: &str) -> Result<Value, JsonFailure> {
    let duplicate = RefCell::new(None);
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let parsed = StrictValue {
        key: String::new(),
        duplicate: &duplicate,
    }
    .deserialize(&mut deserializer)
    .and_then(|value| deserializer.end().map(|()| value));
    match (parsed, duplicate.into_inner()) {
        (_, Some(key)) => Err(JsonFailure::DuplicateKey(key)),
        (Ok(value), None) => Ok(value),
        (Err(error), None) => Err(JsonFailure::Syntax(error)),
    }
}

/// Deserializes one JSON value at `key`. A repeated key in an object is
/// recorded in `duplicate` and stops the parse.
struct StrictValue<'a> {
    key: String,
    duplicate: &'a RefCell<Option<String>>,
}

impl<'de> DeserializeSeed<'de> for StrictValue<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for StrictValue<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(StrictValue {
            key: format!("{}[{}]", self.key, values.len()),
            duplicate: self.duplicate,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(name) = map.next_key::<String>()? {
            let key = child(&self.key, &name);
            if object.contains_key(&name) {
                *self.duplicate.borrow_mut() = Some(key);
                return Err(de::Error::custom("duplicate key"));
            }
            let value = map.next_value_seed(StrictValue {
                key,
                duplicate: self.duplicate,
            })?;
            object.insert(name, value);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_HOME: AtomicU64 = AtomicU64::new(0);

    const GLM: &str = "z-ai/glm-5.3-flash";
    /// Key paths in [`example`], their names joined by spaces.
    const EP: &str = "local inference endpoints";
    const MD: &str = "local inference models";
    const RT: &str = "local routing";

    fn temp_home() -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "rayline-config-test-{}-{}",
            std::process::id(),
            NEXT_HOME.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");
        home
    }

    /// Local mode with cloud ARC, with every optional key and an internal
    /// environment.
    fn example() -> Value {
        json!({
            "mode": "local",
            "cloud": {
                "model": { "id": "arc-sage-1.0" },
                "smol_model": { "id": "arc-swift-1.0" }
            },
            "local": {
                "inference": {
                    "endpoints": {
                        "gpu-box": {
                            "base_url": "https://gpu-box.example.com:8443",
                            "protocols": ["anthropic_messages", "openai_chat", "openai_responses"],
                            "api_key_env": "GPU_BOX_API_KEY"
                        },
                        "ollama": {
                            "base_url": "http://127.0.0.1:11434",
                            "protocols": ["openai_chat", "anthropic_messages"]
                        }
                    },
                    "models": {
                        GLM: {
                            "endpoint": "gpu-box",
                            "model": "GLM-5.3-Flash-EXL3",
                            "context_window": 1_000_000,
                            "max_output_tokens": 32_000,
                            "tokenizer": "zai-org/GLM-5.3-Flash",
                            "price": { "input": 0, "output": 0, "cache_read": 0, "cache_write": 0 }
                        }
                    }
                },
                "routing": {
                    "router": "cloud",
                    "decisions_model": { "id": "arc-swift-1.0" },
                    "classifier_model": "anthropic/claude-haiku-5.5",
                    "policy": {
                        "preset": "arc-policy.prefer-local.v1",
                        "overrides": {
                            "selection": { "min_p_pass": 0.5 },
                            "schedule": { "model": ["cache_boundaries", "user_messages"] }
                        }
                    }
                }
            },
            "environments": {
                "dev": {
                    "base_url": "https://api.dev.example.com",
                    "login_url": "https://platform.dev.example.com/cli-auth"
                }
            },
            "update": { "channel": "dev" }
        })
    }

    /// The names in `path`, joined by spaces.
    fn names(path: &str) -> Vec<&str> {
        path.split(' ').collect()
    }

    fn set(doc: &mut Value, path: &str, value: Value) {
        let path = names(path);
        let (last, parents) = path.split_last().expect("non-empty path");
        let mut node = doc;
        for name in parents {
            node = node.get_mut(*name).expect("parent exists");
        }
        node.as_object_mut()
            .expect("parent is an object")
            .insert((*last).to_owned(), value);
    }

    fn remove(doc: &mut Value, path: &str) {
        let path = names(path);
        let (last, parents) = path.split_last().expect("non-empty path");
        let mut node = doc;
        for name in parents {
            node = node.get_mut(*name).expect("parent exists");
        }
        node.as_object_mut()
            .expect("parent is an object")
            .remove(*last)
            .expect("key exists");
    }

    fn write_default(home: &Path, doc: &Value) -> PathBuf {
        write_default_text(home, &doc.to_string())
    }

    fn write_default_text(home: &Path, text: &str) -> PathBuf {
        let file = settings_file(home);
        fs::create_dir_all(file.parent().expect("settings dir")).expect("create settings dir");
        fs::write(&file, text).expect("write settings");
        file
    }

    fn write_file(home: &Path, name: &str, doc: &Value) -> PathBuf {
        let file = home.join(name);
        fs::write(&file, doc.to_string()).expect("write config");
        file
    }

    /// `rayline run` on the default file, with `--mode` `mode`.
    fn run(mode: Option<Mode>) -> Command<'static> {
        Command::Run { config: None, mode }
    }

    /// `rayline run --config <file>`.
    fn run_file(file: &Path) -> Command<'_> {
        Command::Run {
            config: Some(file),
            mode: None,
        }
    }

    /// `rayline local start` on the default file.
    const START: Command<'static> = Command::LocalStart { config: None };

    fn start_file(file: &Path) -> Command<'_> {
        Command::LocalStart { config: Some(file) }
    }

    fn opts(command: Command<'_>) -> LoadOptions<'_> {
        LoadOptions {
            command,
            env: None,
            harness_protocol: None,
            router: None,
            routing_defaults: None,
        }
    }

    /// Loads with the env vars `env`: the result, and the lines it printed.
    fn load_said(
        home: &Path,
        options: LoadOptions<'_>,
        env: &[(&str, &str)],
    ) -> (Result<Settings, ConfigError>, Vec<String>) {
        let env: HashMap<&str, OsString> = env
            .iter()
            .map(|(name, value)| (*name, OsString::from(value)))
            .collect();
        let mut said = Vec::new();
        let result = load_from(
            home,
            &options,
            &|name| env.get(name).cloned(),
            &mut |line| said.push(line),
        );
        if let Err(error) = &result {
            let file = match options.command {
                Command::Run {
                    config: Some(file), ..
                }
                | Command::LocalStart { config: Some(file) } => file.to_path_buf(),
                _ => settings_file(home),
            };
            assert_names_the_file(error, &file);
        }
        (result, said)
    }

    fn load_env(
        home: &Path,
        options: LoadOptions<'_>,
        env: &[(&str, &str)],
    ) -> Result<Settings, ConfigError> {
        load_said(home, options, env).0
    }

    /// Asserts that a load error about the file names `expected` and renders
    /// as `<file>: ...`, and that a key error renders as
    /// ``<file>: `<key>` <problem>``. An env override error names the env var
    /// and the key it overrides.
    #[track_caller]
    fn assert_names_the_file(error: &ConfigError, expected: &Path) {
        let message = error.to_string();
        match error {
            ConfigError::Key { file, key, problem } => {
                assert_eq!(file, expected, "{message}");
                assert_eq!(
                    message,
                    format!("{}: `{key}` {problem}", expected.display())
                );
            }
            ConfigError::Read { file, .. } | ConfigError::Parse { file, .. } => {
                assert_eq!(file, expected, "{message}");
                assert!(
                    message.starts_with(&format!("{}: ", expected.display())),
                    "{message}"
                );
            }
            ConfigError::Write { file, .. } => assert!(
                message.starts_with(&format!("{}: ", file.display())),
                "{message}"
            ),
            ConfigError::Env { var, key, .. } => assert!(
                message.starts_with(&format!("`{var}` (overrides `{key}`) ")),
                "{message}"
            ),
            ConfigError::NoHome(_) => panic!("expected an error about {}", expected.display()),
        }
    }

    /// Runs the start check, asserting that an error names the loaded file.
    fn check_start(
        settings: &Settings,
        env: &EnvLookup<'_>,
        arc_knows: &dyn Fn(&str) -> bool,
    ) -> Result<(), ConfigError> {
        let result = check_local_start_with(settings, env, arc_knows);
        if let Err(error) = &result {
            assert_names_the_file(error, &settings.file);
        }
        result
    }

    fn load(home: &Path, options: LoadOptions<'_>) -> Result<Settings, ConfigError> {
        load_env(home, options, &[])
    }

    /// Loads `doc` as the default file for a `rayline run` in its mode.
    fn load_doc(doc: &Value) -> Result<Settings, ConfigError> {
        load_doc_for(doc, run(None))
    }

    fn load_doc_for(doc: &Value, command: Command<'_>) -> Result<Settings, ConfigError> {
        let home = temp_home();
        write_default(&home, doc);
        load(&home, opts(command))
    }

    #[track_caller]
    fn key_error<T: fmt::Debug>(result: Result<T, ConfigError>) -> (String, String) {
        match result {
            Err(ConfigError::Key { key, problem, .. }) => (key, problem),
            other => panic!("expected a key error, got {other:?}"),
        }
    }

    #[track_caller]
    fn env_error(result: Result<Settings, ConfigError>) -> (&'static str, String) {
        match result {
            Err(ConfigError::Env { var, problem, .. }) => (var, problem),
            other => panic!("expected an env error, got {other:?}"),
        }
    }

    fn cloud(model: &str, smol_model: &str) -> Cloud {
        Cloud {
            model: CloudModel {
                id: model.to_owned(),
            },
            smol_model: CloudModel {
                id: smol_model.to_owned(),
            },
        }
    }

    /// The whole settings files in `docs/*.md` and `README.md`: each ```json
    /// block that opens with `mode`, named by its file and line.
    pub(crate) fn doc_examples() -> Vec<(String, Value)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files: Vec<PathBuf> = fs::read_dir(root.join("docs"))
            .expect("docs")
            .map(|entry| entry.expect("docs entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        files.sort();
        files.push(root.join("README.md"));
        let mut examples = Vec::new();
        for file in files {
            let text = fs::read_to_string(&file).expect("read doc");
            let mut lines = text.lines().enumerate();
            while let Some((number, line)) = lines.next() {
                if line.trim() != "```json" {
                    continue;
                }
                let block: Vec<&str> = lines
                    .by_ref()
                    .map(|(_, line)| line)
                    .take_while(|line| line.trim() != "```")
                    .collect();
                let opens_with_mode = block.first().is_some_and(|line| line.trim() == "{")
                    && block
                        .get(1)
                        .is_some_and(|line| line.trim().starts_with("\"mode\":"));
                if !opens_with_mode {
                    continue;
                }
                let name = format!(
                    "{}:{}",
                    file.file_name().expect("doc name").to_string_lossy(),
                    number + 1
                );
                let Ok(value) = parse_json(&block.join("\n")) else {
                    panic!("{name} is not JSON");
                };
                examples.push((name, value));
            }
        }
        examples
    }

    // The default file.

    #[test]
    fn a_missing_default_file_is_written_in_cloud_mode() {
        let home = temp_home();
        let file = settings_file(&home);
        let (settings, said) = load_said(&home, opts(run(None)), &[]);
        let settings = settings.expect("load");
        assert_eq!(
            said,
            [format!(
                "Created {} with the default settings (`mode` `cloud`)",
                file.display()
            )]
        );
        assert_eq!(fs::read_to_string(&file).expect("written"), DEFAULT_FILE);
        assert_eq!(
            settings,
            Settings {
                file: file.clone(),
                cloud: Some(cloud("arc-sage-1.0", "arc-swift-1.0")),
                endpoints: BTreeMap::new(),
                models: BTreeMap::new(),
                routing: None,
                environments: BTreeMap::new(),
                update: Update {
                    channel: UpdateChannel::Prod
                },
                local: false,
                env: None,
            }
        );
        // Written once: the next load reads it and says nothing.
        let (again, said) = load_said(&home, opts(run(None)), &[]);
        assert_eq!(again.expect("load"), settings);
        assert!(said.is_empty(), "{said:?}");
    }

    #[test]
    fn every_command_that_reads_the_default_file_writes_it() {
        for command in [run(None), run(Some(Mode::Local)), START, Command::Other] {
            let home = temp_home();
            let (_, said) = load_said(&home, opts(command), &[]);
            assert_eq!(said.len(), 1, "{command:?}");
            assert_eq!(
                fs::read_to_string(settings_file(&home)).expect("written"),
                DEFAULT_FILE,
                "{command:?}"
            );
        }
    }

    #[test]
    fn a_missing_config_file_is_a_read_error_and_nothing_is_written() {
        let home = temp_home();
        let missing = home.join("missing.json");
        for command in [run_file(&missing), start_file(&missing)] {
            let (result, said) = load_said(&home, opts(command), &[]);
            match result {
                Err(ConfigError::Read { file, .. }) => assert_eq!(file, missing),
                other => panic!("expected a read error, got {other:?}"),
            }
            assert!(said.is_empty(), "{said:?}");
            assert!(!settings_file(&home).exists());
            assert!(!missing.exists());
        }
    }

    // The shape.

    #[test]
    fn example_loads_into_typed_settings() {
        let settings = load_doc(&example()).expect("load");
        let gpu_box = &settings.endpoints["gpu-box"];
        assert_eq!(
            gpu_box.base_url.as_str(),
            "https://gpu-box.example.com:8443/"
        );
        assert_eq!(gpu_box.protocols, Protocol::ALL.to_vec());
        assert_eq!(gpu_box.api_key_env.as_deref(), Some("GPU_BOX_API_KEY"));
        assert_eq!(settings.endpoints["ollama"].api_key_env, None);
        assert_eq!(
            settings.models[GLM],
            Model {
                endpoint: "gpu-box".to_owned(),
                model: "GLM-5.3-Flash-EXL3".to_owned(),
                context_window: Some(1_000_000),
                max_output_tokens: Some(32_000),
                tokenizer: Some("zai-org/GLM-5.3-Flash".to_owned()),
                price: Some(Price {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                }),
            }
        );
        let routing = settings.routing.as_ref().expect("routing");
        assert_eq!(routing.router, Router::Cloud);
        assert_eq!(
            routing.decisions_model,
            DecisionsModel {
                id: "arc-swift-1.0".to_owned(),
                v_value: None
            }
        );
        assert_eq!(
            routing.classifier_model.as_deref(),
            Some("anthropic/claude-haiku-5.5")
        );
        assert_eq!(routing.policy.preset, "arc-policy.prefer-local.v1");
        assert_eq!(
            routing.policy.overrides.clone().map(Value::Object),
            Some(example()["local"]["routing"]["policy"]["overrides"].clone())
        );
        assert!(!routing.defaults);
        assert_eq!(
            settings.environments["dev"].login_url.as_str(),
            "https://platform.dev.example.com/cli-auth"
        );
        assert_eq!(settings.update.channel, UpdateChannel::Dev);
        assert!(settings.local, "the file's mode is local");
        assert_eq!(settings.cloud, None, "a local-mode run doesn't use `cloud`");

        let settings = load_doc_for(&example(), run(Some(Mode::Cloud))).expect("load");
        assert!(!settings.local);
        assert_eq!(settings.cloud, Some(cloud("arc-sage-1.0", "arc-swift-1.0")));
        assert_eq!(
            settings.routing, None,
            "a cloud-mode run doesn't use routing"
        );
        // The local inference sections are typed whatever the mode.
        assert_eq!(settings.models.len(), 1);
    }

    #[test]
    fn optional_keys_may_be_left_out() {
        let doc = json!({
            "mode": "local",
            "local": {
                "inference": {
                    "endpoints": { "box": { "base_url": "http://box", "protocols": ["openai_chat"] } },
                    "models": { "m": { "endpoint": "box", "model": "m" } }
                },
                "routing": {
                    "router": "cloud",
                    "decisions_model": { "id": "d" },
                    "policy": { "preset": "p" }
                }
            },
            "update": {}
        });
        let settings = load_doc(&doc).expect("load");
        let model = &settings.models["m"];
        assert_eq!(model.context_window, None);
        assert_eq!(model.max_output_tokens, None);
        assert_eq!(model.tokenizer, None);
        assert_eq!(model.price, None);
        let routing = settings.routing.expect("routing");
        assert_eq!(routing.policy.overrides, None);
        assert_eq!(routing.classifier_model, None);
        assert_eq!(routing.arc_release, None);
        assert_eq!(routing.decisions_model.v_value, None);
        assert_eq!(settings.update.channel, UpdateChannel::Prod);
        // `local` needs no `inference`, and a cloud-mode file no `local`.
        let routing_only = json!({"mode": "local", "local": {"routing": doc["local"]["routing"]}});
        assert!(load_doc(&routing_only).expect("load").endpoints.is_empty());
        let cloud_only = json!({"mode": "cloud", "cloud": {"model": {"id": "m"}}, "local": {}});
        assert!(!load_doc(&cloud_only).expect("load").local);
    }

    #[test]
    fn smol_model_defaults_to_model() {
        let doc = json!({"mode": "cloud", "cloud": {"model": {"id": "arc-sage-1.0"}}});
        assert_eq!(
            load_doc(&doc).expect("load").cloud,
            Some(cloud("arc-sage-1.0", "arc-sage-1.0"))
        );
    }

    #[test]
    fn model_ids_are_taken_as_written() {
        let mut doc = example();
        set(&mut doc, "mode", json!("cloud"));
        set(&mut doc, "cloud model id", json!("any/model-at-all"));
        set(
            &mut doc,
            "cloud smol_model id",
            json!("arc-swift-contributor-1.0"),
        );
        set(
            &mut doc,
            &format!("{RT} decisions_model id"),
            json!("arc-1.0"),
        );
        assert_eq!(
            load_doc(&doc).expect("load").cloud,
            Some(cloud("any/model-at-all", "arc-swift-contributor-1.0"))
        );
        let routing = load_doc_for(&doc, START)
            .expect("load")
            .routing
            .expect("routing");
        assert_eq!(routing.decisions_model.id, "arc-1.0");
    }

    #[test]
    fn mode_comes_from_the_flag_then_the_env_then_the_file() {
        let home = temp_home();
        write_default(&home, &example());
        let mode = |command, env: &[(&str, &str)]| {
            let settings = load_env(&home, opts(command), env).expect("load");
            assert_eq!(settings.cloud.is_some(), !settings.local);
            assert_eq!(settings.routing.is_some(), settings.local);
            if settings.local {
                Mode::Local
            } else {
                Mode::Cloud
            }
        };
        assert_eq!(mode(run(None), &[]), Mode::Local, "the file");
        assert_eq!(mode(run(None), &[("RAYLINE_MODE", "cloud")]), Mode::Cloud);
        assert_eq!(mode(run(Some(Mode::Cloud)), &[]), Mode::Cloud);
        assert_eq!(
            mode(run(Some(Mode::Local)), &[("RAYLINE_MODE", "cloud")]),
            Mode::Local,
            "the flag wins over the env"
        );
        let mut doc = example();
        set(&mut doc, "mode", json!("cloud"));
        write_default(&home, &doc);
        assert_eq!(mode(run(None), &[]), Mode::Cloud, "the file");
        assert_eq!(mode(run(None), &[("RAYLINE_MODE", "local")]), Mode::Local);
        assert_eq!(
            mode(run(Some(Mode::Cloud)), &[("RAYLINE_MODE", "local")]),
            Mode::Cloud
        );
        // `local start` uses `local` whatever the mode.
        assert_eq!(mode(START, &[("RAYLINE_MODE", "cloud")]), Mode::Local);
        // The other commands use neither section.
        let other = load(&home, opts(Command::Other)).expect("load");
        assert_eq!(
            (other.cloud, other.routing, other.local),
            (None, None, false)
        );
        // `--mode` works on a `--config` file too.
        let file = write_file(&home, "other.json", &doc);
        let settings = load(
            &home,
            opts(Command::Run {
                config: Some(&file),
                mode: Some(Mode::Local),
            }),
        )
        .expect("load");
        assert!(settings.local);
        assert_eq!(settings.file, file);
    }

    #[test]
    fn mode_is_required_and_is_cloud_or_local() {
        let mut doc = example();
        remove(&mut doc, "mode");
        let home = temp_home();
        let file = write_file(&home, "config.json", &doc);
        assert_eq!(
            key_error(load(&home, opts(run_file(&file)))),
            ("mode".to_owned(), "is required".to_owned())
        );
        for value in [json!("hosted"), json!("Cloud"), json!(""), json!(1)] {
            let mut doc = example();
            set(&mut doc, "mode", value.clone());
            assert_eq!(
                key_error(load_doc(&doc)),
                (
                    "mode".to_owned(),
                    "must be one of `cloud`, `local`".to_owned()
                ),
                "{value}"
            );
        }
        let home = temp_home();
        write_default(&home, &example());
        for value in ["hosted", "Local"] {
            assert_eq!(
                env_error(load_env(&home, opts(run(None)), &[("RAYLINE_MODE", value)])),
                ("RAYLINE_MODE", "must be one of `cloud`, `local`".to_owned())
            );
        }
    }

    /// `schema/settings.schema.json`, compiled. Rayline doesn't read it; the
    /// tests keep it in step with the loader and the docs.
    fn settings_schema() -> (boon::Schemas, boon::SchemaIndex) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/settings.schema.json");
        let schema = parse_json(&fs::read_to_string(path).expect("read the schema"))
            .ok()
            .expect("the schema is JSON");
        let url = schema["$id"]
            .as_str()
            .expect("the schema has an $id")
            .to_owned();
        let mut compiler = boon::Compiler::new();
        compiler.add_resource(&url, schema).expect("add the schema");
        let mut schemas = boon::Schemas::new();
        let index = compiler
            .compile(&url, &mut schemas)
            .expect("compile the schema");
        (schemas, index)
    }

    /// Every settings file in the docs, the default file and the acceptance
    /// profiles pass the schema.
    #[test]
    fn the_schema_passes_every_example() {
        let (schemas, index) = settings_schema();
        let mut files = doc_examples();
        files.push((
            "default-settings.json".to_owned(),
            parse_json(DEFAULT_FILE)
                .ok()
                .expect("the default file is JSON"),
        ));
        let profiles =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/acceptance/profiles");
        for entry in fs::read_dir(profiles).expect("profiles") {
            let path = entry.expect("profile").path();
            let text = fs::read_to_string(&path).expect("read profile");
            let value = parse_json(&text).ok().expect("a profile is JSON");
            files.push((path.display().to_string(), value));
        }
        for (name, file) in &files {
            if let Err(error) = schemas.validate(file, index) {
                panic!("{name} fails the schema: {error}");
            }
        }
    }

    /// Files the loader refuses fail the schema too.
    #[test]
    fn the_schema_and_the_loader_refuse_the_same_bad_files() {
        let (schemas, index) = settings_schema();
        let cloud = json!({ "model": { "id": "arc-sage-1.0" } });
        let routing = |router: &str| {
            json!({
                "router": router,
                "decisions_model": { "id": "arc-swift-1.0" },
                "policy": { "preset": "arc-policy.prefer-local.v1" }
            })
        };
        let local = |routing: Value| json!({ "routing": routing });
        let with = |mut value: Value, pointer: &str, field: Value| {
            *value.pointer_mut(pointer).expect(pointer) = field;
            value
        };
        let valid_cloud = json!({ "mode": "cloud", "cloud": cloud });
        let valid_local = json!({ "mode": "local", "local": local(routing("cloud")) });
        let endpoint = json!({
            "mode": "local",
            "local": {
                "inference": {
                    "endpoints": {
                        "gpu-box": {
                            "base_url": "https://gpu-box.example.com:8443",
                            "protocols": ["openai_chat"]
                        }
                    }
                },
                "routing": routing("cloud")
            }
        });
        let bad = [
            ("no mode", json!({ "cloud": cloud })),
            (
                "an unknown mode",
                with(valid_cloud.clone(), "/mode", json!("hybrid")),
            ),
            ("an unknown top-level key", {
                let mut value = valid_cloud.clone();
                value["extra"] = json!(true);
                value
            }),
            ("a top-level routing", {
                let mut value = valid_cloud.clone();
                value["routing"] = routing("cloud");
                value
            }),
            ("cloud mode without cloud", json!({ "mode": "cloud" })),
            (
                "local mode without local",
                json!({ "mode": "local", "cloud": cloud }),
            ),
            (
                "local without routing",
                json!({ "mode": "local", "local": {} }),
            ),
            ("cloud mode with a local.routing without router", {
                let mut value = valid_local.clone();
                value["mode"] = json!("cloud");
                value["cloud"] = cloud.clone();
                value["local"]["routing"]
                    .as_object_mut()
                    .expect("routing")
                    .remove("router");
                value
            }),
            (
                "an empty cloud id",
                with(valid_cloud.clone(), "/cloud/model/id", json!("")),
            ),
            (
                "a v_value in cloud",
                with(
                    valid_cloud.clone(),
                    "/cloud/model",
                    json!({ "id": "arc-sage-1.0", "v_value": 1 }),
                ),
            ),
            (
                "an unknown key in a cloud model",
                with(
                    valid_cloud.clone(),
                    "/cloud/model",
                    json!({ "id": "arc-sage-1.0", "name": "Sage" }),
                ),
            ),
            (
                "router hosted",
                with(
                    valid_local.clone(),
                    "/local/routing/router",
                    json!("hosted"),
                ),
            ),
            ("no router", {
                let mut value = valid_local.clone();
                value["local"]["routing"]
                    .as_object_mut()
                    .expect("routing")
                    .remove("router");
                value
            }),
            (
                "a decisions_model string",
                with(
                    valid_local.clone(),
                    "/local/routing/decisions_model",
                    json!("arc-swift-1.0"),
                ),
            ),
            (
                "a v_value with router local",
                with(
                    json!({ "mode": "local", "local": local(routing("local")) }),
                    "/local/routing/decisions_model",
                    json!({ "id": "arc-swift-1.0", "v_value": 1 }),
                ),
            ),
            (
                "a negative v_value",
                with(
                    valid_local.clone(),
                    "/local/routing/decisions_model",
                    json!({ "id": "arc-swift-1.0", "v_value": -1 }),
                ),
            ),
            (
                "a bad protocol",
                with(
                    endpoint.clone(),
                    "/local/inference/endpoints/gpu-box/protocols",
                    json!(["grpc"]),
                ),
            ),
            (
                "no protocols",
                with(
                    endpoint.clone(),
                    "/local/inference/endpoints/gpu-box/protocols",
                    json!([]),
                ),
            ),
            ("an unknown update channel", {
                let mut value = valid_cloud.clone();
                value["update"] = json!({ "channel": "nightly" });
                value
            }),
        ];
        let home = temp_home();
        for (good, file) in [
            ("cloud", &valid_cloud),
            ("local", &valid_local),
            ("endpoint", &endpoint),
        ] {
            assert!(schemas.validate(file, index).is_ok(), "{good}");
            let path = write_file(&home, "good.json", file);
            load(&home, opts(run_file(&path))).expect(good);
        }
        for (why, file) in &bad {
            assert!(
                schemas.validate(file, index).is_err(),
                "the schema passes {why}"
            );
            let path = write_file(&home, "bad.json", file);
            assert!(
                load(&home, opts(run_file(&path))).is_err(),
                "the loader takes {why}"
            );
        }
    }

    /// The settings files in the docs load, and the three of the spec are
    /// the three setups: cloud mode (the default file), local mode with
    /// cloud ARC, and local mode with local ARC.
    #[test]
    fn the_examples_in_the_docs_load() {
        let examples = doc_examples();
        let home = temp_home();
        for (name, doc) in &examples {
            let file = write_file(&home, "example.json", doc);
            let settings = load(&home, opts(run_file(&file))).expect(name);
            assert_eq!(settings.local, doc["mode"] == "local", "{name}");
            if doc.get("cloud").is_some() {
                load(
                    &home,
                    opts(Command::Run {
                        config: Some(&file),
                        mode: Some(Mode::Cloud),
                    }),
                )
                .expect(name);
            }
            if doc.get("local").is_some() {
                load(&home, opts(start_file(&file))).expect(name);
            }
        }
        let spec: Vec<&Value> = examples
            .iter()
            .filter(|(name, _)| name.starts_with("spec.md:"))
            .map(|(_, doc)| doc)
            .collect();
        assert!(spec.len() >= 3, "{examples:?}");
        assert_eq!(
            spec[0],
            &parse_json(DEFAULT_FILE)
                .ok()
                .expect("the default file is JSON"),
            "example 1 is the default file"
        );
        assert_eq!(spec[1]["local"]["routing"]["router"], "cloud");
        assert_eq!(spec[2]["local"]["routing"]["router"], "local");
        let mut one_line = spec[1].clone();
        one_line["local"]["routing"]["router"] = json!("local");
        assert_eq!(&one_line, spec[2], "examples 2 and 3 differ in `router`");
    }

    /// The config profiles `scripts/acceptance.sh` runs load, each in the
    /// mode, router and decisions model its name says.
    #[test]
    fn acceptance_profiles_load() {
        let home = temp_home();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/acceptance/profiles");
        let profile = |name: &str| {
            load(&home, opts(run_file(&dir.join(format!("{name}.json"))))).expect(name)
        };
        assert!(!profile("cloud").local);
        for (name, router, decisions_model) in [
            ("local-cloud-arc-swift", Router::Cloud, "arc-swift-1.0"),
            ("local-cloud-arc-sage", Router::Cloud, "arc-sage-1.0"),
            ("local-router-local", Router::Local, "arc-swift-1.0"),
        ] {
            let settings = profile(name);
            assert!(settings.local, "{name}");
            let routing = settings.routing.expect(name);
            assert_eq!(routing.router, router, "{name}");
            assert_eq!(routing.decisions_model.id, decisions_model, "{name}");
        }
    }

    #[test]
    fn config_file_replaces_the_default_file() {
        let home = temp_home();
        write_default(&home, &example());
        let file = write_file(
            &home,
            "other.json",
            &json!({"mode": "cloud", "cloud": {"model": {"id": "m"}}}),
        );
        let settings = load(&home, opts(run_file(&file))).expect("load");
        assert!(settings.endpoints.is_empty());
        assert!(settings.environments.is_empty());
        assert_eq!(settings.cloud, Some(cloud("m", "m")));
    }

    #[test]
    fn invalid_json_is_a_load_error() {
        let home = temp_home();
        for text in ["", "{", "{\"mode\": }"] {
            write_default_text(&home, text);
            assert!(
                matches!(load(&home, opts(run(None))), Err(ConfigError::Parse { .. })),
                "{text:?}"
            );
        }
    }

    #[test]
    fn duplicate_keys_are_load_errors_naming_the_key() {
        let cases = [
            (r#"{"mode": "cloud", "mode": "local"}"#, "mode"),
            (
                r#"{"update": {"channel": "dev", "channel": "main"}}"#,
                "update.channel",
            ),
            (
                r#"{"cloud": {"model": {"id": "a", "id": "b"}}}"#,
                "cloud.model.id",
            ),
            (
                r#"{"local": {"inference": {"endpoints": {"box": {"base_url": "http://a", "base_url": "http://b"}}}}}"#,
                "local.inference.endpoints.box.base_url",
            ),
            (
                r#"{"local": {"inference": {"models": {"z-ai/glm-5.3-flash": {}, "z-ai/glm-5.3-flash": {}}}}}"#,
                "local.inference.models.\"z-ai/glm-5.3-flash\"",
            ),
            (
                r#"{"local": {"routing": {"policy": {"overrides": {"selection": {"min_p_pass": 0.5, "min_p_pass": 0.6}}}}}}"#,
                "local.routing.policy.overrides.selection.min_p_pass",
            ),
            (
                r#"{"local": {"routing": {"policy": {"overrides": {"rules": [{}, {"a": 1, "a": 2}]}}}}}"#,
                "local.routing.policy.overrides.rules[1].a",
            ),
        ];
        let home = temp_home();
        for (text, key) in cases {
            write_default_text(&home, text);
            assert_eq!(
                key_error(load(&home, opts(run(None)))),
                (key.to_owned(), "appears more than once".to_owned()),
                "{text}"
            );
        }
    }

    #[test]
    fn json_parses_to_the_same_values_as_serde_json() {
        let text = r#"{"a": [1, -2, 3.5, 1e3, true, false, null, "sé", [], {}],
            "b": {"c": {"d": 18446744073709551615}}, "e": ""}"#;
        let expected: Value = serde_json::from_str(text).expect("serde_json parses it");
        assert!(matches!(parse_json(text), Ok(value) if value == expected));
        let doc = example();
        assert!(matches!(parse_json(&doc.to_string()), Ok(value) if value == doc));
        assert!(matches!(parse_json("{} x"), Err(JsonFailure::Syntax(_))));
    }

    #[test]
    fn top_level_must_be_an_object() {
        for doc in [json!([]), json!("x"), json!(null)] {
            assert!(
                matches!(load_doc(&doc), Err(ConfigError::Parse { .. })),
                "{doc}"
            );
        }
    }

    #[test]
    fn unknown_keys_are_load_errors_at_every_level() {
        let glm = |name: &str| format!("local.inference.models.\"z-ai/glm-5.3-flash\".{name}");
        let cases: Vec<(String, String)> = vec![
            ("extra".to_owned(), "extra".to_owned()),
            // The earlier shape's top-level sections, in a file with `mode`.
            ("endpoints".to_owned(), "endpoints".to_owned()),
            ("models".to_owned(), "models".to_owned()),
            ("routing".to_owned(), "routing".to_owned()),
            ("hosted".to_owned(), "hosted".to_owned()),
            ("cloud v_value".to_owned(), "cloud.v_value".to_owned()),
            ("cloud models".to_owned(), "cloud.models".to_owned()),
            // A cloud model has only `id`.
            (
                "cloud model v_value".to_owned(),
                "cloud.model.v_value".to_owned(),
            ),
            (
                "cloud smol_model v_value".to_owned(),
                "cloud.smol_model.v_value".to_owned(),
            ),
            ("cloud model name".to_owned(), "cloud.model.name".to_owned()),
            ("local endpoints".to_owned(), "local.endpoints".to_owned()),
            (
                "local inference routing".to_owned(),
                "local.inference.routing".to_owned(),
            ),
            (
                format!("{EP} gpu-box protocol"),
                "local.inference.endpoints.gpu-box.protocol".to_owned(),
            ),
            (format!("{MD} {GLM} n_ctx"), glm("n_ctx")),
            (
                format!("{MD} {GLM} price input_per_mtok"),
                glm("price.input_per_mtok"),
            ),
            (
                format!("{RT} local_price"),
                "local.routing.local_price".to_owned(),
            ),
            (
                format!("{RT} decisions_model name"),
                "local.routing.decisions_model.name".to_owned(),
            ),
            (
                format!("{RT} policy name"),
                "local.routing.policy.name".to_owned(),
            ),
            (
                "environments dev router_url".to_owned(),
                "environments.dev.router_url".to_owned(),
            ),
            ("update base_url".to_owned(), "update.base_url".to_owned()),
        ];
        for (path, key) in cases {
            let mut doc = example();
            set(&mut doc, &path, json!("x"));
            for command in [run(None), run(Some(Mode::Cloud)), Command::Other] {
                assert_eq!(
                    key_error(load_doc_for(&doc, command)),
                    (key.clone(), "is not a known key".to_owned()),
                    "{path} {command:?}"
                );
            }
        }
    }

    #[test]
    fn policy_overrides_are_passed_through_unchecked() {
        let mut doc = example();
        let overrides = json!({ "anything": { "ARC": ["checks", 1] }, "x": null });
        set(
            &mut doc,
            &format!("{RT} policy overrides"),
            overrides.clone(),
        );
        let settings = load_doc(&doc).expect("load");
        assert_eq!(
            settings.routing.expect("routing").policy.overrides,
            overrides.as_object().cloned()
        );
    }

    #[test]
    fn wrong_values_are_load_errors_naming_the_key() {
        let glm = |name: &str| format!("local.inference.models.\"z-ai/glm-5.3-flash\".{name}");
        let gpu = |name: &str| format!("local.inference.endpoints.gpu-box.{name}");
        let routing = |name: &str| format!("local.routing.{name}");
        let path = |prefix: &str, name: &str| format!("{prefix} {name}");
        let cases: Vec<(String, Value, String)> = vec![
            (
                "cloud".to_owned(),
                json!("arc-sage-1.0"),
                "cloud".to_owned(),
            ),
            (
                "cloud model".to_owned(),
                json!("arc-sage-1.0"),
                "cloud.model".to_owned(),
            ),
            (
                "cloud model id".to_owned(),
                json!(""),
                "cloud.model.id".to_owned(),
            ),
            (
                "cloud model id".to_owned(),
                json!(1),
                "cloud.model.id".to_owned(),
            ),
            (
                "cloud smol_model".to_owned(),
                json!(null),
                "cloud.smol_model".to_owned(),
            ),
            ("local".to_owned(), json!([]), "local".to_owned()),
            (
                "local inference".to_owned(),
                json!("x"),
                "local.inference".to_owned(),
            ),
            (
                EP.to_owned(),
                json!([]),
                "local.inference.endpoints".to_owned(),
            ),
            (
                path(EP, "gpu-box"),
                json!("x"),
                "local.inference.endpoints.gpu-box".to_owned(),
            ),
            (
                path(EP, ""),
                json!({}),
                "local.inference.endpoints.\"\"".to_owned(),
            ),
            (path(EP, "gpu-box base_url"), json!(5), gpu("base_url")),
            (path(EP, "gpu-box base_url"), json!(""), gpu("base_url")),
            (
                path(EP, "gpu-box base_url"),
                json!("gpu-box:8443"),
                gpu("base_url"),
            ),
            (path(EP, "gpu-box base_url"), json!("/v1"), gpu("base_url")),
            (
                path(EP, "gpu-box base_url"),
                json!("ftp://gpu-box"),
                gpu("base_url"),
            ),
            (
                path(EP, "gpu-box protocols"),
                json!("openai_chat"),
                gpu("protocols"),
            ),
            (path(EP, "gpu-box protocols"), json!([]), gpu("protocols")),
            (
                path(EP, "gpu-box protocols"),
                json!(["openai"]),
                gpu("protocols"),
            ),
            (path(EP, "gpu-box protocols"), json!([1]), gpu("protocols")),
            (
                path(EP, "gpu-box protocols"),
                json!(["openai_chat", "openai_chat"]),
                gpu("protocols"),
            ),
            (
                path(EP, "gpu-box api_key_env"),
                json!(""),
                gpu("api_key_env"),
            ),
            (
                path(EP, "gpu-box api_key_env"),
                json!(1),
                gpu("api_key_env"),
            ),
            (
                MD.to_owned(),
                json!("x"),
                "local.inference.models".to_owned(),
            ),
            (
                path(MD, ""),
                json!({}),
                "local.inference.models.\"\"".to_owned(),
            ),
            (
                path(MD, &format!("{GLM} endpoint")),
                json!(""),
                glm("endpoint"),
            ),
            (path(MD, &format!("{GLM} model")), json!(3), glm("model")),
            (
                path(MD, &format!("{GLM} context_window")),
                json!(0),
                glm("context_window"),
            ),
            (
                path(MD, &format!("{GLM} context_window")),
                json!(-1),
                glm("context_window"),
            ),
            (
                path(MD, &format!("{GLM} context_window")),
                json!(1.5),
                glm("context_window"),
            ),
            (
                path(MD, &format!("{GLM} context_window")),
                json!("1000"),
                glm("context_window"),
            ),
            (
                path(MD, &format!("{GLM} max_output_tokens")),
                json!(0),
                glm("max_output_tokens"),
            ),
            (
                path(MD, &format!("{GLM} tokenizer")),
                json!(""),
                glm("tokenizer"),
            ),
            (path(MD, &format!("{GLM} price")), json!(1), glm("price")),
            (
                path(MD, &format!("{GLM} price input")),
                json!(-0.5),
                glm("price.input"),
            ),
            (
                path(MD, &format!("{GLM} price output")),
                json!("1"),
                glm("price.output"),
            ),
            (
                path(MD, &format!("{GLM} price cache_read")),
                json!(null),
                glm("price.cache_read"),
            ),
            (
                path(MD, &format!("{GLM} price cache_write")),
                json!(-1),
                glm("price.cache_write"),
            ),
            (
                path(MD, GLM),
                json!("x"),
                "local.inference.models.\"z-ai/glm-5.3-flash\"".to_owned(),
            ),
            (RT.to_owned(), json!("local"), "local.routing".to_owned()),
            (path(RT, "router"), json!("hosted"), routing("router")),
            (path(RT, "router"), json!("Cloud"), routing("router")),
            (path(RT, "router"), json!(null), routing("router")),
            (
                path(RT, "decisions_model"),
                json!("arc-swift-1.0"),
                routing("decisions_model"),
            ),
            (
                path(RT, "decisions_model id"),
                json!(""),
                routing("decisions_model.id"),
            ),
            (
                path(RT, "decisions_model v_value"),
                json!(-0.5),
                routing("decisions_model.v_value"),
            ),
            (
                path(RT, "decisions_model v_value"),
                json!("2"),
                routing("decisions_model.v_value"),
            ),
            (
                path(RT, "classifier_model"),
                json!(1),
                routing("classifier_model"),
            ),
            (path(RT, "policy"), json!("prefer-local"), routing("policy")),
            (
                path(RT, "policy preset"),
                json!(""),
                routing("policy.preset"),
            ),
            (
                path(RT, "policy overrides"),
                json!([]),
                routing("policy.overrides"),
            ),
            (
                path(RT, "policy overrides"),
                json!(null),
                routing("policy.overrides"),
            ),
            (
                "environments".to_owned(),
                json!([]),
                "environments".to_owned(),
            ),
            (
                "environments ".to_owned(),
                json!({}),
                "environments.\"\"".to_owned(),
            ),
            (
                "environments dev".to_owned(),
                json!("x"),
                "environments.dev".to_owned(),
            ),
            (
                "environments dev base_url".to_owned(),
                json!("dev"),
                "environments.dev.base_url".to_owned(),
            ),
            (
                "environments dev base_url".to_owned(),
                json!(""),
                "environments.dev.base_url".to_owned(),
            ),
            (
                "environments dev login_url".to_owned(),
                json!("ftp://platform.dev.example.com/cli-auth"),
                "environments.dev.login_url".to_owned(),
            ),
            (
                "environments dev login_url".to_owned(),
                json!(1),
                "environments.dev.login_url".to_owned(),
            ),
            ("update".to_owned(), json!("dev"), "update".to_owned()),
            (
                "update channel".to_owned(),
                json!("beta"),
                "update.channel".to_owned(),
            ),
            (
                "update channel".to_owned(),
                json!("Prod"),
                "update.channel".to_owned(),
            ),
            (
                "update channel".to_owned(),
                json!(""),
                "update.channel".to_owned(),
            ),
            (
                "update channel".to_owned(),
                json!(1),
                "update.channel".to_owned(),
            ),
        ];
        for (path, value, key) in cases {
            let mut doc = example();
            set(&mut doc, &path, value.clone());
            // The whole file is checked, whatever the command uses.
            for command in [run(None), run(Some(Mode::Cloud)), Command::Other] {
                assert_eq!(
                    key_error(load_doc_for(&doc, command)).0,
                    key,
                    "{path} = {value}, {command:?}"
                );
            }
        }
    }

    #[test]
    fn malformed_values_say_what_is_wrong() {
        let cases: Vec<(String, Value, &str, &str)> = vec![
            (
                format!("{EP} "),
                json!({}),
                "local.inference.endpoints.\"\"",
                "has an empty id",
            ),
            (
                format!("{MD} "),
                json!({}),
                "local.inference.models.\"\"",
                "has an empty id",
            ),
            (
                "environments ".to_owned(),
                json!({}),
                "environments.\"\"",
                "has an empty id",
            ),
            (
                "cloud model".to_owned(),
                json!("arc-sage-1.0"),
                "cloud.model",
                "must be an object",
            ),
            (
                format!("{RT} decisions_model"),
                json!("arc-swift-1.0"),
                "local.routing.decisions_model",
                "must be an object",
            ),
            (
                format!("{RT} decisions_model v_value"),
                json!(-1),
                "local.routing.decisions_model.v_value",
                "must be a number >= 0",
            ),
            (
                format!("{RT} router"),
                json!("hosted"),
                "local.routing.router",
                "must be one of `cloud`, `local`",
            ),
            (
                format!("{EP} gpu-box base_url"),
                json!("gpu-box:8443"),
                "local.inference.endpoints.gpu-box.base_url",
                "must be an http or https URL",
            ),
            (
                "environments dev login_url".to_owned(),
                json!("/cli-auth"),
                "environments.dev.login_url",
                "must be an http or https URL",
            ),
            (
                "environments dev base_url".to_owned(),
                json!("http://api.dev.example.com"),
                "environments.dev.base_url",
                "must be https, or http on a loopback host, not http://api.dev.example.com/",
            ),
            (
                "environments dev base_url".to_owned(),
                json!("https://user:secret@api.dev.example.com"),
                "environments.dev.base_url",
                "must not contain user:password@, not https://api.dev.example.com/",
            ),
            (
                "environments dev base_url".to_owned(),
                json!("http://user@127.0.0.1:9"),
                "environments.dev.base_url",
                "must not contain user:password@, not http://127.0.0.1:9/",
            ),
            (
                format!("{EP} gpu-box protocols"),
                json!(["openai_chat", "anthropic_messages", "openai_chat"]),
                "local.inference.endpoints.gpu-box.protocols",
                "lists `openai_chat` more than once",
            ),
            (
                format!("{EP} gpu-box api_key_env"),
                json!(""),
                "local.inference.endpoints.gpu-box.api_key_env",
                "must not be empty",
            ),
            (
                format!("{MD} {GLM} context_window"),
                json!(0),
                "local.inference.models.\"z-ai/glm-5.3-flash\".context_window",
                "must be an integer > 0",
            ),
            (
                format!("{MD} {GLM} price input"),
                json!(-0.5),
                "local.inference.models.\"z-ai/glm-5.3-flash\".price.input",
                "must be a number >= 0",
            ),
        ];
        for (path, value, key, problem) in cases {
            let mut doc = example();
            set(&mut doc, &path, value.clone());
            assert_eq!(
                key_error(load_doc(&doc)),
                (key.to_owned(), problem.to_owned()),
                "{path} = {value}"
            );
        }
    }

    #[test]
    fn an_environment_router_may_be_http_only_on_loopback() {
        for base_url in [
            "http://127.0.0.1:9",
            "http://localhost:8080",
            "http://[::1]:9",
        ] {
            let mut doc = example();
            set(&mut doc, "environments dev base_url", json!(base_url));
            let settings = load_doc(&doc).unwrap();
            assert_eq!(
                settings.environments["dev"].base_url,
                Url::parse(base_url).unwrap()
            );
        }
    }

    #[test]
    fn missing_required_keys_are_load_errors() {
        let glm = |name: &str| format!("local.inference.models.\"z-ai/glm-5.3-flash\".{name}");
        let cases: Vec<(String, String)> = vec![
            (
                format!("{EP} gpu-box base_url"),
                "local.inference.endpoints.gpu-box.base_url".to_owned(),
            ),
            (
                format!("{EP} gpu-box protocols"),
                "local.inference.endpoints.gpu-box.protocols".to_owned(),
            ),
            (format!("{MD} {GLM} endpoint"), glm("endpoint")),
            (format!("{MD} {GLM} model"), glm("model")),
            (format!("{MD} {GLM} price input"), glm("price.input")),
            (format!("{MD} {GLM} price output"), glm("price.output")),
            (
                format!("{MD} {GLM} price cache_read"),
                glm("price.cache_read"),
            ),
            (
                format!("{MD} {GLM} price cache_write"),
                glm("price.cache_write"),
            ),
            (format!("{RT} router"), "local.routing.router".to_owned()),
            (
                format!("{RT} decisions_model"),
                "local.routing.decisions_model".to_owned(),
            ),
            (
                format!("{RT} decisions_model id"),
                "local.routing.decisions_model.id".to_owned(),
            ),
            (
                format!("{RT} policy"),
                "local.routing.policy.preset".to_owned(),
            ),
            (
                format!("{RT} policy preset"),
                "local.routing.policy.preset".to_owned(),
            ),
            (
                "environments dev base_url".to_owned(),
                "environments.dev.base_url".to_owned(),
            ),
            (
                "environments dev login_url".to_owned(),
                "environments.dev.login_url".to_owned(),
            ),
        ];
        for (path, key) in cases {
            let mut doc = example();
            remove(&mut doc, &path);
            assert_eq!(
                key_error(load_doc(&doc)),
                (key, "is required".to_owned()),
                "{path}"
            );
        }
        for (path, key) in [
            ("cloud model", "cloud.model"),
            ("cloud model id", "cloud.model.id"),
            ("cloud smol_model id", "cloud.smol_model.id"),
        ] {
            let mut doc = example();
            remove(&mut doc, path);
            assert_eq!(
                key_error(load_doc_for(&doc, run(Some(Mode::Cloud)))),
                (key.to_owned(), "is required".to_owned()),
                "{path}"
            );
        }
    }

    /// A section the file has needs its required keys for every command,
    /// including one that uses neither section; which sections must be
    /// there depends on the command.
    #[test]
    fn required_keys_are_checked_in_every_section_the_file_has() {
        for (path, key) in [
            (
                format!("{RT} decisions_model"),
                "local.routing.decisions_model",
            ),
            (
                format!("{RT} decisions_model id"),
                "local.routing.decisions_model.id",
            ),
            (format!("{RT} router"), "local.routing.router"),
            (format!("{RT} policy"), "local.routing.policy.preset"),
            ("cloud model".to_owned(), "cloud.model"),
            ("cloud model id".to_owned(), "cloud.model.id"),
            ("cloud smol_model id".to_owned(), "cloud.smol_model.id"),
        ] {
            let mut doc = example();
            remove(&mut doc, &path);
            for command in [
                run(Some(Mode::Cloud)),
                run(Some(Mode::Local)),
                START,
                Command::Other,
            ] {
                assert_eq!(
                    key_error(load_doc_for(&doc, command)),
                    (key.to_owned(), "is required".to_owned()),
                    "{path} {command:?}"
                );
            }
        }
        // Without the section, only a command that uses it fails.
        let mut doc = example();
        remove(&mut doc, "cloud");
        load_doc_for(&doc, Command::Other).expect("no `cloud`");
        load_doc_for(&doc, run(Some(Mode::Local))).expect("local mode, no `cloud`");
    }

    #[test]
    fn the_section_of_the_mode_is_required() {
        let mut doc = example();
        remove(&mut doc, "cloud");
        assert_eq!(
            key_error(load_doc_for(&doc, run(Some(Mode::Cloud)))),
            ("cloud".to_owned(), "is required in cloud mode".to_owned())
        );
        load_doc(&doc).expect("local mode doesn't use `cloud`");
        let mut doc = example();
        remove(&mut doc, "local");
        let no_local = format!("is required in local mode. See {LOCAL_MODE_DOCS}");
        for command in [run(None), START] {
            assert_eq!(
                key_error(load_doc_for(&doc, command)),
                ("local".to_owned(), no_local.clone())
            );
        }
        load_doc_for(&doc, run(Some(Mode::Cloud))).expect("cloud mode doesn't use `local`");
        let mut doc = example();
        remove(&mut doc, RT);
        let home = temp_home();
        write_default(&home, &doc);
        let file = write_file(&home, "local.json", &doc);
        for command in [run(None), START, start_file(&file)] {
            assert_eq!(
                key_error(load(&home, opts(command))),
                (
                    "local.routing".to_owned(),
                    "is required in local mode".to_owned()
                )
            );
        }
        // The default file of a new user is in cloud mode.
        let empty = temp_home();
        assert_eq!(
            key_error(load(&empty, opts(run(Some(Mode::Local))))),
            ("local".to_owned(), no_local)
        );
    }

    #[test]
    fn decisions_local_starts_without_a_routing_section() {
        let with = |router| LoadOptions {
            router: Some(router),
            ..opts(START)
        };
        // A new user's default file: the local decisions defaults.
        let empty = temp_home();
        let routing = load(&empty, with(Router::Local))
            .expect("no file")
            .routing
            .expect("routing");
        assert_eq!(routing.router, Router::Local);
        assert_eq!(routing.decisions_model.id, DEFAULT_DECISIONS_MODEL);
        assert_eq!(routing.decisions_model.v_value, None);
        assert_eq!(routing.policy.preset, DEFAULT_POLICY_PRESET);
        assert_eq!(routing.classifier_model, None);
        assert!(routing.defaults);
        // A local-mode run on a process started from them loads the same
        // routing.
        let run_on_it = LoadOptions {
            routing_defaults: Some(Router::Local),
            ..opts(run(Some(Mode::Local)))
        };
        assert_eq!(
            load(&empty, run_on_it).expect("run").routing,
            Some(routing.clone())
        );
        // Claude Code's snippet: this routing, with `classifier_model`.
        let snippet: Value =
            serde_json::from_str(&routing.with_classifier_model()).expect("snippet is JSON");
        assert_eq!(
            snippet,
            json!({"mode": "local", "local": {"routing": {"router": "local",
                "decisions_model": {"id": DEFAULT_DECISIONS_MODEL},
                "policy": {"preset": DEFAULT_POLICY_PRESET}, "classifier_model": "<model id>"}}})
        );
        // `cloud` has the same defaults, with its own router.
        let cloud = load(&empty, with(Router::Cloud))
            .expect("no local.routing")
            .routing
            .expect("routing");
        assert_eq!(
            cloud,
            Routing {
                router: Router::Cloud,
                ..routing.clone()
            }
        );
        // A run on a process started from the cloud defaults loads them.
        let run_on_cloud = LoadOptions {
            routing_defaults: Some(Router::Cloud),
            ..opts(run(Some(Mode::Local)))
        };
        assert_eq!(
            load(&empty, run_on_cloud).expect("run").routing,
            Some(cloud.clone())
        );
        // So does a `--config` file in local mode with no `local.routing`.
        let file = empty.join("x.json");
        fs::write(&file, r#"{"mode": "local", "local": {}}"#).expect("write");
        let run_file_on_cloud = LoadOptions {
            routing_defaults: Some(Router::Cloud),
            ..opts(run_file(&file))
        };
        assert_eq!(
            load(&empty, run_file_on_cloud).expect("run").routing,
            Some(cloud)
        );
        // Over a file's `local.routing`, only `router` changes.
        let home = temp_home();
        write_default(&home, &example());
        let file = load(&home, opts(START))
            .expect("file")
            .routing
            .expect("routing");
        let flagged = load(&home, with(Router::Local))
            .expect("flagged")
            .routing
            .expect("routing");
        assert_eq!(flagged.router, Router::Local);
        assert_eq!(flagged.decisions_model, file.decisions_model);
        assert_eq!(flagged.policy, file.policy);
        assert_eq!(flagged.classifier_model, file.classifier_model);
        assert!(!flagged.defaults);
        // A file's `local.routing` wins over the defaults a run would use.
        assert_eq!(load(&home, run_on_it).expect("run").routing, Some(file));

        // Commands that use no `local.routing` don't read its overrides, so
        // a process started this way can be checked and stopped.
        load_env(
            &empty,
            opts(Command::Other),
            &[("RAYLINE_LOCAL_ROUTING_CLASSIFIER_MODEL", "")],
        )
        .expect("status and stop load");
        // Env overrides refine the defaults; the flag still wins over
        // `RAYLINE_LOCAL_ROUTING_ROUTER`.
        let routing = load_env(
            &empty,
            with(Router::Local),
            &[
                ("RAYLINE_LOCAL_ROUTING_ROUTER", "cloud"),
                ("RAYLINE_LOCAL_ROUTING_CLASSIFIER_MODEL", "c"),
            ],
        )
        .expect("env")
        .routing
        .expect("routing");
        assert_eq!(routing.router, Router::Local);
        assert_eq!(routing.classifier_model.as_deref(), Some("c"));
        assert_eq!(routing.decisions_model.id, DEFAULT_DECISIONS_MODEL);
    }

    #[test]
    fn the_classifier_snippet_keeps_a_v_value() {
        let mut doc = example();
        set(&mut doc, &format!("{RT} decisions_model v_value"), json!(2));
        let routing = load_doc(&doc).expect("load").routing.expect("routing");
        let snippet: Value =
            serde_json::from_str(&routing.with_classifier_model()).expect("snippet is JSON");
        assert_eq!(
            snippet["local"]["routing"]["decisions_model"],
            json!({"id": "arc-swift-1.0", "v_value": 2.0})
        );
        assert_eq!(snippet["local"]["routing"]["router"], "cloud");
    }

    #[test]
    fn environment_names_follow_the_env_name_rule() {
        for name in ["dev.eu", "dev eu", "dév"] {
            let mut doc = example();
            doc["environments"][name] = json!({
                "base_url": "https://dev.example.test",
                "login_url": "https://dev.example.test/cli-auth"
            });
            assert_eq!(
                key_error(load_doc(&doc)),
                (
                    child("environments", name),
                    "is not a valid environment name (use letters, digits, `-` and `_`)".to_owned()
                )
            );
        }
    }

    #[test]
    fn model_endpoint_must_be_in_endpoints() {
        let mut doc = example();
        set(
            &mut doc,
            &format!("{MD} {GLM} endpoint"),
            json!("lm-studio"),
        );
        for command in [run(None), run(Some(Mode::Cloud)), Command::Other] {
            assert_eq!(
                key_error(load_doc_for(&doc, command)),
                (
                    "local.inference.models.\"z-ai/glm-5.3-flash\".endpoint".to_owned(),
                    "names `lm-studio`, which is not in `local.inference.endpoints`".to_owned()
                )
            );
        }
    }

    #[test]
    fn local_mode_endpoints_must_list_the_harness_protocol() {
        let home = temp_home();
        write_default(&home, &example());
        let local_file = write_file(&home, "local.json", &example());
        let load_for = |command, protocol| {
            load(
                &home,
                LoadOptions {
                    harness_protocol: protocol,
                    ..opts(command)
                },
            )
        };

        for command in [run(None), run_file(&local_file)] {
            assert_eq!(
                key_error(load_for(command, Some(Protocol::OpenaiResponses))),
                (
                    "local.inference.endpoints.ollama.protocols".to_owned(),
                    "does not list `openai_responses`, the protocol of the harness being run"
                        .to_owned()
                )
            );
            load_for(command, Some(Protocol::AnthropicMessages)).expect("both endpoints list it");
        }
        // Cloud mode doesn't use endpoints; `local start` has no harness.
        load_for(run(Some(Mode::Cloud)), Some(Protocol::OpenaiResponses)).expect("cloud");
        load_for(START, None).expect("local start");
        load_for(start_file(&local_file), None).expect("local start --config");
    }

    #[test]
    fn env_selects_an_environments_entry() {
        let home = temp_home();
        write_default(&home, &example());
        let with_env = |env| {
            load(
                &home,
                LoadOptions {
                    env: Some(env),
                    ..opts(run(None))
                },
            )
        };
        assert_eq!(with_env("dev").expect("dev").env.as_deref(), Some("dev"));
        for name in ["staging", "prod"] {
            assert_eq!(
                key_error(with_env(name)),
                (
                    format!("environments.{name}"),
                    format!("is not defined, and `--env {name}` selects it")
                )
            );
        }
    }

    // `local.routing`.

    #[test]
    fn routing_names_its_router_and_release() {
        let routing = |extra: Value| {
            let mut doc = example();
            for (key, value) in extra.as_object().expect("object") {
                doc["local"]["routing"][key] = value.clone();
            }
            load_doc(&doc).map(|settings| settings.routing.expect("routing"))
        };
        let plain = routing(json!({})).expect("load");
        assert_eq!(plain.router, Router::Cloud);
        assert_eq!(plain.arc_release, None);
        let local = routing(json!({"router": "local"})).expect("load");
        assert_eq!(local.router, Router::Local);
        for model in ["arc-swift-1.0", "arc-sage-1.0"] {
            routing(json!({"router": "local", "decisions_model": {"id": model}})).expect(model);
        }
        let (key, problem) = key_error(routing(
            json!({"router": "local", "decisions_model": {"id": "arc-1.0"}}),
        ));
        assert_eq!(key, "local.routing.decisions_model.id");
        assert!(
            problem.contains("`arc-1.0`") && problem.contains("`local.routing.router` to `cloud`"),
            "{problem}"
        );
        routing(json!({"decisions_model": {"id": "arc-1.0"}})).expect("cloud decides arc-1.0");

        let release = json!({"repo": "org/arc", "revision": "a".repeat(40),
                             "manifest_sha256": "b".repeat(64)});
        let pinned = routing(json!({"arc_release": release})).expect("load");
        assert_eq!(
            pinned.arc_release,
            Some(ArcRelease {
                repo: "org/arc".to_owned(),
                revision: "a".repeat(40),
                manifest_sha256: "b".repeat(64),
                dir: None,
            })
        );
        let mut in_dir = release.clone();
        in_dir["dir"] = json!("releases/runtime");
        let in_dir = routing(json!({"arc_release": in_dir})).expect("load");
        assert_eq!(
            in_dir.arc_release.and_then(|r| r.dir).as_deref(),
            Some("releases/runtime")
        );
        let with = |key: &str, value: Value| {
            let mut release = release.clone();
            release[key] = value;
            key_error(routing(json!({"arc_release": release})))
        };
        for repo in ["arc", "org/", "/arc", "org/../x", "org/a b"] {
            assert_eq!(
                with("repo", json!(repo)).0,
                "local.routing.arc_release.repo"
            );
        }
        assert_eq!(
            with("revision", json!("main")),
            (
                "local.routing.arc_release.revision".to_owned(),
                "must be 40 lowercase hex digits".to_owned()
            )
        );
        assert_eq!(
            with("manifest_sha256", json!("B".repeat(64))).0,
            "local.routing.arc_release.manifest_sha256"
        );
        for dir in ["", "/abs", "a/../b", "a//b", "a b"] {
            assert_eq!(with("dir", json!(dir)).0, "local.routing.arc_release.dir");
        }
        assert_eq!(
            with("path", json!("runtime")),
            (
                "local.routing.arc_release.path".to_owned(),
                "is not a known key".to_owned()
            )
        );
        let mut missing = release.clone();
        missing.as_object_mut().expect("object").remove("revision");
        assert_eq!(
            key_error(routing(json!({"arc_release": missing}))),
            (
                "local.routing.arc_release.revision".to_owned(),
                "is required".to_owned()
            )
        );
    }

    #[test]
    fn a_v_value_goes_with_the_cloud_router() {
        let mut doc = example();
        set(
            &mut doc,
            &format!("{RT} decisions_model"),
            json!({"id": "arc-1.0", "v_value": 2}),
        );
        let routing = load_doc(&doc).expect("load").routing.expect("routing");
        assert_eq!(
            routing.decisions_model,
            DecisionsModel {
                id: "arc-1.0".to_owned(),
                v_value: Some(2.0)
            }
        );
        set(&mut doc, &format!("{RT} decisions_model v_value"), json!(0));
        let routing = load_doc(&doc).expect("load").routing.expect("routing");
        assert_eq!(routing.decisions_model.v_value, Some(0.0));
    }

    #[test]
    fn a_v_value_with_the_local_router_is_a_load_error() {
        let refused = (
            "local.routing.decisions_model.v_value".to_owned(),
            "must not be set with `local.routing.router` `local`: local decisions take no \
             `v_value`"
                .to_owned(),
        );
        let mut doc = example();
        set(&mut doc, &format!("{RT} router"), json!("local"));
        set(&mut doc, &format!("{RT} decisions_model v_value"), json!(2));
        // In the file: every command refuses it.
        for command in [run(None), run(Some(Mode::Cloud)), START, Command::Other] {
            assert_eq!(
                key_error(load_doc_for(&doc, command)),
                refused,
                "{command:?}"
            );
        }
        // From the env, or the router from the env or `--router`.
        let home = temp_home();
        let mut cloud_router = doc.clone();
        set(&mut cloud_router, &format!("{RT} router"), json!("cloud"));
        write_default(&home, &cloud_router);
        assert_eq!(
            key_error(load_env(
                &home,
                opts(START),
                &[("RAYLINE_LOCAL_ROUTING_ROUTER", "local")]
            )),
            refused
        );
        let flagged = LoadOptions {
            router: Some(Router::Local),
            ..opts(START)
        };
        assert_eq!(key_error(load(&home, flagged)), refused);
        let mut no_v_value = doc.clone();
        remove(&mut no_v_value, &format!("{RT} decisions_model v_value"));
        write_default(&home, &no_v_value);
        assert_eq!(
            key_error(load_env(
                &home,
                opts(START),
                &[("RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE", "2")]
            )),
            refused
        );
    }

    // Env overrides.

    /// The env var table of section 5.7.
    const OVERRIDES: [EnvVar; 9] = [
        MODE,
        CLOUD_MODEL_ID,
        CLOUD_SMOL_MODEL_ID,
        LOCAL_ROUTING_ROUTER,
        LOCAL_ROUTING_DECISIONS_MODEL_ID,
        LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE,
        LOCAL_ROUTING_CLASSIFIER_MODEL,
        LOCAL_ROUTING_POLICY_PRESET,
        UPDATE_CHANNEL,
    ];

    #[test]
    fn the_override_table_is_section_5_7() {
        assert_eq!(
            OVERRIDES,
            [
                ("RAYLINE_MODE", "mode"),
                ("RAYLINE_CLOUD_MODEL_ID", "cloud.model.id"),
                ("RAYLINE_CLOUD_SMOL_MODEL_ID", "cloud.smol_model.id"),
                ("RAYLINE_LOCAL_ROUTING_ROUTER", "local.routing.router"),
                (
                    "RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_ID",
                    "local.routing.decisions_model.id"
                ),
                (
                    "RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE",
                    "local.routing.decisions_model.v_value"
                ),
                (
                    "RAYLINE_LOCAL_ROUTING_CLASSIFIER_MODEL",
                    "local.routing.classifier_model"
                ),
                (
                    "RAYLINE_LOCAL_ROUTING_POLICY_PRESET",
                    "local.routing.policy.preset"
                ),
                ("RAYLINE_UPDATE_CHANNEL", "update.channel"),
            ]
        );
        // Each var is named from its key's path.
        for (var, key) in OVERRIDES {
            assert_eq!(
                var,
                format!("RAYLINE_{}", key.replace('.', "_").to_uppercase())
            );
        }
        let mut sections = CLOUD_OVERRIDES.to_vec();
        sections.extend(ROUTING_OVERRIDES);
        sections.sort();
        let mut expected: Vec<EnvVar> = OVERRIDES[1..8].to_vec();
        expected.sort();
        assert_eq!(sections, expected);
    }

    #[test]
    fn each_env_override_applies_on_its_own() {
        let home = temp_home();
        write_default(&home, &example());
        let file = write_file(&home, "local.json", &example());
        for config in [None, Some(file.as_path())] {
            let with = |mode, var, value| {
                load_env(&home, opts(Command::Run { config, mode }), &[(var, value)]).expect("load")
            };
            let local = |var, value| with(None, var, value).routing.expect("routing");
            let cloud_with = |var, value| with(Some(Mode::Cloud), var, value).cloud.expect("cloud");

            let routing = local("RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_ID", "d2");
            assert_eq!(routing.decisions_model.id, "d2");
            assert_eq!(
                routing.classifier_model.as_deref(),
                Some("anthropic/claude-haiku-5.5")
            );
            let routing = local("RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE", "0.75");
            assert_eq!(
                routing.decisions_model,
                DecisionsModel {
                    id: "arc-swift-1.0".to_owned(),
                    v_value: Some(0.75)
                }
            );
            let routing = local("RAYLINE_LOCAL_ROUTING_CLASSIFIER_MODEL", "c2");
            assert_eq!(routing.classifier_model.as_deref(), Some("c2"));
            assert_eq!(routing.decisions_model.id, "arc-swift-1.0");
            let routing = local("RAYLINE_LOCAL_ROUTING_POLICY_PRESET", "p2");
            assert_eq!(routing.policy.preset, "p2");
            let routing = local("RAYLINE_LOCAL_ROUTING_ROUTER", "local");
            assert_eq!(routing.router, Router::Local);
            assert_eq!(
                cloud_with("RAYLINE_CLOUD_MODEL_ID", "m2"),
                cloud("m2", "arc-swift-1.0")
            );
            assert_eq!(
                cloud_with("RAYLINE_CLOUD_SMOL_MODEL_ID", "s2"),
                cloud("arc-sage-1.0", "s2")
            );
            assert!(!with(None, "RAYLINE_MODE", "cloud").local);
            for channel in UpdateChannel::ALL {
                assert_eq!(
                    with(None, "RAYLINE_UPDATE_CHANNEL", channel.as_str())
                        .update
                        .channel,
                    channel
                );
            }
        }
    }

    #[test]
    fn env_overrides_replace_file_values() {
        let home = temp_home();
        write_default(&home, &example());
        let settings = load_env(
            &home,
            opts(run(None)),
            &[
                ("RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_ID", "arc-1.0"),
                ("RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE", "2"),
                ("RAYLINE_LOCAL_ROUTING_CLASSIFIER_MODEL", "classifier-x"),
                (
                    "RAYLINE_LOCAL_ROUTING_POLICY_PRESET",
                    "arc-policy.hybrid-default.v1",
                ),
                ("RAYLINE_UPDATE_CHANNEL", "main"),
            ],
        )
        .expect("load");
        let routing = settings.routing.expect("routing");
        assert_eq!(routing.decisions_model.id, "arc-1.0");
        assert_eq!(routing.decisions_model.v_value, Some(2.0));
        assert_eq!(routing.classifier_model.as_deref(), Some("classifier-x"));
        assert_eq!(routing.policy.preset, "arc-policy.hybrid-default.v1");
        assert!(
            routing.policy.overrides.is_some(),
            "overrides stay from the file"
        );
        assert_eq!(settings.update.channel, UpdateChannel::Main);
    }

    /// An env override replaces a value the file has; it doesn't give a
    /// required key the file lacks. It may give an optional one.
    #[test]
    fn env_overrides_do_not_give_missing_required_keys() {
        let all = [
            ("RAYLINE_CLOUD_MODEL_ID", "m"),
            ("RAYLINE_LOCAL_ROUTING_ROUTER", "cloud"),
            ("RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_ID", "arc-swift-1.0"),
            (
                "RAYLINE_LOCAL_ROUTING_POLICY_PRESET",
                "arc-policy.prefer-local.v1",
            ),
        ];
        for (path, key) in [
            (
                format!("{RT} decisions_model"),
                "local.routing.decisions_model",
            ),
            (
                format!("{RT} decisions_model id"),
                "local.routing.decisions_model.id",
            ),
            (format!("{RT} router"), "local.routing.router"),
            (format!("{RT} policy"), "local.routing.policy.preset"),
            ("cloud model".to_owned(), "cloud.model"),
        ] {
            let home = temp_home();
            let mut doc = example();
            remove(&mut doc, &path);
            write_default(&home, &doc);
            for command in [run(Some(Mode::Cloud)), run(Some(Mode::Local)), START] {
                assert_eq!(
                    key_error(load_env(&home, opts(command), &all)),
                    (key.to_owned(), "is required".to_owned()),
                    "{path} {command:?}"
                );
            }
        }
        // A v_value alone gives the decisions model no id either.
        let home = temp_home();
        let mut doc = example();
        remove(&mut doc, &format!("{RT} decisions_model id"));
        write_default(&home, &doc);
        assert_eq!(
            key_error(load_env(
                &home,
                opts(START),
                &[("RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE", "1")],
            )),
            (
                "local.routing.decisions_model.id".to_owned(),
                "is required".to_owned()
            )
        );
        // Optional keys: `cloud.smol_model` and `update.channel`.
        let home = temp_home();
        let mut doc = example();
        remove(&mut doc, "cloud smol_model");
        remove(&mut doc, "update");
        write_default(&home, &doc);
        let settings = load_env(
            &home,
            opts(run(Some(Mode::Cloud))),
            &[
                ("RAYLINE_CLOUD_SMOL_MODEL_ID", "s"),
                ("RAYLINE_UPDATE_CHANNEL", "dev"),
            ],
        )
        .expect("load");
        assert_eq!(settings.cloud.expect("cloud").smol_model.id, "s");
        assert_eq!(settings.update.channel, UpdateChannel::Dev);
    }

    #[test]
    fn invalid_env_overrides_are_load_errors() {
        let home = temp_home();
        write_default(&home, &example());
        let cases = [
            ("RAYLINE_MODE", "", "must not be empty"),
            ("RAYLINE_MODE", "hosted", "must be one of `cloud`, `local`"),
            ("RAYLINE_CLOUD_MODEL_ID", "", "must not be empty"),
            ("RAYLINE_CLOUD_SMOL_MODEL_ID", "", "must not be empty"),
            ("RAYLINE_LOCAL_ROUTING_ROUTER", "", "must not be empty"),
            (
                "RAYLINE_LOCAL_ROUTING_ROUTER",
                "hosted",
                "must be one of `cloud`, `local`",
            ),
            (
                "RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_ID",
                "",
                "must not be empty",
            ),
            (
                "RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE",
                "",
                "must not be empty",
            ),
            (
                "RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE",
                "-1",
                "must be a number >= 0",
            ),
            (
                "RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE",
                "two",
                "must be a number >= 0",
            ),
            (
                "RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE",
                "inf",
                "must be a number >= 0",
            ),
            (
                "RAYLINE_LOCAL_ROUTING_CLASSIFIER_MODEL",
                "",
                "must not be empty",
            ),
            (
                "RAYLINE_LOCAL_ROUTING_POLICY_PRESET",
                "",
                "must not be empty",
            ),
            ("RAYLINE_UPDATE_CHANNEL", "", "must not be empty"),
            (
                "RAYLINE_UPDATE_CHANNEL",
                "beta",
                "must be one of `prod`, `dev`, `main`",
            ),
            (
                "RAYLINE_UPDATE_CHANNEL",
                "PROD",
                "must be one of `prod`, `dev`, `main`",
            ),
        ];
        for (var, value, problem) in cases {
            let mode = if var.starts_with("RAYLINE_CLOUD_") {
                Some(Mode::Cloud)
            } else {
                None
            };
            assert_eq!(
                env_error(load_env(&home, opts(run(mode)), &[(var, value)])),
                (var, problem.to_owned()),
                "{var}={value:?}"
            );
        }
    }

    /// An env value that is not valid UTF-8 on this platform.
    fn not_utf8() -> OsString {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;
            OsString::from_vec(vec![0xff])
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt as _;
            OsString::from_wide(&[0xd800])
        }
    }

    #[test]
    fn non_utf8_env_override_is_a_load_error() {
        let home = temp_home();
        write_default(&home, &example());
        for (var, _) in OVERRIDES {
            let mode = if var.starts_with("RAYLINE_CLOUD_") {
                Some(Mode::Cloud)
            } else {
                None
            };
            let result = load_from(
                &home,
                &opts(run(mode)),
                &|name| (name == var).then(not_utf8),
                &mut |_| {},
            );
            assert_eq!(
                env_error(result),
                (var, "must be valid UTF-8".to_owned()),
                "{var}"
            );
        }
    }

    /// Section 5.7: loading reads no env var but the table's, and of those
    /// only the ones of the sections the command uses, so nothing else
    /// (`local.inference.*`, `environments.*`, `policy.overrides`, the earlier
    /// `RAYLINE_ROUTING_*` names, any other `RAYLINE_*`) can be overridden.
    #[test]
    fn loading_reads_only_the_override_table() {
        let home = temp_home();
        write_default(&home, &example());
        let file = write_file(&home, "other.json", &example());
        let cloud_vars = ["RAYLINE_CLOUD_MODEL_ID", "RAYLINE_CLOUD_SMOL_MODEL_ID"];
        let routing_vars = [
            "RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_ID",
            "RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE",
            "RAYLINE_LOCAL_ROUTING_CLASSIFIER_MODEL",
            "RAYLINE_LOCAL_ROUTING_POLICY_PRESET",
            "RAYLINE_LOCAL_ROUTING_ROUTER",
        ];
        let table: Vec<&str> = OVERRIDES.iter().map(|(var, _)| *var).collect();
        for (command, section) in [
            (run(None), &routing_vars[..]),
            (run(Some(Mode::Cloud)), &cloud_vars[..]),
            (run_file(&file), &routing_vars[..]),
            (START, &routing_vars[..]),
            (start_file(&file), &routing_vars[..]),
            (Command::Other, &[][..]),
        ] {
            for env in [None, Some("dev")] {
                for harness_protocol in [None, Some(Protocol::AnthropicMessages)] {
                    let options = LoadOptions {
                        env,
                        harness_protocol,
                        ..opts(command)
                    };
                    let read = RefCell::new(Vec::new());
                    // Any other var the loader asked for would be set.
                    let settings = load_from(
                        &home,
                        &options,
                        &|name| {
                            read.borrow_mut().push(name.to_owned());
                            (!table.contains(&name)).then(|| OsString::from("other"))
                        },
                        &mut |_| {},
                    )
                    .expect("load");
                    let mut expected = vec!["RAYLINE_MODE"];
                    expected.extend(section);
                    expected.push("RAYLINE_UPDATE_CHANNEL");
                    assert_eq!(read.into_inner(), expected, "{command:?} {env:?}");
                    assert_eq!(
                        settings,
                        load(&home, options).expect("load"),
                        "{command:?} {env:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_earlier_env_names_are_not_read() {
        let home = temp_home();
        write_default(&home, &example());
        let earlier_names = [
            ("RAYLINE_ROUTING_ROUTER", "local"),
            ("RAYLINE_ROUTING_DECISIONS_MODEL", "arc-1.0"),
            ("RAYLINE_ROUTING_CLASSIFIER_MODEL", ""),
            ("RAYLINE_ROUTING_POLICY_PRESET", ""),
        ];
        for command in [run(None), START, Command::Other] {
            assert_eq!(
                load_env(&home, opts(command), &earlier_names).expect("load"),
                load(&home, opts(command)).expect("load"),
                "{command:?}"
            );
        }
    }

    #[test]
    fn an_override_for_a_missing_section_is_an_error_for_the_commands_that_read_it() {
        let home = temp_home();
        let mut doc = example();
        remove(&mut doc, RT);
        remove(&mut doc, "cloud");
        write_default(&home, &doc);
        let file = write_file(&home, "local.json", &doc);
        for (var, _) in ROUTING_OVERRIDES {
            for command in [run(None), run_file(&file), START, start_file(&file)] {
                assert_eq!(
                    env_error(load_env(&home, opts(command), &[(var, "x")])),
                    (
                        var,
                        "is set, but the config file has no `local.routing`".to_owned()
                    ),
                    "{command:?}"
                );
            }
            for command in [run(Some(Mode::Cloud)), Command::Other] {
                let result = load_env(&home, opts(command), &[(var, "x")]);
                assert!(
                    !matches!(result, Err(ConfigError::Env { .. })),
                    "{var} {command:?}: {result:?}"
                );
            }
        }
        for (var, _) in CLOUD_OVERRIDES {
            assert_eq!(
                env_error(load_env(&home, opts(run(Some(Mode::Cloud))), &[(var, "x")])),
                (var, "is set, but the config file has no `cloud`".to_owned())
            );
            for command in [START, Command::Other] {
                assert!(
                    !matches!(
                        load_env(&home, opts(command), &[(var, "x")]),
                        Err(ConfigError::Env { .. })
                    ),
                    "{var} {command:?}"
                );
            }
        }
        let settings = load_env(
            &home,
            opts(Command::Other),
            &[("RAYLINE_UPDATE_CHANNEL", "dev")],
        )
        .expect("update.channel needs no section");
        assert_eq!(settings.update.channel, UpdateChannel::Dev);
    }

    // The earlier shape.

    /// A file in the earlier shape with every section.
    fn earlier_file() -> Value {
        json!({
            "endpoints": example()["local"]["inference"]["endpoints"].clone(),
            "models": example()["local"]["inference"]["models"].clone(),
            "routing": {
                "decisions_model": "arc-swift-1.0",
                "classifier_model": "anthropic/claude-haiku-5.5",
                "policy": example()["local"]["routing"]["policy"].clone()
            },
            "environments": example()["environments"].clone(),
            "update": { "channel": "dev" }
        })
    }

    #[test]
    fn an_earlier_shape_default_file_is_converted_and_kept_as_a_backup() {
        let home = temp_home();
        let original = serde_json::to_string_pretty(&earlier_file()).expect("JSON");
        let file = write_default_text(&home, &original);
        let backup = file.with_file_name("settings.json.bak");
        let (settings, said) = load_said(&home, opts(run(None)), &[]);
        let settings = settings.expect("load");
        assert_eq!(
            said,
            [format!(
                "Converted {} from the earlier shape (`mode` `local`); the original is {}",
                file.display(),
                backup.display()
            )]
        );
        assert_eq!(fs::read_to_string(&backup).expect("backup"), original);
        // Everything the file holds, under its new keys, with `mode` and
        // the default file's `cloud`.
        let mut expected = example();
        set(&mut expected, &format!("{RT} router"), json!("cloud"));
        let converted = fs::read_to_string(&file).expect("converted");
        assert_eq!(parse_json(&converted).ok(), Some(expected.clone()));
        assert!(
            converted.starts_with("{\n  \"mode\": \"local\",\n  \"cloud\": {\n"),
            "{converted}"
        );
        assert_eq!(
            settings,
            load_doc_for(&expected, run(None))
                .map(|s| Settings {
                    file: file.clone(),
                    ..s
                })
                .expect("load")
        );
        // Converted once: the next load reads the new file and says nothing.
        let (again, said) = load_said(&home, opts(run(None)), &[]);
        assert_eq!(again.expect("load"), settings);
        assert!(said.is_empty(), "{said:?}");
        assert_eq!(fs::read_to_string(&backup).expect("backup"), original);
    }

    #[test]
    fn an_earlier_shape_file_takes_its_mode_from_routing() {
        let mut no_routing = earlier_file();
        remove(&mut no_routing, "routing");
        for (doc, mode) in [
            (no_routing, "cloud"),
            (json!({}), "cloud"),
            (json!({"update": {"channel": "main"}}), "cloud"),
            (json!({"routing": {}}), "local"),
            (earlier_file(), "local"),
        ] {
            let home = temp_home();
            let file = write_default(&home, &doc);
            let loaded = load_said(&home, opts(Command::Other), &[]).0;
            if doc == json!({"routing": {}}) {
                // Converted as it is: the empty `local.routing` then misses
                // its required keys.
                assert_eq!(key_error(loaded).0, "local.routing.decisions_model");
            } else {
                loaded.expect("load");
            }
            let converted = parse_json(&fs::read_to_string(&file).expect("converted"))
                .ok()
                .expect("JSON");
            assert_eq!(converted["mode"], mode, "{doc}");
            assert_eq!(
                converted["cloud"],
                parse_json(DEFAULT_FILE).ok().expect("JSON")["cloud"],
                "{doc}"
            );
            assert_eq!(
                converted.get("local").is_some(),
                doc.get("endpoints").is_some() || doc.get("routing").is_some(),
                "{doc}"
            );
        }
    }

    #[test]
    fn an_earlier_shape_router_converts_to_cloud_unless_it_is_local() {
        for (router, converted) in [
            (None, "cloud"),
            (Some("hosted"), "cloud"),
            (Some("local"), "local"),
        ] {
            let mut doc = earlier_file();
            if let Some(router) = router {
                set(&mut doc, "routing router", json!(router));
            }
            set(&mut doc, "routing decisions_model", json!("arc-sage-1.0"));
            let home = temp_home();
            let file = write_default(&home, &doc);
            let routing = load(&home, opts(START))
                .expect("load")
                .routing
                .expect("routing");
            assert_eq!(routing.router.as_str(), converted, "{router:?}");
            let text = fs::read_to_string(&file).expect("converted");
            let value = parse_json(&text).ok().expect("JSON");
            assert_eq!(
                value["local"]["routing"]["decisions_model"],
                json!({"id": "arc-sage-1.0"})
            );
            assert_eq!(value["local"]["routing"]["router"], converted);
            assert!(!text.contains("hosted"), "{text}");
        }
    }

    #[test]
    fn the_backup_replaces_an_older_one() {
        let home = temp_home();
        let file = write_default(&home, &json!({}));
        let backup = file.with_file_name("settings.json.bak");
        fs::write(&backup, "an older backup").expect("write");
        load(&home, opts(Command::Other)).expect("load");
        assert_eq!(fs::read_to_string(&backup).expect("backup"), "{}");
    }

    #[test]
    fn every_command_that_reads_the_default_file_converts_it() {
        for command in [run(None), run(Some(Mode::Cloud)), START, Command::Other] {
            let home = temp_home();
            let file = write_default(&home, &earlier_file());
            let (_, said) = load_said(&home, opts(command), &[]);
            assert_eq!(said.len(), 1, "{command:?}");
            assert!(
                fs::read_to_string(&file)
                    .expect("read")
                    .contains("\"mode\""),
                "{command:?}"
            );
        }
    }

    #[test]
    fn an_invalid_earlier_shape_file_is_an_error_naming_its_key_and_nothing_is_written() {
        let cases = [
            (json!({"hybrid": {}}), "hybrid", "is not a known key"),
            (json!({"cloud": {}}), "cloud", "is not a known key"),
            (
                json!({"routing": {"router": "cloud"}}),
                "routing.router",
                "must be one of `local`, `hosted`",
            ),
            (
                json!({"routing": {"decisions_model": {"id": "arc-swift-1.0"}}}),
                "routing.decisions_model",
                "must be a string",
            ),
            (
                json!({"routing": {"router": "local", "decisions_model": "arc-1.0"}}),
                "routing.decisions_model",
                "is `arc-1.0`, which local decisions don't make: they make `arc-swift-1.0` \
                 and `arc-sage-1.0`",
            ),
            (
                json!({"routing": {"local_price": 1}}),
                "routing.local_price",
                "is not a known key",
            ),
            (
                json!({"endpoints": {"box": {"base_url": 5, "protocols": ["openai_chat"]}}}),
                "endpoints.box.base_url",
                "must be a string",
            ),
            (
                json!({"models": {"m": {"endpoint": "box", "model": "m"}}}),
                "models.m.endpoint",
                "names `box`, which is not in `endpoints`",
            ),
            (
                json!({"update": {"channel": "beta"}}),
                "update.channel",
                "must be one of `prod`, `dev`, `main`",
            ),
        ];
        for (doc, key, problem) in cases {
            let home = temp_home();
            let file = write_default(&home, &doc);
            let (result, said) = load_said(&home, opts(Command::Other), &[]);
            assert_eq!(
                key_error(result),
                (
                    key.to_owned(),
                    format!(
                        "{problem} (the file has no `mode`, so it is read as the earlier shape)"
                    )
                ),
                "{doc}"
            );
            assert!(said.is_empty(), "{said:?}");
            assert_eq!(fs::read_to_string(&file).expect("read"), doc.to_string());
            assert!(!file.with_file_name("settings.json.bak").exists(), "{doc}");
        }
    }

    #[test]
    fn a_default_file_that_is_not_a_json_object_is_not_converted() {
        for text in ["{", "[]", "\"x\""] {
            let home = temp_home();
            let file = write_default_text(&home, text);
            assert!(
                matches!(
                    load(&home, opts(Command::Other)),
                    Err(ConfigError::Parse { .. })
                ),
                "{text}"
            );
            assert_eq!(fs::read_to_string(&file).expect("read"), text);
            assert!(!file.with_file_name("settings.json.bak").exists());
        }
    }

    #[test]
    fn an_earlier_shape_config_file_is_a_load_error_and_is_not_converted() {
        let home = temp_home();
        for (doc, key) in [
            (earlier_file(), "endpoints"),
            (json!({"update": {}}), "mode"),
        ] {
            let file = write_file(&home, "old.json", &doc);
            for command in [run_file(&file), start_file(&file)] {
                let (result, said) = load_said(&home, opts(command), &[]);
                assert_eq!(key_error(result).0, key, "{command:?}");
                assert!(said.is_empty());
                assert_eq!(fs::read_to_string(&file).expect("read"), doc.to_string());
                assert!(!home.join("settings.json.bak").exists());
            }
        }
    }

    // The start checks.

    #[test]
    fn start_check_refuses_an_unset_api_key_env() {
        let home = temp_home();
        write_default(&home, &example());
        let file = write_file(&home, "local.json", &example());
        let arc_knows_all = |_: &str| true;
        for command in [START, start_file(&file)] {
            let settings = load(&home, opts(command)).expect("load");
            assert_eq!(
                key_error(check_start(&settings, &|_| None, &arc_knows_all)),
                (
                    "local.inference.endpoints.gpu-box.api_key_env".to_owned(),
                    "names `GPU_BOX_API_KEY`, which is not set".to_owned()
                )
            );
            let env = |name: &str| (name == "GPU_BOX_API_KEY").then(|| OsString::from("key"));
            check_start(&settings, &env, &arc_knows_all).expect("key is set");
        }
    }

    #[test]
    fn start_check_needs_limits_for_models_arc_does_not_know() {
        let env = |_: &str| Some(OsString::from("key"));
        let arc_knows_glm = |id: &str| id == GLM;
        let arc_knows_none = |_: &str| false;
        for key in ["context_window", "max_output_tokens"] {
            let mut doc = example();
            remove(&mut doc, &format!("{MD} {GLM} {key}"));
            let home = temp_home();
            let file = write_file(&home, "local.json", &doc);
            let settings = load(&home, opts(start_file(&file))).expect("load");
            check_start(&settings, &env, &arc_knows_glm).expect("ARC knows the model");
            assert_eq!(
                key_error(check_start(&settings, &env, &arc_knows_none)),
                (
                    format!("local.inference.models.\"z-ai/glm-5.3-flash\".{key}"),
                    "is required: ARC doesn't know this model".to_owned()
                )
            );
        }
        let settings = load_doc_for(&example(), START).expect("load");
        check_start(&settings, &env, &arc_knows_none).expect("limits are set");
    }

    #[test]
    fn errors_name_the_file_and_the_key() {
        let key = ConfigError::Key {
            file: PathBuf::from("settings.json"),
            key: "local.routing.policy.preset".to_owned(),
            problem: "is required".to_owned(),
        };
        assert_eq!(
            key.to_string(),
            "settings.json: `local.routing.policy.preset` is required"
        );
        let env = ConfigError::Env {
            var: "RAYLINE_UPDATE_CHANNEL",
            key: "update.channel",
            problem: "must not be empty".to_owned(),
        };
        assert_eq!(
            env.to_string(),
            "`RAYLINE_UPDATE_CHANNEL` (overrides `update.channel`) must not be empty"
        );
    }
}
