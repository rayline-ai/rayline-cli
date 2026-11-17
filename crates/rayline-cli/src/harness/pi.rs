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

use std::fmt;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;
use serde_json::{Value, json};

use super::{Plan, RenderContext, json_file};

pub(crate) const PROVIDER_ID: &str = "rayline";
/// The first pi tested to send `x-session-affinity` on Messages.
pub(crate) const MIN_LOCAL_VERSION: (u64, u64, u64) = (0, 87, 1);

/// Refuse a pi that local mode can't run: older than [`MIN_LOCAL_VERSION`].
/// `command` is pi with the launch's environment. A pi that cannot run, exits
/// non-zero or prints no version is an error naming the probe.
pub(crate) fn require_local_version(mut command: Command) -> Result<(), String> {
    let shown = format!("`{} --version`", Path::new(command.get_program()).display());
    let output = command
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("failed to run {shown}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{shown} failed: {}", output.status));
    }
    let printed = String::from_utf8_lossy(&output.stdout);
    let version = super::omp::parse_version(&printed)
        .ok_or_else(|| format!("{shown} printed no version: {:?}", printed.trim()))?;
    local_version_error(version).map_or(Ok(()), Err)
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

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Result<Plan, String> {
    let compat = json!({
        "sendSessionAffinityHeaders": true
    });
    let launch = &ctx.models;
    let entries: Vec<_> = launch
        .offered
        .iter()
        .map(|model| {
            let mut entry = json!({
                "id": model.id,
                // ARC selects thinking effort; clients must not expose controls.
                "reasoning": ctx.capabilities.is_some()
            });
            super::add_details(&mut entry, model);
            entry
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
    // pi also saves its own settings (theme, model picks) to settings.json, so
    // each launch sets only Rayline's keys there and keeps the rest.
    let settings = [
        ("defaultProvider", json!(PROVIDER_ID)),
        ("defaultModel", json!(launch.main)),
    ];
    let fresh = set_keys("{}\n", &settings).expect("an empty object takes any key");
    let settings = super::merged_file(ctx.config_dir, "settings.json", fresh, |existing| {
        set_keys(existing, &settings)
    })?;
    Ok(Plan {
        files: vec![json_file("models.json", &models), settings],
        env_set: vec![("PI_CODING_AGENT_DIR", ctx.config_dir.as_os_str().to_owned())],
        env_remove: Vec::new(),
    })
}

/// `existing`, a JSON object (pi's `settings.json`), with each of `values`
/// set at its top level: an entry with that key is replaced, and a missing one
/// is added at the end. Every other entry keeps its place and its text. The
/// result is laid out as pi writes the file (two-space indent).
fn set_keys(existing: &str, values: &[(&str, Value)]) -> Result<String, String> {
    let text = existing.strip_prefix('\u{feff}').unwrap_or(existing);
    let Entries(mut entries) = serde_json::from_str(text).map_err(|error| error.to_string())?;
    for (key, value) in values {
        let raw = RawValue::from_string(value.to_string()).expect("a Value is valid JSON");
        // pi (JSON.parse) reads the last of repeated keys; set them all.
        let mut found = false;
        for entry in entries.iter_mut().filter(|entry| entry.0 == *key) {
            entry.1 = raw.clone();
            found = true;
        }
        if !found {
            entries.push(((*key).to_owned(), raw));
        }
    }
    let mut out = String::from("{");
    for (index, (key, value)) in entries.iter().enumerate() {
        out.push_str(if index == 0 { "\n  " } else { ",\n  " });
        out.push_str(&serde_json::to_string(key).expect("a string serializes"));
        out.push_str(": ");
        out.push_str(value.get());
    }
    if !entries.is_empty() {
        out.push('\n');
    }
    out.push('}');
    if existing.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// A JSON object's entries in file order, each value as its original text.
struct Entries(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Object;
        impl<'de> Visitor<'de> for Object {
            type Value = Entries;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Entries, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Entries(entries))
            }
        }
        deserializer.deserialize_map(Object)
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
    fn a_contributor_key_sets_its_listed_sage() {
        let dir = super::super::tests::temp_dir("pi-contributor");
        let mut ctx = super::super::tests::render_context(&dir);
        ctx.models = super::super::LaunchModels::from_listing(
            &super::super::tests::listing(Some("contributor")),
            &super::super::tests::cloud(Some("contributor")),
        )
        .unwrap();
        super::super::write_files(&dir, &super::plan(&ctx).unwrap().files).unwrap();
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
    fn each_launch_sets_rayline_s_keys_and_keeps_pi_s() {
        let dir = super::super::tests::temp_dir("pi-saved");
        let ctx = super::super::tests::render_context(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        // As pi writes it (JSON.stringify, two-space indent, no final newline),
        // with a model picked in pi.
        let saved = "{\n  \"theme\": \"dark\",\n  \"defaultProvider\": \"other\",\n  \"defaultModel\": \"rayline-router\",\n  \"packages\": [\n    \"a\",\n    {\n      \"source\": \"b\"\n    }\n  ],\n  \"retry\": {}\n}";
        std::fs::write(&settings, saved).unwrap();
        std::fs::write(dir.join("models.json"), "{}").unwrap();
        super::super::write_files(&dir, &super::plan(&ctx).unwrap().files).unwrap();
        let expected = saved
            .replace("\"other\"", "\"rayline\"")
            .replace("\"rayline-router\"", "\"arc-sage-1.0\"");
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), expected);
        // models.json holds the key and URL, so every launch rewrites it.
        let models: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("models.json")).unwrap())
                .unwrap();
        assert_eq!(models["providers"]["rayline"]["baseUrl"], TEST_URL);
    }

    #[test]
    fn missing_keys_are_added_and_a_file_that_is_not_an_object_is_refused() {
        use super::set_keys;
        let values = [
            ("defaultProvider", serde_json::json!("rayline")),
            ("defaultModel", serde_json::json!("arc-sage-1.0")),
        ];
        assert_eq!(
            set_keys("{\"theme\": \"dark\"}\n", &values).unwrap(),
            "{\n  \"theme\": \"dark\",\n  \"defaultProvider\": \"rayline\",\n  \"defaultModel\": \"arc-sage-1.0\"\n}\n"
        );
        assert_eq!(
            set_keys("{}", &values).unwrap(),
            "{\n  \"defaultProvider\": \"rayline\",\n  \"defaultModel\": \"arc-sage-1.0\"\n}"
        );
        // pi reads the last of repeated keys, so each one is set.
        assert_eq!(
            set_keys(
                "{\"defaultModel\": \"a\", \"defaultModel\": \"b\"}",
                &values[1..]
            )
            .unwrap(),
            "{\n  \"defaultModel\": \"arc-sage-1.0\",\n  \"defaultModel\": \"arc-sage-1.0\"\n}"
        );
        for invalid in ["", "[]", "{\"theme\": }", "{} {}"] {
            assert!(set_keys(invalid, &values).is_err(), "{invalid:?}");
        }

        let dir = super::super::tests::temp_dir("pi-invalid");
        let ctx = super::super::tests::render_context(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), "{\"theme\": ").unwrap();
        let error = super::plan(&ctx).unwrap_err();
        assert!(error.starts_with("cannot update "), "{error}");
        assert!(error.contains("settings.json"), "{error}");
        assert!(error.ends_with("; fix or remove the file"), "{error}");
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
