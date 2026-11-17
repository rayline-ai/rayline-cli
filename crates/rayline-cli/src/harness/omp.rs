//! omp (oh-my-pi): `PI_CODING_AGENT_DIR=<dir>` holds `models.yml` and
//! `config.yml`. A named profile (`OMP_PROFILE`/`PI_PROFILE`) makes omp ignore
//! that override, so both are removed. omp also reads Claude Code's config from
//! `CLAUDE_CONFIG_DIR` (MCP servers, skills); in testing, a
//! `CLAUDE_CONFIG_DIR` inherited from `rayline run claude` pulling another config
//! into omp, so it is removed too. Header values name env vars; `apiKey` is the
//! literal key.
//!
//! omp sends its session id as `X-Claude-Code-Session-Id`, which the gateway
//! keys the episode on. In `auth: apiKey` mode it does so from 18.1.10; an
//! older omp gets a warning, since each of its turns is then a new episode.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::json;

use super::{Plan, RenderContext, merged_file, to_yaml, yaml_file, yaml_merge};

pub(crate) const PROVIDER_ID: &str = "rayline";
/// First omp that sends `X-Claude-Code-Session-Id` in `auth: apiKey` mode.
pub(crate) const MIN_SESSION_HEADER_VERSION: (u64, u64, u64) = (18, 1, 10);

/// Warn when `omp --version` reports a version that sends no session id in
/// apiKey mode. Silent when the version cannot be read.
pub(crate) fn warn_if_old(binary: &Path) {
    let mut command = Command::new(binary);
    crate::codex::withhold_router_key(&mut command);
    let Ok(output) = command
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return;
    };
    if let Some(warning) = version_warning(&String::from_utf8_lossy(&output.stdout)) {
        eprintln!("{warning}");
    }
}

/// The warning for `omp --version` output (`omp/18.5.0`), if it is too old.
pub(crate) fn version_warning(version_output: &str) -> Option<String> {
    let version = parse_version(version_output)?;
    (version < MIN_SESSION_HEADER_VERSION).then(|| {
        let (major, minor, patch) = MIN_SESSION_HEADER_VERSION;
        format!(
            "warning: omp {}.{}.{} sends no session id with an API key, so every turn is a \
             separate Rayline episode. Upgrade to omp {major}.{minor}.{patch} or later.",
            version.0, version.1, version.2
        )
    })
}

/// The first `X.Y.Z` in `text`.
pub(super) fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    text.split(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .find_map(|token| {
            let mut parts = token.split('.').map(|part| part.parse::<u64>().ok());
            match (parts.next(), parts.next(), parts.next()) {
                (Some(Some(major)), Some(Some(minor)), Some(Some(patch))) => {
                    Some((major, minor, patch))
                }
                _ => None,
            }
        })
}

/// The `config.yml` keys Rayline sets on every launch.
const ROLES: &[&[&str]] = &[
    &["modelRoles", "default"],
    &["modelRoles", "smol"],
    &["modelRoles", "slow"],
    &["modelRoles", "plan"],
];

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Result<Plan, String> {
    let launch = &ctx.models;
    let entries: Vec<_> = launch
        .offered
        .iter()
        .map(|model| {
            let hosted_arc = model.product.is_some();
            let mut entry = json!({
                "id": model.id,
                // ARC owns reasoning effort; do not expose omp thinking controls.
                "reasoning": !hosted_arc
            });
            super::add_details(&mut entry, model);
            // USD / million tokens, from the router's price card. A model
            // without a row there (hybrid's virtual model) shows no cost.
            if let Some(price) = model.price {
                entry["cost"] = json!({
                    "input": price.input,
                    "output": price.output,
                    "cacheRead": price.cache_read,
                    "cacheWrite": price.cache_write
                });
            }
            entry
        })
        .collect();
    let models = json!({
        "providers": {
            PROVIDER_ID: {
                "baseUrl": ctx.messages_base(),
                "apiKey": ctx.key,
                "api": "anthropic-messages",
                // Without `auth: apiKey` omp poses as Claude Code (renamed tools,
                // billing system block). apiKey mode is the honest request shape.
                "auth": "apiKey",
                "models": entries
            }
        }
    });
    // omp also saves its own settings to config.yml, so each launch sets only
    // Rayline's roles ([`ROLES`]) there and keeps every other line as omp
    // wrote it. `smol` is omp's small/fast role (side calls, quick subagent
    // work).
    let main = format!("{PROVIDER_ID}/{}", launch.main);
    let config = json!({
        "modelRoles": {
            "default": main,
            "smol": format!("{PROVIDER_ID}/{}", launch.background),
            "slow": main,
            "plan": main
        }
    });
    let config_file = merged_file(ctx.config_dir, "config.yml", to_yaml(&config), |existing| {
        yaml_merge::merge(existing, &config, ROLES)
    })?;
    Ok(Plan {
        files: vec![yaml_file("models.yml", &models, true), config_file],
        env_set: vec![("PI_CODING_AGENT_DIR", ctx.config_dir.as_os_str().to_owned())],
        env_remove: vec!["CLAUDE_CONFIG_DIR", "OMP_PROFILE", "PI_PROFILE"],
    })
}

#[cfg(test)]
mod tests {
    use super::super::Harness;
    use super::super::tests::{env_value, render};

    #[test]
    fn renders_api_key_mode_and_clears_claude_config_dir() {
        let (dir, plan) = render(Harness::Omp);
        assert_eq!(
            env_value(&plan, "PI_CODING_AGENT_DIR"),
            Some(&dir.clone().into_os_string())
        );
        assert!(plan.env_remove.contains(&"CLAUDE_CONFIG_DIR"));
        let config = std::fs::read_to_string(dir.join("config.yml")).unwrap();
        assert!(config.starts_with("modelRoles:\n"));
        for role in ["default", "slow", "plan"] {
            assert!(config.contains(&format!("  {role}: \"rayline/arc-sage-1.0\"\n")));
        }
        assert!(config.contains("  smol: \"rayline/arc-swift-1.0\"\n"));
    }

    #[test]
    fn each_launch_sets_rayline_s_roles_and_keeps_omp_s_settings() {
        let dir = super::super::tests::temp_dir("omp-saved");
        let ctx = super::super::tests::render_context(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.yml");
        // As omp writes it (Bun's YAML.stringify), with roles picked in omp.
        let saved = "\
lastChangelogVersion: 18.21.0
modelRoles:
  default: other/model-a
  commit: other/model-b
  smol: rayline/rayline-router
theme:
  dark: titanium
enabledModels:
  - rayline/arc-sage-1.0
  - a:b
disabledExtensions: []
note: \"line one\\nline two\"
";
        std::fs::write(&config, saved).unwrap();
        super::super::write_files(&dir, &super::plan(&ctx).unwrap().files).unwrap();
        let expected = "\
lastChangelogVersion: 18.21.0
modelRoles:
  default: \"rayline/arc-sage-1.0\"
  commit: other/model-b
  smol: \"rayline/arc-swift-1.0\"
  slow: \"rayline/arc-sage-1.0\"
  plan: \"rayline/arc-sage-1.0\"
theme:
  dark: titanium
enabledModels:
  - rayline/arc-sage-1.0
  - a:b
disabledExtensions: []
note: \"line one\\nline two\"
";
        assert_eq!(std::fs::read_to_string(&config).unwrap(), expected);

        // A file the merge cannot split is refused, not rewritten.
        std::fs::write(&config, "theme: {dark: titanium}\n").unwrap();
        let error = super::plan(&ctx).unwrap_err();
        assert!(error.starts_with("cannot update "), "{error}");
        assert!(error.contains("config.yml"), "{error}");
    }

    #[test]
    fn hosted_picker_excludes_legacy_but_hybrid_keeps_its_virtual_model() {
        let dir = super::super::tests::temp_dir("omp-model-roster");
        let mut ctx = super::super::tests::render_context(&dir);
        let hosted = super::plan(&ctx).unwrap();
        assert!(!hosted.files[0].contents.contains("id: \"rayline-auto\""));
        let capabilities = rayline_local_router::ModelCapabilities {
            context_window: 16384,
            max_output_tokens: 8192,
            input_modalities: vec!["text".into()],
        };
        ctx.capabilities = Some(&capabilities);
        ctx.models = super::super::LaunchModels::hybrid(&capabilities);
        let hybrid = super::plan(&ctx).unwrap();
        assert!(hybrid.files[0].contents.contains("id: \"rayline-auto\""));
        assert!(!hybrid.files[0].contents.contains("id: \"arc-sage-1.0\""));
    }

    #[test]
    fn warns_below_18_1_10_only() {
        use super::version_warning;
        let old = version_warning("omp/18.0.5\n").expect("18.0.5 warns");
        assert!(
            old.contains("omp 18.0.5") && old.contains("18.1.10"),
            "{old}"
        );
        assert!(version_warning("omp 18.1.9").is_some());
        assert_eq!(version_warning("omp/18.1.10"), None);
        assert_eq!(version_warning("omp/18.5.0"), None);
        assert_eq!(version_warning("omp/19.0.0"), None);
        assert_eq!(version_warning("no version here"), None);
    }
}
