//! Fetches a model's tokenizer and chat template from Hugging Face, the repo
//! its `models.*.tokenizer` names, for the local process's prompt counter
//! (`--token-counter-dir`). A model with no `tokenizer` is counted by the
//! core.

use std::path::{Path, PathBuf};

const REQUIRED: &[&str] = &[rayline_tokencount::TOKENIZER_FILE];
/// A template ships as its own file or inside `tokenizer_config.json`; a
/// processor config describes image input.
const OPTIONAL: &[&str] = &[
    rayline_tokencount::TEMPLATE_FILE,
    rayline_tokencount::TOKENIZER_CONFIG_FILE,
    rayline_tokencount::PROCESSOR_FILE,
    rayline_tokencount::PREPROCESSOR_FILE,
];

/// A tokenizer snapshot directory, and whether it is the cached copy because
/// Hugging Face did not serve the current one.
pub(crate) struct Snapshot {
    pub(crate) dir: PathBuf,
    pub(crate) cached: bool,
}

/// Downloads the counter's files at the repo's current commit into the
/// Hugging Face cache at `hub`. When the Hub does not answer or a file fails
/// to download, the last complete snapshot is used instead.
pub(crate) fn fetch(hub: &Path, repo: &str) -> Result<Snapshot, String> {
    fetch_with(
        hub,
        repo,
        rayline_hf::hf_api_get_commit(repo),
        |file, commit| rayline_hf::download_to_hf_cache(hub, repo, file, commit).map(drop),
    )
}

/// [`fetch`], given the Hub's answer for the repo's current commit and a
/// `download` of one file at a commit into `hub`.
fn fetch_with(
    hub: &Path,
    repo: &str,
    current_commit: Result<String, String>,
    download: impl Fn(&str, &str) -> Result<(), String>,
) -> Result<Snapshot, String> {
    let (commit, cached_commit) = match current_commit {
        Ok(commit) => (commit, false),
        Err(error) => (cached_commit(hub, repo)?.ok_or(error)?, true),
    };
    // Complete only when every optional file is cached or confirmed absent
    // (404); a rate limit or network failure leaves the snapshot unrecorded.
    let mut failed = None;
    for (file, required) in REQUIRED
        .iter()
        .map(|file| (file, true))
        .chain(OPTIONAL.iter().map(|file| (file, false)))
    {
        if rayline_hf::cached_hf_file(hub, repo, file, &commit).is_some() {
            continue;
        }
        if let Err(error) = download(file, &commit) {
            if required {
                return cached_or(hub, repo, format!("download {file} from {repo}: {error}"));
            }
            if !is_absent(&error) {
                failed.get_or_insert(format!("download {file} from {repo}: {error}"));
            }
        }
    }
    if let Some(error) = failed {
        // A partial snapshot could miscount; use the last complete one.
        return cached_or(hub, repo, error);
    }
    let dir = rayline_hf::hf_cache_snapshot_path(hub, repo, "", &commit);
    // Every candidate was fetched or confirmed absent.
    if !has_chat_template(&dir)? {
        return Err(format!(
            "{repo} publishes no chat template; name a repo that does, such as the model's instruct or chat variant"
        ));
    }
    // `cached_or` uses this snapshot alone.
    let marker = complete_marker(hub, repo);
    std::fs::write(&marker, &commit).map_err(|error| format!("{}: {error}", marker.display()))?;
    Ok(Snapshot {
        dir,
        cached: cached_commit,
    })
}

/// The Hub answered that the file does not exist in the repo.
fn is_absent(error: &str) -> bool {
    error.contains("status 404")
}

/// Names the last snapshot `fetch` finished. `refs/main` moves as each file
/// downloads, so it can name a snapshot still missing optional files.
fn complete_marker(hub: &Path, repo: &str) -> PathBuf {
    hub.join(rayline_hf::repo_to_folder_name(repo))
        .join("rayline-counter-complete")
}

/// The last complete snapshot in `hub`, marked cached, when the Hub did not
/// serve the current one; when `fetch` never finished one, `error`. A marker
/// that cannot be read, or names no commit or an incomplete snapshot, is an
/// error naming it. What `start` uses when the Hub does not answer in time or
/// a download fails.
pub(crate) fn cached_or(hub: &Path, repo: &str, error: String) -> Result<Snapshot, String> {
    let marker = complete_marker(hub, repo);
    let Some(commit) = read_commit(&marker)? else {
        return Err(error);
    };
    if !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{}: not a commit: {commit}", marker.display()));
    }
    let dir = hub
        .join(rayline_hf::repo_to_folder_name(repo))
        .join("snapshots")
        .join(&commit);
    let complete =
        dir.join(rayline_tokencount::TOKENIZER_FILE).is_file() && has_chat_template(&dir)?;
    if !complete {
        return Err(format!(
            "{}: the snapshot {} it names is incomplete",
            marker.display(),
            dir.display()
        ));
    }
    Ok(Snapshot { dir, cached: true })
}

/// Whether the snapshot at `dir` has a chat template where the counter reads
/// one: `chat_template.jinja`, else the `chat_template` key of
/// `tokenizer_config.json`. A config that cannot be read is an error naming it.
fn has_chat_template(dir: &Path) -> Result<bool, String> {
    if dir.join(rayline_tokencount::TEMPLATE_FILE).is_file() {
        return Ok(true);
    }
    let config = dir.join(rayline_tokencount::TOKENIZER_CONFIG_FILE);
    let bytes = match std::fs::read(&config) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("{}: {error}", config.display())),
    };
    let config_json: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", config.display()))?;
    Ok(config_json.get("chat_template").is_some())
}

/// The commit `refs/main` names in the Hugging Face cache at `hub`.
fn cached_commit(hub: &Path, repo: &str) -> Result<Option<String>, String> {
    read_commit(
        &hub.join(rayline_hf::repo_to_folder_name(repo))
            .join("refs")
            .join("main"),
    )
}

/// The commit `file` names, `None` when it does not exist. An unreadable or
/// empty file is an error naming it.
fn read_commit(file: &Path) -> Result<Option<String>, String> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", file.display())),
    };
    match text.trim() {
        "" => Err(format!("{}: empty", file.display())),
        commit => Ok(Some(commit.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "example/model";

    fn temp_hub(name: &str) -> PathBuf {
        let hub =
            std::env::temp_dir().join(format!("rayline-tokenizer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&hub);
        std::fs::create_dir_all(&hub).unwrap();
        hub
    }

    #[test]
    fn without_a_finished_snapshot_the_download_error_stands() {
        let hub = temp_hub("none");
        let error = "download tokenizer.json from example/model: status 500".to_owned();
        assert_eq!(
            cached_or(&hub, REPO, error.clone()).err(),
            Some(error.clone())
        );
        let _ = std::fs::remove_dir_all(&hub);
    }

    #[test]
    fn a_bad_marker_is_an_error_naming_it() {
        let hub = temp_hub("bad-marker");
        let snapshot = hub
            .join(rayline_hf::repo_to_folder_name(REPO))
            .join("snapshots")
            .join("abc123");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join(rayline_tokencount::TOKENIZER_FILE), "{}").unwrap();
        let marker = complete_marker(&hub, REPO);
        let cached = || cached_or(&hub, REPO, "unused".to_owned()).err().unwrap();
        // A marked snapshot that lacks its chat template is incomplete.
        std::fs::write(&marker, "abc123").unwrap();
        assert_eq!(
            cached(),
            format!(
                "{}: the snapshot {} it names is incomplete",
                marker.display(),
                snapshot.display()
            )
        );
        std::fs::write(&marker, "\n").unwrap();
        assert_eq!(cached(), format!("{}: empty", marker.display()));
        std::fs::write(&marker, "../x").unwrap();
        assert_eq!(
            cached(),
            format!("{}: not a commit: ../x", marker.display())
        );
        let _ = std::fs::remove_dir_all(&hub);
    }

    #[test]
    fn a_complete_cached_snapshot_is_used_and_marked_cached() {
        let hub = temp_hub("complete");
        let snapshot = hub
            .join(rayline_hf::repo_to_folder_name(REPO))
            .join("snapshots")
            .join("abc123");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join(rayline_tokencount::TOKENIZER_FILE), "{}").unwrap();
        std::fs::write(snapshot.join(rayline_tokencount::TEMPLATE_FILE), "").unwrap();
        std::fs::write(complete_marker(&hub, REPO), "abc123\n").unwrap();
        let used = cached_or(&hub, REPO, "unused".to_owned()).unwrap();
        assert_eq!(used.dir, snapshot);
        assert!(used.cached);
        let _ = std::fs::remove_dir_all(&hub);
    }

    const RATE_LIMITED: &str = "Download failed with status 429 Too Many Requests";
    const NOT_FOUND: &str = "Download failed with status 404 Not Found";

    /// A Hub that serves the tokenizer, fails the chat template with
    /// `template_error`, and has none of the other optional files.
    fn hub_serving<'a>(
        hub: &'a Path,
        template_error: &'static str,
    ) -> impl Fn(&str, &str) -> Result<(), String> + 'a {
        move |file, commit| {
            if file == rayline_tokencount::TEMPLATE_FILE {
                return Err(template_error.to_owned());
            }
            if file != rayline_tokencount::TOKENIZER_FILE {
                return Err(NOT_FOUND.to_owned());
            }
            let path = rayline_hf::hf_cache_snapshot_path(hub, REPO, file, commit);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "{}").map_err(|error| error.to_string())
        }
    }

    /// A finished snapshot at `abc123` with a tokenizer and a chat template.
    fn cache_complete_snapshot(hub: &Path) -> PathBuf {
        let snapshot = hub
            .join(rayline_hf::repo_to_folder_name(REPO))
            .join("snapshots")
            .join("abc123");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join(rayline_tokencount::TOKENIZER_FILE), "{}").unwrap();
        std::fs::write(snapshot.join(rayline_tokencount::TEMPLATE_FILE), "").unwrap();
        std::fs::write(complete_marker(hub, REPO), "abc123\n").unwrap();
        snapshot
    }

    fn fetch_current(hub: &Path, template_error: &'static str) -> Result<Snapshot, String> {
        fetch_with(
            hub,
            REPO,
            Ok("def456".to_owned()),
            hub_serving(hub, template_error),
        )
    }

    #[test]
    fn a_failed_template_download_uses_the_complete_cached_snapshot() {
        let hub = temp_hub("template-failed-cached");
        let snapshot = cache_complete_snapshot(&hub);
        let used = fetch_current(&hub, RATE_LIMITED).unwrap();
        assert_eq!(used.dir, snapshot);
        assert!(used.cached);
        let _ = std::fs::remove_dir_all(&hub);
    }

    #[test]
    fn a_failed_template_download_without_a_cached_snapshot_is_the_download_error() {
        let hub = temp_hub("template-failed-uncached");
        assert_eq!(
            fetch_current(&hub, RATE_LIMITED).err(),
            Some(format!(
                "download {} from {REPO}: {RATE_LIMITED}",
                rayline_tokencount::TEMPLATE_FILE
            ))
        );
        let _ = std::fs::remove_dir_all(&hub);
    }

    const NO_TEMPLATE: &str = "example/model publishes no chat template; name a repo that does, such as the model's instruct or chat variant";

    #[test]
    fn a_repo_with_no_template_candidate_publishes_no_chat_template() {
        // Every candidate was fetched and none exists, so a cached snapshot
        // does not stand in.
        let hub = temp_hub("no-template");
        cache_complete_snapshot(&hub);
        assert_eq!(
            fetch_current(&hub, NOT_FOUND).err().as_deref(),
            Some(NO_TEMPLATE)
        );
        let _ = std::fs::remove_dir_all(&hub);
    }

    /// A Hub that serves `tokenizer.json` and `tokenizer_config.json` with
    /// `config`, and has none of the other optional files.
    fn hub_with_config<'a>(
        hub: &'a Path,
        config: &'static str,
    ) -> impl Fn(&str, &str) -> Result<(), String> + 'a {
        move |file, commit| {
            let body = match file {
                rayline_tokencount::TOKENIZER_FILE => "{}",
                rayline_tokencount::TOKENIZER_CONFIG_FILE => config,
                _ => return Err(NOT_FOUND.to_owned()),
            };
            let path = rayline_hf::hf_cache_snapshot_path(hub, REPO, file, commit);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).map_err(|error| error.to_string())
        }
    }

    #[test]
    fn a_tokenizer_config_without_a_chat_template_publishes_no_chat_template() {
        // A base model's config: special tokens, no `chat_template`.
        let hub = temp_hub("config-no-template");
        let fetched = fetch_with(
            &hub,
            REPO,
            Ok("def456".to_owned()),
            hub_with_config(&hub, r#"{"bos_token": "<bos>"}"#),
        );
        assert_eq!(fetched.err().as_deref(), Some(NO_TEMPLATE));
        assert!(!complete_marker(&hub, REPO).exists());
        let _ = std::fs::remove_dir_all(&hub);
    }

    #[test]
    fn a_chat_template_in_the_tokenizer_config_is_used() {
        let hub = temp_hub("config-template");
        let fetched = fetch_with(
            &hub,
            REPO,
            Ok("def456".to_owned()),
            hub_with_config(&hub, r#"{"chat_template": "{{ messages }}"}"#),
        )
        .unwrap();
        assert!(!fetched.cached);
        assert_eq!(
            std::fs::read_to_string(complete_marker(&hub, REPO)).unwrap(),
            "def456"
        );
        let _ = std::fs::remove_dir_all(&hub);
    }

    #[test]
    fn a_marked_snapshot_without_a_chat_template_is_incomplete() {
        // A snapshot marked complete before its template was checked.
        let hub = temp_hub("marked-no-template");
        let snapshot = cache_complete_snapshot(&hub);
        std::fs::remove_file(snapshot.join(rayline_tokencount::TEMPLATE_FILE)).unwrap();
        std::fs::write(
            snapshot.join(rayline_tokencount::TOKENIZER_CONFIG_FILE),
            r#"{"bos_token": "<bos>"}"#,
        )
        .unwrap();
        assert_eq!(
            cached_or(&hub, REPO, "unused".to_owned()).err(),
            Some(format!(
                "{}: the snapshot {} it names is incomplete",
                complete_marker(&hub, REPO).display(),
                snapshot.display()
            ))
        );
        let _ = std::fs::remove_dir_all(&hub);
    }

    #[test]
    fn only_a_missing_file_counts_as_absent() {
        assert!(is_absent("Download failed with status 404 Not Found"));
        assert!(!is_absent(
            "Download failed with status 429 Too Many Requests"
        ));
        assert!(!is_absent("error sending request: connection reset"));
    }
}
