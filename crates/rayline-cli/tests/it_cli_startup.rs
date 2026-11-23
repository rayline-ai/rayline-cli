//! The installed binary refuses a bad command line or config before it runs
//! anything (docs/spec.md sections 3 and 5.8), and writes or converts the
//! default config file only for a command that reads it (section 5.9).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("rayline-startup-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&path).expect("temporary home");
        Self(path)
    }

    fn settings_file(&self) -> PathBuf {
        self.0.join(".config").join("rayline").join("settings.json")
    }

    fn settings(&self, json: &str) {
        let file = self.settings_file();
        fs::create_dir_all(file.parent().expect("settings dir")).expect("settings dir");
        fs::write(file, json).expect("settings file");
    }

    /// Runs `rayline` in the home directory with `PATH` empty, so a run that got as far as starting
    /// a harness would fail differently.
    fn run(&self, args: &[&str]) -> Output {
        self.run_exe(Path::new(env!("CARGO_BIN_EXE_rayline")), args)
    }

    /// Like [`Home::run`], with the binary at `exe`. The Hugging Face cache is
    /// `<home>/hf`.
    fn run_exe(&self, exe: &Path, args: &[&str]) -> Output {
        self.run_exe_on(exe, args, Path::new(""))
    }

    /// Like [`Home::run_exe`], with `PATH` set to `path`.
    fn run_exe_on(&self, exe: &Path, args: &[&str], path: &Path) -> Output {
        let mut command = Command::new(exe);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("RAYLINE_") {
                command.env_remove(name);
            }
        }
        command
            .args(args)
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env("PATH", path)
            .env("HF_HOME", self.0.join("hf"))
            .env_remove("HF_HUB_CACHE")
            .output()
            .expect("run rayline")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const ROUTING: &str = r#"{"mode": "local", "local": {"routing": {"router": "cloud",
    "decisions_model": {"id": "arc-swift-1.0"},
    "classifier_model": "anthropic/claude-haiku-5.5",
    "policy": {"preset": "arc-policy.hybrid-default.v1"}}}}"#;

/// A `--config` file with `local.routing` and `mode` `local`, inside the
/// temporary home.
const ROUTING_FILE: &str = "routing.json";

fn assert_fails(output: &Output, code: i32, stderr: &str) {
    let text = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(code), "{text}");
    assert!(text.contains(stderr), "{text}");
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn a_usage_error_exits_2() {
    let home = Home::new();
    for name in ["nope", "claude-desktop"] {
        assert_fails(
            &home.run(&["run", name]),
            2,
            &format!("rayline: unknown harness `{name}`"),
        );
    }
    assert_fails(
        &home.run(&["run", "pi", "--local"]),
        2,
        "unexpected `--local`",
    );
    assert_fails(
        &home.run(&["local", "start", "--router", "hosted"]),
        2,
        "--router must be `cloud` or `local`, not `hosted`",
    );
    assert!(
        !home.settings_file().exists(),
        "a usage error reads no file"
    );
}

#[test]
fn local_mode_without_local_fails_before_anything_runs() {
    let home = Home::new();
    // The default file a new user gets is in cloud mode, with no `local`.
    for args in [&["run", "pi", "--mode", "local"][..], &["local", "start"]] {
        assert_fails(&home.run(args), 1, "`local` is required in local mode");
    }
    home.settings(r#"{"mode": "local", "local": {}}"#);
    for args in [&["run", "pi"][..], &["local", "start"]] {
        assert_fails(
            &home.run(args),
            1,
            "`local.routing` is required in local mode",
        );
    }
}

#[test]
fn the_default_file_is_written_for_a_command_that_reads_it() {
    let home = Home::new();
    for args in [
        &["--version"][..],
        &["--help"],
        &["run", "--help"],
        &["local", "start", "--help"],
        &["run", "claude", "--config", "missing.json"],
    ] {
        home.run(args);
        assert!(!home.settings_file().exists(), "{args:?}");
    }
    let output = home.run(&["local", "status"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!(
            "Created {} with the default settings (`mode` `cloud`)\n",
            home.settings_file().display()
        )
    );
    let written = fs::read_to_string(home.settings_file()).expect("written");
    assert!(written.starts_with("{\n  \"mode\": \"cloud\""), "{written}");
}

#[test]
fn an_earlier_shape_default_file_is_converted_by_a_command_that_reads_it() {
    let home = Home::new();
    let original = r#"{"routing": {"router": "hosted", "decisions_model": "arc-sage-1.0",
        "policy": {"preset": "arc-policy.hybrid-default.v1"}}}"#;
    home.settings(original);
    for args in [&["--version"][..], &["--help"]] {
        home.run(args);
        assert_eq!(
            fs::read_to_string(home.settings_file()).expect("kept"),
            original
        );
    }
    let output = home.run(&["local", "status"]);
    assert_eq!(output.status.code(), Some(0));
    let backup = home.settings_file().with_file_name("settings.json.bak");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!(
            "Converted {} from the earlier shape (`mode` `local`); the original is {}\n",
            home.settings_file().display(),
            backup.display()
        )
    );
    assert_eq!(fs::read_to_string(&backup).expect("backup"), original);
    let converted = fs::read_to_string(home.settings_file()).expect("converted");
    assert!(converted.contains("\"router\": \"cloud\""), "{converted}");
    assert!(
        converted.contains("\"decisions_model\": {\n        \"id\": \"arc-sage-1.0\""),
        "{converted}"
    );
}

#[test]
fn an_earlier_shape_config_file_fails_and_is_not_converted() {
    let home = Home::new();
    let original = r#"{"routing": {"decisions_model": "arc-swift-1.0",
        "policy": {"preset": "arc-policy.hybrid-default.v1"}}}"#;
    fs::write(home.0.join("old.json"), original).expect("old file");
    for args in [
        &["run", "pi", "--config", "old.json"][..],
        &["local", "start", "--config", "old.json"],
    ] {
        assert_fails(&home.run(args), 1, "`routing` is not a known key");
        assert_eq!(
            fs::read_to_string(home.0.join("old.json")).expect("kept"),
            original
        );
        assert!(!home.0.join("old.json.bak").exists());
        assert!(!home.settings_file().exists());
    }
}

#[test]
fn a_missing_config_file_fails_before_anything_runs() {
    let home = Home::new();
    let missing = home.0.join("missing.json");
    let missing = missing.to_str().expect("utf-8");
    for args in [
        &["run", "claude", "--config", missing][..],
        &["local", "start", "--config", missing],
    ] {
        assert_fails(&home.run(args), 1, "missing.json");
    }
}

#[test]
fn claude_is_found_on_path_only() {
    let home = Home::new();
    // Where Claude Code's installers put it; neither is looked at.
    for file in [".local/bin/claude", ".claude/local/claude"] {
        let file = home.0.join(file);
        fs::create_dir_all(file.parent().expect("parent")).expect("install dir");
        fs::write(file, "").expect("claude");
    }
    assert_fails(
        &home.run(&["run", "claude"]),
        1,
        "`claude` not found on PATH",
    );
}

#[test]
fn a_config_file_starts_the_local_process_from_it() {
    let home = Home::new();
    fs::write(home.0.join(ROUTING_FILE), ROUTING).expect("routing file");
    for args in [
        &["run", "claude", "--config", ROUTING_FILE][..],
        &["local", "start", "--config", ROUTING_FILE],
        &["run", "codex", "--config", ROUTING_FILE],
    ] {
        assert_fails(&home.run(args), 1, "installed rayline-core is missing at");
    }
}

#[test]
fn an_unknown_env_fails_before_anything_runs() {
    let home = Home::new();
    for args in [
        &["--env", "nope", "run", "pi"][..],
        &["--env", "nope", "auth", "status"],
    ] {
        assert_fails(
            &home.run(args),
            1,
            "`environments.nope` is not defined, and `--env nope` selects it",
        );
    }
}

#[test]
fn a_bad_update_channel_fails_before_update_runs() {
    let home = Home::new();
    home.settings(
        r#"{"mode": "cloud", "cloud": {"model": {"id": "m"}},
        "update": {"channel": "nightly"}}"#,
    );
    assert_fails(&home.run(&["update", "--check"]), 1, "update.channel");
}

#[test]
fn local_mode_refuses_openclaw_container_mode_before_starting_the_local_process() {
    let home = Home::new();
    home.settings(ROUTING);
    assert_fails(
        &home.run(&["run", "openclaw", "--", "--container", "c"]),
        1,
        "OpenClaw container mode cannot use the hybrid gateway configuration",
    );
}

#[test]
fn local_mode_refuses_the_kept_options_it_does_not_run() {
    let home = Home::new();
    home.settings(ROUTING);
    for (args, option) in [
        (&["run", "claude", "--isolated"][..], "`--isolated`"),
        (&["run", "codex", "app"], "`run codex app`"),
        (&["run", "codex", "configure"], "`run codex configure`"),
    ] {
        assert_fails(
            &home.run(args),
            1,
            &format!("Error: {option} is not available in local mode"),
        );
    }
}

/// A config whose model names a tokenizer, so a start that got past the
/// router key would download it.
const LOCAL_WITH_TOKENIZER: &str = r#"{"mode": "local", "local": {
    "inference": {
        "endpoints": {"ollama": {"base_url": "http://127.0.0.1:9",
            "protocols": ["anthropic_messages", "openai_chat"]}},
        "models": {"z-ai/glm-5.3-flash": {"endpoint": "ollama", "model": "glm",
            "context_window": 1000000, "max_output_tokens": 32000,
            "tokenizer": "zai-org/GLM-5.3-Flash",
            "price": {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0}}}},
    "routing": {"router": "cloud", "decisions_model": {"id": "arc-swift-1.0"},
        "classifier_model": "anthropic/claude-haiku-5.5",
        "policy": {"preset": "arc-policy.hybrid-default.v1"}}}}"#;

#[test]
fn a_signed_out_local_start_fails_before_it_clears_state_or_downloads() {
    let home = Home::new();
    home.settings(LOCAL_WITH_TOKENIZER);
    // An installed release: `rayline` with `rayline-core` beside it.
    let bin = home.0.join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let exe = bin.join(format!("rayline{}", std::env::consts::EXE_SUFFIX));
    fs::copy(env!("CARGO_BIN_EXE_rayline"), &exe).expect("copy rayline");
    fs::write(
        bin.join(format!("rayline-core{}", std::env::consts::EXE_SUFFIX)),
        "",
    )
    .expect("core");
    // What a local process that has exited left behind.
    let mut exited = Command::new(&exe)
        .arg("--version")
        .stdout(Stdio::null())
        .spawn()
        .expect("run rayline");
    exited.wait().expect("rayline exits");
    let state = home.0.join(".rayline").join("local");
    fs::create_dir_all(&state).expect("state dir");
    let supervisor = format!(
        r#"{{"pid": {}, "config": "/c.json", "env": null, "router": "cloud"}}"#,
        exited.id()
    );
    fs::write(state.join("supervisor.json"), &supervisor).expect("supervisor");
    fs::write(state.join("ready.json"), "{}").expect("ready");

    for args in [&["local", "start"][..], &["run", "claude"]] {
        assert_fails(
            &home.run_exe(&exe, args),
            1,
            "Not signed in to Rayline. Run: rayline auth login",
        );
        assert_eq!(
            fs::read_to_string(state.join("supervisor.json")).expect("supervisor kept"),
            supervisor
        );
        assert!(state.join("ready.json").exists());
        assert!(!home.0.join("hf").exists());
    }
}

/// A lone `--help`, `-h`, `--version` or `-v` goes straight to the harness:
/// signed out, with no settings file and no local process, nothing else runs.
#[cfg(unix)]
#[test]
fn a_lone_help_or_version_flag_runs_only_the_harness() {
    use std::os::unix::fs::PermissionsExt as _;

    let home = Home::new();
    let bin = home.0.join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let harnesses = [
        "claude", "codex", "opencode", "pi", "omp", "hermes", "openclaw",
    ];
    for name in harnesses {
        let script = bin.join(name);
        fs::write(&script, format!("#!/bin/sh\necho \"{name} $*\"\nexit 3\n")).expect("script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let exe = Path::new(env!("CARGO_BIN_EXE_rayline"));
    for name in harnesses {
        for flag in ["--help", "-h", "--version", "-v"] {
            for args in [
                vec!["run", name, flag],
                vec!["run", name, "--mode", "local", flag],
                vec!["run", name, "--", flag],
            ] {
                let output = home.run_exe_on(exe, &args, &bin);
                assert_eq!(output.status.code(), Some(3), "{args:?}");
                assert_eq!(
                    String::from_utf8_lossy(&output.stdout),
                    format!("{name} {flag}\n"),
                    "{args:?}"
                );
                assert!(output.stderr.is_empty(), "{args:?}");
            }
        }
    }
    assert!(!home.0.join(".config").exists());
    // Anything more than the one flag is a launch, which reads the settings
    // (writing the default file) and needs a sign-in.
    assert_fails(
        &home.run_exe_on(exe, &["run", "claude", "--help", "x"], &bin),
        1,
        "Not signed in to Rayline. Run: rayline auth login",
    );
    assert!(!home.0.join(".rayline").exists());
    assert!(home.settings_file().exists());
}
