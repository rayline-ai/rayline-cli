//! `rayline run codex`: run Codex CLI on ARC through the Rayline router.
//!
//! Codex talks to the environment's router directly over the OpenAI Responses
//! API. It runs on the user's own Codex home (`CODEX_HOME`, else `~/.codex`)
//! with `--profile rayline`. Each launch rewrites that profile,
//! `rayline.config.toml`, owner-only: the `rayline` provider, the model and the
//! router key. The key never goes in Codex's argv or environment.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    pub codex_args: Vec<OsString>,
    /// The hosted environment the loaded config selects, whose router Codex
    /// talks to.
    pub hosted: crate::status::HostedEnvironment,
    /// `cloud`: the models the launch gives the harness.
    pub cloud: crate::config::Cloud,
}

/// The `rayline` provider Codex is configured with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Provider {
    /// The router's Responses base URL, ending in `/v1`.
    pub base_url: String,
    /// The `rlk-` router key Codex sends as its bearer token.
    pub key: String,
    pub model: String,
}

/// Resolve the provider for a Codex launch: the environment's router, the
/// stored `rlk-` router key (see [`crate::status::router_key`]) and
/// `cloud.model.id`.
pub(crate) async fn resolve_provider(
    hosted: &crate::status::HostedEnvironment,
    cloud: &crate::config::Cloud,
) -> Result<Provider, String> {
    let home = crate::home().map_err(|error| error.to_string())?;
    let router_url = &hosted.base_url;
    let key = crate::status::router_key(hosted, &home)
        .await
        .map_err(|error| error.to_string())?;
    let model = crate::harness::fetch_hosted_models(router_url, &key, cloud)
        .await?
        .main;
    Ok(Provider {
        base_url: format!("{router_url}/v1"),
        key,
        model,
    })
}

/// The Codex profile a cloud `rayline run codex` writes and runs with.
pub(crate) const PROFILE: &str = "rayline";

/// The user's Codex home: `CODEX_HOME`, else `~/.codex` under `home`, Codex's
/// own default.
pub(crate) fn user_codex_home(home: &Path) -> io::Result<PathBuf> {
    Ok(match crate::harness::exported_dir("CODEX_HOME")? {
        Some(codex_home) => codex_home,
        None => home.join(".codex"),
    })
}

/// The `rayline` profile in the user's Codex home: `rayline.config.toml`.
pub(crate) fn profile_path(home: &Path) -> io::Result<PathBuf> {
    Ok(user_codex_home(home)?.join(format!("{PROFILE}.config.toml")))
}

/// Write the `rayline` profile for `provider` into the user's Codex home,
/// owner-only since it holds the router key. Returns its path.
fn write_profile(home: &Path, provider: &Provider) -> io::Result<PathBuf> {
    let path = profile_path(home)?;
    let profile = profile_toml(provider, cfg!(windows), &path.with_file_name("config.toml"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::harness::write_private(&path, &profile)?;
    Ok(path)
}

/// The profile's text. On Windows (`windows`) it also sets `[windows]
/// sandbox`: the user's own value from `user_config` (their `config.toml`),
/// else `unelevated`, since Codex with no sandbox setting rejects every shell
/// command. A `user_config` that can't be read or parsed is an error naming it;
/// it is read only on Windows.
fn profile_toml(provider: &Provider, windows: bool, user_config: &Path) -> io::Result<String> {
    let profile = provider_config_toml(provider);
    if !windows {
        return Ok(profile);
    }
    let seed = crate::codex_app::read_if_present(user_config)?;
    crate::codex_app::with_windows_sandbox(&profile, seed.as_deref()).map_err(|error| {
        crate::codex_app::toml_parse_error(user_config, seed.as_deref().unwrap_or(""), &error)
    })
}

/// Delete the `rayline` profile at `path` when it holds `key`: the whole file
/// is Rayline's, and the next cloud `rayline run codex` writes it again. A
/// profile holding another environment's key, or none, is left alone. Returns
/// whether it was deleted.
pub(crate) fn remove_profile(path: &Path, key: &str) -> io::Result<bool> {
    let in_path =
        |error: io::Error| io::Error::new(error.kind(), format!("{}: {error}", path.display()));
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(in_path(error)),
    };
    if key.is_empty() || !String::from_utf8_lossy(&bytes).contains(key) {
        return Ok(false);
    }
    std::fs::remove_file(path).map_err(in_path)?;
    Ok(true)
}

/// `codex --profile rayline <args>`, on the user's own Codex home.
fn run_command(codex_args: Vec<OsString>) -> Command {
    let mut command = codex_command();
    command.arg("--profile").arg(PROFILE).args(codex_args);
    command
}

pub async fn run(request: RunRequest) -> ExitCode {
    let provider = match resolve_provider(&request.hosted, &request.cloud).await {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };
    let home = match crate::home() {
        Ok(home) => home,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };
    if let Err(error) = write_profile(&home, &provider) {
        eprintln!("Error: failed to write the Codex `{PROFILE}` profile: {error}");
        return ExitCode::from(1);
    }
    exec_or_status(&mut run_command(request.codex_args))
}

/// Keep the router key out of Codex's environment (#84). Codex reads the key
/// from its owner-only `config.toml`, so it never needs it in the environment.
/// Codex snapshots its environment for the shells it spawns
/// (`$CODEX_HOME/shell_snapshots`), so a key it inherited would be written to
/// disk in plain text.
pub(crate) fn withhold_router_key(command: &mut Command) {
    command.env_remove("RAYLINE_ROUTER_API_KEY");
}

/// A `codex` child process with the router key withheld (#84). Every Codex
/// spawn goes through here (the run and the app launch), so none can inherit
/// the key, and each finds npm's `codex.cmd` on Windows.
pub(crate) fn codex_command() -> Command {
    codex_command_on(std::env::var_os("PATH"))
}

fn codex_command_on(path: Option<OsString>) -> Command {
    let mut command = Command::new(crate::harness::launch_program("codex", path));
    withhold_router_key(&mut command);
    command
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// The Codex `config.toml` settings that point Codex at the `rayline` provider.
/// `rayline run codex` writes them as the `rayline` profile; `rayline run codex
/// app` merges them into its own `CODEX_HOME`'s `config.toml`.
pub(crate) fn provider_config_toml(provider: &Provider) -> String {
    format!(
        "model = {}\nmodel_provider = \"rayline\"\n\n[model_providers.rayline]\nname = \"Rayline\"\nbase_url = {}\nwire_api = \"responses\"\nexperimental_bearer_token = {}\n",
        toml_string(&provider.model),
        toml_string(&provider.base_url),
        toml_string(&provider.key),
    )
}

#[cfg(unix)]
fn exec_or_status(command: &mut Command) -> ExitCode {
    use std::os::unix::process::CommandExt;

    let error = command.exec();
    eprintln!("rayline: failed to exec codex: {error}");
    ExitCode::from(127)
}

#[cfg(not(unix))]
fn exec_or_status(command: &mut Command) -> ExitCode {
    match command.status() {
        Ok(status) => crate::child_exit_code(status),
        Err(error) => {
            eprintln!("rayline: failed to run codex: {error}");
            ExitCode::from(127)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{Provider, codex_command, codex_command_on, provider_config_toml};
    use std::ffi::OsString;

    /// A provider as a launch resolves it, with a placeholder key.
    pub(crate) fn provider(model: &str) -> Provider {
        Provider {
            base_url: "https://api.rayline.ai/v1".to_owned(),
            key: "rlk-test-not-a-real-key".to_owned(),
            model: model.to_owned(),
        }
    }

    #[test]
    fn provider_config_points_codex_at_the_router_with_the_key() {
        let toml = provider_config_toml(&provider("arc-sage-1.0"));
        let doc: toml_edit::DocumentMut = toml.parse().unwrap();
        assert_eq!(doc["model"].as_str(), Some("arc-sage-1.0"));
        assert_eq!(doc["model_provider"].as_str(), Some("rayline"));
        let rayline = &doc["model_providers"]["rayline"];
        assert_eq!(
            rayline["base_url"].as_str(),
            Some("https://api.rayline.ai/v1")
        );
        assert_eq!(rayline["wire_api"].as_str(), Some("responses"));
        assert_eq!(
            rayline["experimental_bearer_token"].as_str(),
            Some("rlk-test-not-a-real-key")
        );
        for absent in ["requires_openai_auth", "forced_login_method", "127.0.0.1"] {
            assert!(!toml.contains(absent), "{absent}");
        }
    }

    /// Runs `act` with `CODEX_HOME` set to `value` (or unset), restoring it.
    fn with_codex_home<T>(value: Option<&std::path::Path>, act: impl FnOnce() -> T) -> T {
        let saved = std::env::var_os("CODEX_HOME");
        // SAFETY: the workspace tests run with --test-threads=1; restored below.
        match value {
            Some(value) => unsafe { std::env::set_var("CODEX_HOME", value) },
            None => unsafe { std::env::remove_var("CODEX_HOME") },
        }
        let result = act();
        // SAFETY: as above.
        match saved {
            Some(value) => unsafe { std::env::set_var("CODEX_HOME", value) },
            None => unsafe { std::env::remove_var("CODEX_HOME") },
        }
        result
    }

    /// The profile goes into the user's Codex home, owner-only, and replaces
    /// the last launch's; the user's own files there are left alone, and
    /// nothing is written under `~/.rayline`.
    #[test]
    fn the_profile_is_written_into_the_users_codex_home() {
        let home = crate::harness::tests::temp_dir("codex-profile");
        let user_config = "model = \"gpt-x\"\n[mcp_servers.docs]\ncommand = \"docs-mcp\"\n";
        let default_home = home.join(".codex");
        std::fs::create_dir_all(&default_home).unwrap();
        std::fs::write(default_home.join("config.toml"), user_config).unwrap();
        let path = with_codex_home(None, || {
            super::write_profile(&home, &provider("arc-sage")).unwrap();
            super::write_profile(&home, &provider("arc-swift")).unwrap()
        });
        assert_eq!(path, default_home.join("rayline.config.toml"));
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, provider_config_toml(&provider("arc-swift")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(
            std::fs::read_to_string(default_home.join("config.toml")).unwrap(),
            user_config
        );
        assert!(!home.join(crate::DOT_CONFIG_DIR).exists());

        // `CODEX_HOME` names the user's Codex home, created when missing.
        let exported = home.join("elsewhere").join("codex");
        let path = with_codex_home(Some(&exported), || {
            super::write_profile(&home, &provider("arc-sage")).unwrap()
        });
        assert_eq!(path, exported.join("rayline.config.toml"));
        std::fs::remove_dir_all(home).unwrap();
    }

    /// On Windows the profile carries the user's sandbox, else `unelevated`;
    /// elsewhere it is the provider alone and the user's config isn't read.
    #[test]
    fn the_profile_sets_the_windows_sandbox_only_on_windows() {
        let dir = crate::harness::tests::temp_dir("codex-profile-sandbox");
        let user_config = dir.join("config.toml");
        let sandbox = |windows| {
            let text = super::profile_toml(&provider("arc-sage"), windows, &user_config).unwrap();
            let doc: toml_edit::DocumentMut = text.parse().unwrap();
            doc.get("windows")
                .and_then(|windows| windows.get("sandbox"))
                .and_then(|sandbox| sandbox.as_str())
                .map(ToOwned::to_owned)
        };
        assert_eq!(sandbox(false), None);
        assert_eq!(sandbox(true).as_deref(), Some("unelevated"));
        std::fs::write(&user_config, "[windows]\nsandbox = \"elevated\"\n").unwrap();
        assert_eq!(sandbox(true).as_deref(), Some("elevated"));
        std::fs::write(&user_config, "not toml = = =\n").unwrap();
        assert_eq!(sandbox(false), None);
        let error = super::profile_toml(&provider("arc-sage"), true, &user_config).unwrap_err();
        assert!(error.to_string().contains("config.toml"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Logout deletes a profile that holds its key, and leaves one that holds
    /// another environment's key.
    #[test]
    fn remove_profile_deletes_only_a_profile_with_the_key() {
        let dir = crate::harness::tests::temp_dir("codex-remove-profile");
        let path = dir.join("rayline.config.toml");
        assert!(!super::remove_profile(&path, "rlk-a").unwrap());
        std::fs::write(&path, "experimental_bearer_token = \"rlk-b\"\n").unwrap();
        assert!(!super::remove_profile(&path, "rlk-a").unwrap());
        assert!(!super::remove_profile(&path, "").unwrap());
        assert!(path.exists());
        assert!(super::remove_profile(&path, "rlk-b").unwrap());
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The launch runs the `rayline` profile ahead of the user's arguments,
    /// on the inherited `CODEX_HOME`, without the router key.
    #[test]
    fn the_run_uses_the_rayline_profile_on_the_users_home() {
        let command = super::run_command(vec![OsString::from("exec"), OsString::from("hi")]);
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["--profile", "rayline", "exec", "hi"]);
        assert!(command.get_envs().all(|(key, _)| key != "CODEX_HOME"));
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "RAYLINE_ROUTER_API_KEY" && value.is_none())
        );
    }

    /// #84: every Codex child is built by `codex_command`, which withholds the
    /// router key, so no spawn site can forget it: the app launch included.
    #[test]
    fn every_codex_spawn_goes_through_codex_command() {
        let sources = [
            ("codex.rs", include_str!("codex.rs")),
            ("codex_app.rs", include_str!("codex_app.rs")),
        ];
        let count = |needle: &str| -> usize {
            sources
                .iter()
                .map(|(_, text)| text.matches(needle).count())
                .sum()
        };
        let bare = count(concat!("Command::new(", "\"codex\")"));
        assert_eq!(
            bare, 0,
            "a Codex child is spawned by bare name, which misses codex.cmd on Windows"
        );
        let total = count(concat!("launch_program(", "\"codex\""));
        assert_eq!(
            total, 1,
            "a Codex child is spawned outside codex_command (found {total} spawn sites)"
        );
    }

    /// Windows process creation does not apply PATHEXT, so a bare `codex`
    /// misses the `codex.cmd` launcher npm installs. The command resolves it
    /// the way a local-mode run does, and keeps withholding the router key.
    #[test]
    fn codex_command_finds_the_npm_launcher_on_windows() {
        let dir = crate::harness::tests::temp_dir("codex-launcher");
        // npm installs an extensionless sh shim beside the `.cmd` launcher.
        std::fs::write(dir.join("codex"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("codex.cmd"), "@echo off\r\n").unwrap();
        let command = codex_command_on(Some(std::env::join_paths([&dir]).unwrap()));
        if cfg!(windows) {
            assert_eq!(command.get_program(), dir.join("codex.cmd").as_os_str());
        } else {
            assert_eq!(command.get_program(), "codex");
        }
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "RAYLINE_ROUTER_API_KEY" && value.is_none())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_command_withholds_the_router_key() {
        let command = codex_command();
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "RAYLINE_ROUTER_API_KEY" && value.is_none()),
            "codex_command does not remove RAYLINE_ROUTER_API_KEY"
        );
    }

    /// #84: a router key in the launcher's environment must not reach Codex,
    /// which snapshots its environment to disk for the shells it spawns.
    #[cfg(unix)]
    #[test]
    fn codex_child_never_inherits_the_router_key() {
        let var = "RAYLINE_ROUTER_API_KEY";
        let previous = std::env::var_os(var);
        unsafe { std::env::set_var(var, "rlk-test-not-a-real-key") };
        let mut command = std::process::Command::new("/usr/bin/env");
        super::withhold_router_key(&mut command);
        let output = command.output().expect("run env");
        match previous {
            Some(value) => unsafe { std::env::set_var(var, value) },
            None => unsafe { std::env::remove_var(var) },
        }
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success());
        assert!(
            !printed.contains(var),
            "the child environment still carries {var}"
        );
    }
}
