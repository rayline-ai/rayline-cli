//! pi: `PI_CODING_AGENT_DIR=<dir>`, which holds `models.json`, `settings.json`,
//! `auth.json` and `sessions/`. pi resolves `$VAR` in header values; `apiKey`
//! is the literal key.
//!
//! `compat.sendSessionAffinityHeaders` makes pi send its session id as
//! `x-session-affinity` on Messages (off by default for non-OpenRouter
//! endpoints), which the gateway keys the episode on. `-c` and `--resume`
//! reuse the session id, so a resumed session keeps its episode. A local-mode
//! turn without that id fails, so local mode refuses a pi older than
//! [`MIN_LOCAL_VERSION`], the first release tested to send it, before anything
//! starts.
//!
//! `compat.allowEmptySignature` is not set, in any environment. pi then resends
//! a thinking block that came back without a signature as a text block, which
//! is its default for Messages, so no thinking block with an empty signature
//! goes upstream.

use std::process::Command;

use serde_json::json;

use super::{Plan, RenderContext, json_file};

pub(crate) const PROVIDER_ID: &str = "rayline";
/// The first pi tested to send `x-session-affinity` on Messages.
pub(crate) const MIN_LOCAL_VERSION: (u64, u64, u64) = (0, 87, 1);

/// Refuse a pi that local mode can't run: older than [`MIN_LOCAL_VERSION`].
/// `command` is pi with the launch's environment. A pi that cannot run, exits
/// non-zero or prints no version is an error naming the probe.
pub(crate) fn require_local_version(command: Command) -> Result<(), String> {
    local_version_error(super::installed_version(command)?).map_or(Ok(()), Err)
}

fn local_version_error(version: (u64, u64, u64)) -> Option<String> {
    (version < MIN_LOCAL_VERSION).then(|| {
        let (major, minor, patch) = MIN_LOCAL_VERSION;
        format!(
            "pi {}.{}.{} doesn't send its session id, which local mode needs; upgrade to pi \
             {major}.{minor}.{patch} or later: npm install -g @earendil-works/pi-coding-agent",
            version.0, version.1, version.2
        )
    })
}

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    let compat = json!({
        "sendSessionAffinityHeaders": true
    });
    let launch = &ctx.models;
    let entries: Vec<_> = launch
        .offered
        .iter()
        .map(|model| {
            json!({
                "id": model.id,
                "name": model.display_name,
                // ARC selects thinking effort; clients must not expose controls.
                "reasoning": ctx.capabilities.is_some(),
                "input": model.input_modalities,
                "contextWindow": model.context_window,
                "maxTokens": model.max_output_tokens
            })
        })
        .collect();
    let models = json!({
        "providers": {
            PROVIDER_ID: {
                "baseUrl": ctx.messages_base(),
                "api": "anthropic-messages",
                "apiKey": ctx.key,
                "compat": compat,
                "models": entries
            }
        }
    });
    // pi writes settings.json itself (model picks, theme), so only seed it.
    let settings = json!({
        "defaultProvider": PROVIDER_ID,
        "defaultModel": launch.main
    });
    Plan {
        files: vec![
            json_file("models.json", &models, true),
            json_file("settings.json", &settings, false),
        ],
        env_set: vec![("PI_CODING_AGENT_DIR", ctx.config_dir.as_os_str().to_owned())],
        env_remove: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::Harness;
    use super::super::tests::{TEST_URL, env_value, render};
    use super::local_version_error;

    #[test]
    fn local_mode_refuses_a_pi_that_sends_no_session_id() {
        let error = local_version_error((0, 73, 1)).expect("too old");
        assert!(
            error.starts_with("pi 0.73.1 doesn't send its session id"),
            "{error}"
        );
        assert!(error.contains("pi 0.87.1 or later"), "{error}");
        assert_eq!(local_version_error((0, 87, 1)), None);
        assert_eq!(local_version_error((1, 5, 0)), None);
    }

    #[test]
    fn renders_models_and_default_settings() {
        let (dir, plan) = render(Harness::Pi);
        assert_eq!(
            env_value(&plan, "PI_CODING_AGENT_DIR"),
            Some(&dir.clone().into_os_string())
        );
        let read = |name: &str| -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap()
        };
        let provider = &read("models.json")["providers"]["rayline"];
        assert_eq!(provider["baseUrl"], TEST_URL);
        assert_eq!(provider["api"], "anthropic-messages");
        assert_eq!(provider["apiKey"], super::super::tests::TEST_KEY);
        assert_eq!(provider["compat"]["sendSessionAffinityHeaders"], true);
        // Unsigned thinking is resent as text, pi's default for Messages.
        assert_eq!(provider["compat"].get("allowEmptySignature"), None);
        let ids: Vec<_> = provider["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["arc-sage-1.0", "arc-swift-1.0"]);
        let model = &provider["models"][0];
        assert_eq!(model["name"], "ARC Sage");
        assert_eq!(model["reasoning"], false);
        assert_eq!(model["input"], serde_json::json!(["text", "image"]));
        assert_eq!(model["contextWindow"], 1000000);
        assert_eq!(model["maxTokens"], 128000);
        assert_eq!(provider["models"][1]["name"], "ARC Swift");
        assert_eq!(provider["models"][1]["maxTokens"], 128000);
        let settings = read("settings.json");
        assert_eq!(settings["defaultProvider"], "rayline");
        assert_eq!(settings["defaultModel"], "arc-sage-1.0");
    }

    #[test]
    fn a_contributor_key_seeds_its_listed_sage() {
        let dir = super::super::tests::temp_dir("pi-contributor");
        let mut ctx = super::super::tests::render_context(&dir);
        ctx.models = super::super::LaunchModels::from_listing(&super::super::tests::listing(Some(
            "contributor",
        )))
        .unwrap();
        super::super::write_files(&dir, &super::plan(&ctx).files).unwrap();
        let read = |name: &str| -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap()
        };
        let settings = read("settings.json");
        assert_eq!(settings["defaultProvider"], "rayline");
        assert_eq!(settings["defaultModel"], "arc-sage-contributor-1.0");
        let ids: Vec<_> = read("models.json")["providers"]["rayline"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids[0], "arc-sage-contributor-1.0");
    }

    #[test]
    fn saved_settings_are_left_as_pi_wrote_them() {
        let dir = super::super::tests::temp_dir("pi-saved");
        let ctx = super::super::tests::render_context(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        // A pick the key does not list stays: Rayline only seeds this file.
        let saved = "{\n  \"defaultProvider\": \"rayline\",\n  \"defaultModel\": \"rayline-router\",\n  \"theme\": \"dark\"\n}\n";
        std::fs::write(&settings, saved).unwrap();
        std::fs::write(dir.join("models.json"), "{}").unwrap();
        super::super::write_files(&dir, &super::plan(&ctx).files).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), saved);
        // models.json holds the key and URL, so every launch rewrites it.
        let models: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("models.json")).unwrap())
                .unwrap();
        assert_eq!(models["providers"]["rayline"]["baseUrl"], TEST_URL);
    }

    /// The probe reads the version pi prints on stdout, and refuses one it
    /// can't read.
    #[cfg(unix)]
    #[test]
    fn the_version_probe_reads_pi_s_stdout() {
        let probe = |script: &str| {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", script, "pi"]);
            super::require_local_version(command)
        };
        assert_eq!(probe("echo 0.87.1"), Ok(()));
        assert!(
            probe("echo 0.80.0")
                .unwrap_err()
                .contains("upgrade to pi 0.87.1")
        );
        assert!(
            probe("echo 0.87.1 >&2")
                .unwrap_err()
                .contains("printed no version")
        );
        assert!(
            probe("echo 0.87.1; exit 1")
                .unwrap_err()
                .contains("failed: exit status: 1")
        );
    }
}
