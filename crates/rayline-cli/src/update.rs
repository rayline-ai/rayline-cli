use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::UpdateChannel;

const CORE_LICENSE_NAME: &str = "rayline-core-LICENSE";
/// The local ARC encoder, fetched into `~/.rayline/arc/<version>/` by the
/// first `local` decisions start of that version.
const ENCODER_NAME: &str = "rayline-arc-encoder";
const ENCODER_LICENSE_NAME: &str = "rayline-arc-encoder-LICENSE";
/// The platform whose encoder loads DLLs from its own directory: every
/// `.dll` that `SHA256SUMS` lists (cuBLAS) is fetched beside it.
const ENCODER_DLL_PLATFORM: &str = "windows_x86_64";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateRequest {
    pub pinned_version: Option<Version>,
    pub force: bool,
    pub check_only: bool,
    pub dry_run: bool,
    /// Hidden `--base-url`: where releases are read from.
    pub base_url: Option<Url>,
    /// Hidden `--install-path`: which binary is replaced.
    pub install_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateResult {
    pub message: String,
    pub exit_code: u8,
    pub stderr: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateError {
    Checksum(String),
    InvalidVersion(String),
    Install(String),
    /// A Windows install step whose file another process still held open
    /// after `move_into_place` waited for it.
    InUse(String),
    Network(String),
    Signature(String),
    UnsupportedPlatform(String),
}

struct UpdateArtifacts {
    launcher: PathBuf,
    core: PathBuf,
    license: PathBuf,
    /// The signed SHA256SUMS of the release, for the encoder.
    sums: String,
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Checksum(message)
            | Self::Install(message)
            | Self::InUse(message)
            | Self::Network(message)
            | Self::Signature(message)
            | Self::UnsupportedPlatform(message) => formatter.write_str(message),
            Self::InvalidVersion(version) => write!(formatter, "invalid --version '{version}'"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// Maps an io error to an install error that names the failed operation and
/// the path it was on.
pub(crate) fn io_failed<'a>(
    operation: &'a str,
    path: &'a Path,
) -> impl FnOnce(io::Error) -> UpdateError + 'a {
    move |error| UpdateError::Install(format!("failed to {operation} {}: {error}", path.display()))
}

/// Maps an io error to an install error that names the failed operation and
/// both of its paths.
fn io_failed_between<'a>(
    operation: &'a str,
    from: &'a Path,
    to: &'a Path,
) -> impl FnOnce(io::Error) -> UpdateError + 'a {
    move |error| {
        UpdateError::Install(format!(
            "failed to {operation} {} to {}: {error}",
            from.display(),
            to.display()
        ))
    }
}

/// `rayline update` on `channel`, the loaded `update.channel`.
pub async fn run(
    request: &UpdateRequest,
    channel: UpdateChannel,
) -> Result<UpdateResult, UpdateError> {
    let home = crate::home().map_err(|error| UpdateError::Install(error.to_string()))?;
    run_on(
        request,
        channel,
        &home,
        crate::RELEASES_URL,
        crate::MINISIGN_PUBLIC_KEYS,
    )
    .await
}

/// The pointer file naming the latest release on `channel`.
fn latest_pointer(channel: UpdateChannel) -> &'static str {
    match channel {
        UpdateChannel::Prod => "cli/latest.txt",
        UpdateChannel::Dev => "cli/latest-dev.txt",
        UpdateChannel::Main => "cli/latest-main.txt",
    }
}

/// `run` with the GitHub release downloads a bare `--version` is resolved
/// from, and the keys release signatures are checked against.
async fn run_on(
    request: &UpdateRequest,
    channel: UpdateChannel,
    home: &Path,
    releases_url: &str,
    pubkeys: &[&str],
) -> Result<UpdateResult, UpdateError> {
    let base_url = request
        .base_url
        .as_ref()
        .map_or(crate::UPDATE_BASE_URL, Url::as_str)
        .trim_end_matches('/');
    let install_path = install_path(request.install_path.as_deref())?;
    if is_homebrew_managed(&install_path)? {
        return Ok(UpdateResult {
            message: format!(
                "Installed via Homebrew. Run `brew upgrade {}` to update.\n",
                crate::CLI_BIN
            ),
            exit_code: 0,
            stderr: false,
        });
    }
    let platform_tag = detect_platform_tag()?;
    let current = Version::parse(crate::RAYLINE_VERSION)
        .map_err(|_| UpdateError::InvalidVersion(crate::RAYLINE_VERSION.to_owned()))?;
    let target = match &request.pinned_version {
        Some(version) => full_version(releases_url, version, &current, pubkeys).await?,
        None => fetch_latest_version(base_url, channel, pubkeys).await?,
    };

    let decision = evaluate_check(
        &current,
        &target,
        channel,
        request.pinned_version.is_some(),
        request.force,
    );
    if request.check_only {
        return Ok(decision);
    }
    if decision.exit_code == 0 {
        return Ok(decision);
    }

    // Removed however the update ends.
    let temp_dir = TempDir(temp_update_dir()?);
    let temp_dir = temp_dir.0.as_path();
    let artifacts =
        download_and_verify(base_url, &target, &current, platform_tag, temp_dir, pubkeys).await;
    // The encoder is refreshed only for a user who already fetched one.
    let encoder_dir = has_encoder(&arc_dir(home)).then(|| encoder_dir(home, &target));
    match artifacts {
        Ok(artifacts) => {
            if request.dry_run {
                Ok(UpdateResult {
                    message: format!(
                        "Would install {}, {} and {}{}.\n",
                        artifacts.launcher.display(),
                        artifacts.core.display(),
                        artifacts.license.display(),
                        encoder_dir.as_ref().map_or(String::new(), |dir| format!(
                            ", and the ARC encoder into {}",
                            dir.display()
                        ))
                    ),
                    exit_code: 0,
                    stderr: false,
                })
            } else {
                let core_install_path = install_path
                    .with_file_name(format!("rayline-core{}", std::env::consts::EXE_SUFFIX));
                let license_install_path = core_install_path.with_file_name(CORE_LICENSE_NAME);
                #[cfg(windows)]
                if let Err(error) = cleanup_retired_backups(&install_path) {
                    eprintln!(
                        "Could not finish old update backup cleanup: {error}. Continuing installation; a later update will retry."
                    );
                }
                // The new version's encoder first, so a failed fetch leaves
                // this version installed with its own.
                if let Some(dir) = &encoder_dir {
                    fetch_encoder_with_sums(
                        base_url,
                        &target,
                        platform_tag,
                        &artifacts.sums,
                        dir,
                        &Report::SILENT,
                    )
                    .await?;
                }
                replace_file(&artifacts.license, &license_install_path, false)?;
                replace_binary(&artifacts.core, &core_install_path)?;
                replace_binary(&artifacts.launcher, &install_path)?;
                // A running local process restarts its encoder from its own
                // version's directory, and a starting one may have just
                // fetched its own: other versions go only while none runs or
                // starts, else at the next start.
                if let Some(dir) = &encoder_dir {
                    crate::hybrid::while_no_local_process(home, || remove_other_encoders(dir));
                }
                Ok(UpdateResult {
                    message: format!(
                        "Updated {} {} \u{2192} {}.\n",
                        crate::DISPLAY_NAME,
                        current.public,
                        target.public
                    ),
                    exit_code: 0,
                    stderr: false,
                })
            }
        }
        Err(error) => Err(error),
    }
}

/// A directory removed when dropped.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `version` with its build label. Releases are published under their full
/// version (`cli/v0.2.12+ee1afb00b6cd/`). A version without one is the
/// running version, `current`, when it names it; otherwise it is resolved
/// from its GitHub release's signed `latest.txt`, which must name the same
/// version.
async fn full_version(
    releases_url: &str,
    version: &Version,
    current: &Version,
    pubkeys: &[&str],
) -> Result<Version, UpdateError> {
    if version.normalized.contains('+') {
        return Ok(version.clone());
    }
    if version.public == current.public {
        return Ok(current.clone());
    }
    let url = format!("{releases_url}/v{}/latest.txt", version.public);
    let not_found = |error| match error {
        UpdateError::Network(message) if message.ends_with("HTTP 404") => {
            UpdateError::Network(format!("release {} does not exist", version.public))
        }
        error => error,
    };
    let pointer = fetch_pointer_bytes(&url).await.map_err(not_found)?;
    let signature = fetch_pointer_bytes(&format!("{url}.minisig")).await?;
    let full = parse_verified_latest(&pointer, &signature, pubkeys)?;
    if full.public != version.public {
        return Err(UpdateError::Network(format!(
            "release {} names version {}",
            version.public, full.normalized
        )));
    }
    Ok(full)
}

/// What `rayline update` decides for `target`, the version asked for
/// (`explicit_intent`) or else `channel`'s latest.
pub fn evaluate_check(
    current: &Version,
    target: &Version,
    channel: UpdateChannel,
    explicit_intent: bool,
    force: bool,
) -> UpdateResult {
    let update_available = if explicit_intent {
        target.normalized != current.normalized || force
    } else {
        // `--force` reinstalls the channel's latest, never one older than the
        // running version: a replayed older `latest.txt` can't roll back.
        match target.public_cmp(current) {
            Ordering::Greater => true,
            Ordering::Equal => force,
            Ordering::Less => false,
        }
    };

    if update_available {
        return UpdateResult {
            message: format!(
                "Update available: {} \u{2192} {}. Run: {} update\n",
                current.public,
                target.public,
                crate::CLI_BIN
            ),
            exit_code: 1,
            stderr: false,
        };
    }

    let message = if !explicit_intent && target.public_cmp(current) == Ordering::Less {
        format!(
            "Already up to date: {} is newer than the latest {} release ({}). To install that release instead, run: {} update --version {}\n",
            current.public,
            channel.as_str(),
            target.normalized,
            crate::CLI_BIN,
            target.normalized
        )
    } else {
        format!("Already on latest ({}).\n", current.public)
    };
    UpdateResult {
        message,
        exit_code: 0,
        stderr: false,
    }
}

async fn fetch_latest_version(
    base_url: &str,
    channel: UpdateChannel,
    pubkeys: &[&str],
) -> Result<Version, UpdateError> {
    let url = format!("{base_url}/{}", latest_pointer(channel));
    let pointer = fetch_pointer_bytes(&url).await?;
    // Fail closed: the version pointer must carry a verifiable signature before
    // we trust the version it names. This prevents an attacker from FORGING a
    // pointer to an arbitrary or never-released version. It does NOT, on its
    // own, defeat replay/freeze: an attacker who can serve content may replay an
    // older, validly-signed pointer. The `target > current` check in
    // `evaluate_check` blocks rolling BELOW the installed version, but full
    // anti-rollback/freshness (a signed timestamp+expiry or highest-seen-version
    // floor) is tracked as a follow-up — see docs/release.md.
    let signature = fetch_pointer_bytes(&format!("{url}.minisig")).await?;
    parse_verified_latest(&pointer, &signature, pubkeys)
}

async fn fetch_pointer_bytes(url: &str) -> Result<Vec<u8>, UpdateError> {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|error| UpdateError::Network(format!("failed to fetch {url}: {error}")))?
        .get(url)
        .send()
        .await
        .map_err(|error| UpdateError::Network(format!("failed to fetch {url}: {error}")))?;
    if !response.status().is_success() {
        return Err(UpdateError::Network(format!(
            "{url} returned HTTP {}",
            response.status().as_u16()
        )));
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| UpdateError::Network(format!("failed to fetch {url}: {error}")))
}

/// Verify the latest-pointer bytes against a pinned key, then parse the version.
///
/// Fails closed: an absent/invalid signature, non-UTF-8 bytes, or an
/// unparseable version is an error. The signature is over the pointer's raw
/// bytes, so a tampered version string no longer matches.
fn parse_verified_latest(
    pointer: &[u8],
    signature: &[u8],
    pubkeys: &[&str],
) -> Result<Version, UpdateError> {
    verify_signature(pointer, signature, pubkeys)?;
    let text = std::str::from_utf8(pointer)
        .map_err(|_| UpdateError::Network("latest pointer is not valid UTF-8".to_owned()))?
        .trim();
    Version::parse(text).map_err(|_| {
        UpdateError::Network(format!("could not parse version from pointer: {text:?}"))
    })
}

fn artifact_url_for(base_url: &str, version: &str, platform_tag: &str, name: &str) -> String {
    format!("{base_url}/cli/v{version}/{name}-{platform_tag}")
}

fn checksums_url_for(base_url: &str, version: &str) -> String {
    format!("{base_url}/cli/v{version}/SHA256SUMS")
}

fn checksums_sig_url_for(base_url: &str, version: &str) -> String {
    format!("{base_url}/cli/v{version}/SHA256SUMS.minisig")
}

/// The first release whose SHA256SUMS signature names its version in the
/// trusted comment (`version:<full version>`).
const FIRST_VERSION_SIGNED_SUMS: [u64; 3] = [0, 5, 10];

/// Verify that `sums` is the signed SHA256SUMS of release `target`: signed by
/// one of `pubkeys`, with `version:<target>` in the trusted comment.
///
/// A release before 0.5.10 is signed without the field. One without it is
/// accepted only when `target` is older than both 0.5.10 and `running`, the
/// version this check runs in: an explicit `--version` downgrade. An update
/// (newer than `running`) and the encoder check (`running` itself) always
/// need the field, so a host can't serve an older release under their
/// version.
pub(crate) fn verify_release_sums(
    sums: &[u8],
    sig_bytes: &[u8],
    pubkeys: &[&str],
    target: &Version,
    running: &Version,
) -> Result<(), UpdateError> {
    let signature = verify_signature(sums, sig_bytes, pubkeys)?;
    let signed: Vec<&str> = signature
        .trusted_comment()
        .split('\t')
        .filter_map(|field| field.strip_prefix("version:"))
        .collect();
    match signed.as_slice() {
        [version] if *version == target.normalized => Ok(()),
        [] if target.base < FIRST_VERSION_SIGNED_SUMS
            && target.public_cmp(running) == Ordering::Less =>
        {
            Ok(())
        }
        [] => Err(UpdateError::Signature(format!(
            "SHA256SUMS for {} is signed without its version \u{2014} refusing to install",
            target.normalized
        ))),
        _ => Err(UpdateError::Signature(format!(
            "SHA256SUMS for {} is signed for version {} \u{2014} refusing to install",
            target.normalized,
            signed.join(", ")
        ))),
    }
}

/// Verify that `sums` is signed by at least one of the pinned `pubkeys`, and
/// return the signature.
///
/// Fails closed: returns `Err` if `sig_bytes` is empty, malformed, or no
/// pinned key validates the signature. The caller must call this BEFORE
/// trusting any checksum from `sums`.
pub fn verify_signature(
    sums: &[u8],
    sig_bytes: &[u8],
    pubkeys: &[&str],
) -> Result<Signature, UpdateError> {
    if sig_bytes.is_empty() {
        return Err(UpdateError::Signature(
            "SHA256SUMS.minisig is empty — refusing to install unsigned update".to_owned(),
        ));
    }

    let sig =
        Signature::decode(std::str::from_utf8(sig_bytes).map_err(|_| {
            UpdateError::Signature("SHA256SUMS.minisig is not valid UTF-8".to_owned())
        })?)
        .map_err(|error| {
            UpdateError::Signature(format!("malformed SHA256SUMS.minisig: {error}"))
        })?;

    for pubkey_str in pubkeys {
        let pk = PublicKey::from_base64(pubkey_str).map_err(|error| {
            UpdateError::Signature(format!("invalid pinned public key: {error}"))
        })?;
        if pk.verify(sums, &sig, true).is_ok() {
            return Ok(sig);
        }
    }

    Err(UpdateError::Signature(
        "SHA256SUMS signature could not be verified against any pinned public key — \
         refusing to install"
            .to_owned(),
    ))
}

pub(crate) fn detect_platform_tag() -> Result<&'static str, UpdateError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("macosx_11_0_arm64"),
        ("macos", "x86_64") => Ok("macosx_10_12_x86_64"),
        ("linux", "x86_64") => Ok("linux_x86_64"),
        ("linux", "aarch64") => Ok("linux_aarch64"),
        ("windows", "x86_64") => Ok("windows_x86_64"),
        (os, arch) => Err(UpdateError::UnsupportedPlatform(format!(
            "unsupported platform: {os}-{arch}. Supported: linux_aarch64, linux_x86_64, macosx_10_12_x86_64, macosx_11_0_arm64, windows_x86_64",
        ))),
    }
}

/// Downloads the release's SHA256SUMS and its signature into `dest_dir` and
/// returns SHA256SUMS once [`verify_release_sums`] accepts it for `target`
/// from `running`.
async fn fetch_verified_sums(
    base_url: &str,
    target: &Version,
    running: &Version,
    dest_dir: &Path,
    pubkeys: &[&str],
) -> Result<String, UpdateError> {
    let version = target.normalized.as_str();
    fs::create_dir_all(dest_dir).map_err(io_failed("create", dest_dir))?;
    let sums_path = dest_dir.join("SHA256SUMS");
    let sig_path = dest_dir.join("SHA256SUMS.minisig");

    // Download checksums and its signature first so we can verify before
    // trusting any artifact bytes.
    download_to(&checksums_url_for(base_url, version), &sums_path).await?;
    download_to(&checksums_sig_url_for(base_url, version), &sig_path).await?;

    // Verify signature BEFORE reading or trusting the checksums file.
    // Fail closed: missing or invalid signature aborts the update.
    let sums_bytes = fs::read(&sums_path).map_err(io_failed("read", &sums_path))?;
    let sig_bytes = fs::read(&sig_path).map_err(io_failed("read", &sig_path))?;
    verify_release_sums(&sums_bytes, &sig_bytes, pubkeys, target, running)?;

    String::from_utf8(sums_bytes)
        .map_err(|_| UpdateError::Checksum("SHA256SUMS contains invalid UTF-8".to_owned()))
}

async fn download_and_verify(
    base_url: &str,
    target: &Version,
    running: &Version,
    platform_tag: &str,
    dest_dir: &Path,
    pubkeys: &[&str],
) -> Result<UpdateArtifacts, UpdateError> {
    let sums = fetch_verified_sums(base_url, target, running, dest_dir, pubkeys).await?;
    let version = target.normalized.as_str();
    let launcher_name = format!("{}-{platform_tag}", crate::CLI_ASSET);
    let launcher_path = dest_dir.join(&launcher_name);
    let core_name = format!("rayline-core-{platform_tag}");
    let core_path = dest_dir.join(&core_name);
    let license_path = dest_dir.join(CORE_LICENSE_NAME);

    download_to(
        &artifact_url_for(base_url, version, platform_tag, crate::CLI_ASSET),
        &launcher_path,
    )
    .await?;
    download_to(
        &artifact_url_for(base_url, version, platform_tag, "rayline-core"),
        &core_path,
    )
    .await?;
    download_to(
        &format!("{base_url}/cli/v{version}/{CORE_LICENSE_NAME}"),
        &license_path,
    )
    .await?;

    verify_checksum(&sums, &launcher_name, &launcher_path)?;
    verify_checksum(&sums, &core_name, &core_path)?;
    verify_checksum(&sums, CORE_LICENSE_NAME, &license_path)?;

    Ok(UpdateArtifacts {
        launcher: launcher_path,
        core: core_path,
        license: license_path,
        sums,
    })
}

/// `~/.rayline/arc`: the ARC encoder of each version that fetched one.
pub(crate) fn arc_dir(home: &Path) -> PathBuf {
    home.join(crate::DOT_CONFIG_DIR).join("arc")
}

/// Where version `version`'s encoder lives.
pub(crate) fn encoder_dir(home: &Path, version: &Version) -> PathBuf {
    arc_dir(home).join(&version.normalized)
}

/// The encoder's file name in its directory.
pub(crate) fn encoder_file_name() -> String {
    format!("{ENCODER_NAME}{}", std::env::consts::EXE_SUFFIX)
}

/// Whether any version directory in `arc` holds an encoder.
fn has_encoder(arc: &Path) -> bool {
    fs::read_dir(arc).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.path().join(encoder_file_name()).is_file())
    })
}

/// Removes the other versions' encoders beside `keep`: every entry of its
/// parent but `keep` and hidden ones (downloads in progress). One that
/// can't be removed stays for the next time.
pub(crate) fn remove_other_encoders(keep: &Path) {
    let (Some(arc), Some(name)) = (keep.parent(), keep.file_name()) else {
        return;
    };
    for entry in fs::read_dir(arc).into_iter().flatten().flatten() {
        let entry_name = entry.file_name();
        if entry_name != name && !entry_name.to_string_lossy().starts_with('.') {
            let path = entry.path();
            let _ = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
        }
    }
}

/// The files of the encoder `sums` lists for `platform_tag`: each as
/// (name in SHA256SUMS, file name in the encoder's directory), the encoder
/// last. An error when the release has no encoder for the platform.
pub(crate) fn encoder_files(
    sums: &str,
    platform_tag: &str,
) -> Result<Vec<(String, String)>, UpdateError> {
    let asset = format!("{ENCODER_NAME}-{platform_tag}");
    expected_sha256(sums, &asset)?;
    let mut files = vec![(
        ENCODER_LICENSE_NAME.to_owned(),
        ENCODER_LICENSE_NAME.to_owned(),
    )];
    if platform_tag == ENCODER_DLL_PLATFORM {
        files.extend(
            listed_dlls(sums)
                .into_iter()
                .map(|dll| (dll.to_owned(), dll.to_owned())),
        );
    }
    files.push((asset, encoder_file_name()));
    Ok(files)
}

/// Version `version`'s encoder already fetched into `dir`: its SHA256SUMS
/// is signed for `version` by one of `pubkeys`, and `matches` accepts each
/// file it lists for `platform_tag` (given the path and its listed sha256).
/// `None` when anything is missing or doesn't match.
pub(crate) fn fetched_encoder(
    dir: &Path,
    version: &Version,
    platform_tag: &str,
    pubkeys: &[&str],
    matches: &dyn Fn(&Path, &str) -> bool,
) -> Option<PathBuf> {
    let sums = fs::read(dir.join("SHA256SUMS")).ok()?;
    let signature = fs::read(dir.join("SHA256SUMS.minisig")).ok()?;
    verify_release_sums(&sums, &signature, pubkeys, version, version).ok()?;
    fetched_files(dir, &String::from_utf8(sums).ok()?, platform_tag, matches)
}

/// [`fetched_encoder`] once `sums` is trusted.
fn fetched_files(
    dir: &Path,
    sums: &str,
    platform_tag: &str,
    matches: &dyn Fn(&Path, &str) -> bool,
) -> Option<PathBuf> {
    let files = encoder_files(sums, platform_tag).ok()?;
    files
        .iter()
        .all(|(listed, name)| {
            expected_sha256(sums, listed).is_ok_and(|sha| matches(&dir.join(name), &sha))
        })
        .then(|| dir.join(encoder_file_name()))
}

/// What a download reports: each file's length when the server gives it,
/// its bytes so far, and its end.
pub(crate) struct Report<'a> {
    pub on_len: &'a dyn Fn(u64),
    pub on_bytes: &'a dyn Fn(u64),
    pub on_done: &'a dyn Fn(),
}

impl Report<'_> {
    /// Reports nothing.
    pub(crate) const SILENT: Report<'static> = Report {
        on_len: &|_| {},
        on_bytes: &|_| {},
        on_done: &|| {},
    };
}

/// Fetches version `version`'s encoder for `platform_tag` from its release
/// under `base_url` into `dir`, with the release's SHA256SUMS checked
/// against `pubkeys`, telling `report`.
pub(crate) async fn fetch_encoder(
    base_url: &str,
    version: &Version,
    platform_tag: &str,
    dir: &Path,
    pubkeys: &[&str],
    report: &Report<'_>,
) -> Result<(), UpdateError> {
    let staging = staging_dir(dir)?;
    let result = async {
        let sums = fetch_verified_sums(base_url, version, version, &staging, pubkeys).await?;
        fetch_encoder_into(
            base_url,
            version,
            platform_tag,
            &sums,
            &staging,
            dir,
            report,
        )
        .await
    }
    .await;
    let _ = fs::remove_dir_all(&staging);
    result
}

/// [`fetch_encoder`] with the release's SHA256SUMS already checked, as an
/// update has them.
async fn fetch_encoder_with_sums(
    base_url: &str,
    version: &Version,
    platform_tag: &str,
    sums: &str,
    dir: &Path,
    report: &Report<'_>,
) -> Result<(), UpdateError> {
    let staging = staging_dir(dir)?;
    let result = async {
        // The signed manifest beside the encoder, which each start checks.
        let sums_path = staging.join("SHA256SUMS");
        fs::write(&sums_path, sums).map_err(io_failed("write", &sums_path))?;
        download_to(
            &checksums_sig_url_for(base_url, &version.normalized),
            &staging.join("SHA256SUMS.minisig"),
        )
        .await?;
        fetch_encoder_into(base_url, version, platform_tag, sums, &staging, dir, report).await
    }
    .await;
    let _ = fs::remove_dir_all(&staging);
    result
}

/// A new staging directory beside `dir`, on its filesystem, so its files
/// move into `dir` by rename.
fn staging_dir(dir: &Path) -> Result<PathBuf, UpdateError> {
    let parent = dir
        .parent()
        .ok_or_else(|| UpdateError::Install(format!("{} has no parent", dir.display())))?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| UpdateError::Install(format!("system clock error: {error}")))?
        .as_nanos();
    let staging = parent.join(format!(".fetch-{}-{unique}", std::process::id()));
    fs::create_dir_all(&staging).map_err(io_failed("create", &staging))?;
    Ok(staging)
}

/// Downloads into `staging` each encoder file `sums` lists that `dir` lacks
/// or holds with another sha256, checks it, then moves it into `dir` (the
/// encoder last, executable on Unix), and the signed SHA256SUMS after them.
async fn fetch_encoder_into(
    base_url: &str,
    version: &Version,
    platform_tag: &str,
    sums: &str,
    staging: &Path,
    dir: &Path,
    report: &Report<'_>,
) -> Result<(), UpdateError> {
    let files = encoder_files(sums, platform_tag)?;
    let mut fetched = Vec::new();
    for (listed, name) in &files {
        if installed_matches(sums, listed, &dir.join(name))? {
            continue;
        }
        let downloaded = staging.join(listed);
        download_to_with(
            &format!("{base_url}/cli/v{}/{listed}", version.normalized),
            &downloaded,
            report.on_len,
            report.on_bytes,
        )
        .await?;
        (report.on_done)();
        verify_checksum(sums, listed, &downloaded)?;
        #[cfg(unix)]
        if *name == encoder_file_name() {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&downloaded, fs::Permissions::from_mode(0o755))
                .map_err(io_failed("set the mode of", &downloaded))?;
        }
        fetched.push((downloaded, dir.join(name)));
    }
    fs::create_dir_all(dir).map_err(io_failed("create", dir))?;
    for name in ["SHA256SUMS", "SHA256SUMS.minisig"] {
        fetched.push((staging.join(name), dir.join(name)));
    }
    for (from, to) in &fetched {
        fs::rename(from, to).map_err(io_failed_between("move", from, to))?;
    }
    Ok(())
}

/// The `.dll` files `sums` lists: plain file names, without a path.
fn listed_dlls(sums: &str) -> Vec<&str> {
    sums.lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(|name| name.trim_start_matches('*'))
        .filter(|name| {
            name.to_ascii_lowercase().ends_with(".dll")
                && !name.contains(['/', '\\'])
                && !name.starts_with('.')
        })
        .collect()
}

/// Whether `installed` exists with the sha256 `sums` lists for `name`. A
/// release that lists the encoder but not `name` fails: it can't be verified.
fn installed_matches(sums: &str, name: &str, installed: &Path) -> Result<bool, UpdateError> {
    let expected = expected_sha256(sums, name)?;
    if !installed.is_file() {
        return Ok(false);
    }
    Ok(sha256_file(installed)?.eq_ignore_ascii_case(&expected))
}

pub(crate) async fn download_to(url: &str, path: &Path) -> Result<(), UpdateError> {
    download_to_with(url, path, &|_| {}, &|_| {}).await
}

/// [`download_to`], reporting the response's length when it gives one
/// (`on_len`) and the bytes written so far (`on_bytes`).
async fn download_to_with(
    url: &str,
    path: &Path,
    on_len: &dyn Fn(u64),
    on_bytes: &dyn Fn(u64),
) -> Result<(), UpdateError> {
    let part = path.with_extension("part");
    // No overall deadline: the encoder is large and a link may be slow. A stalled connection
    // still fails, on connect or when a read waits too long.
    let mut response = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| UpdateError::Network(format!("download failed for {url}: {error}")))?
        .get(url)
        .send()
        .await
        .map_err(|error| UpdateError::Network(format!("download failed for {url}: {error}")))?;
    if !response.status().is_success() {
        return Err(UpdateError::Network(format!(
            "GET {url} returned HTTP {}",
            response.status().as_u16()
        )));
    }
    // The header itself: over HTTP/2 the body's size hint may not carry it.
    if let Some(len) = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
    {
        on_len(len);
    }
    // Streamed to disk, so a large file is never held in memory.
    let mut file = fs::File::create(&part).map_err(io_failed("write", &part))?;
    let mut written = 0;
    on_bytes(written);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| UpdateError::Network(format!("download failed for {url}: {error}")))?
    {
        file.write_all(&chunk).map_err(io_failed("write", &part))?;
        written += chunk.len() as u64;
        on_bytes(written);
    }
    file.sync_all().map_err(io_failed("write", &part))?;
    drop(file);
    fs::rename(&part, path).map_err(io_failed_between("move", &part, path))?;
    Ok(())
}

fn expected_sha256(sums: &str, artifact_name: &str) -> Result<String, UpdateError> {
    for line in sums.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let mut parts = stripped.split_whitespace();
        let Some(digest) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        if name.trim().trim_start_matches('*') == artifact_name {
            return Ok(digest.to_owned());
        }
    }
    Err(UpdateError::Checksum(format!(
        "no sha256 entry for {artifact_name} in SHA256SUMS"
    )))
}

pub(crate) fn verify_checksum(
    sums: &str,
    artifact_name: &str,
    path: &Path,
) -> Result<(), UpdateError> {
    let expected = expected_sha256(sums, artifact_name)?;
    let actual = sha256_file(path)?;
    if expected != actual {
        let _ = fs::remove_file(path);
        return Err(UpdateError::Checksum(format!(
            "sha256 mismatch for {artifact_name}: expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

/// Hashed in chunks, so a large file (such as the encoder) is never
/// held in memory.
fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let mut file = fs::File::open(path).map_err(io_failed("read", path))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).map_err(io_failed("read", path))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// The binary to replace: `--install-path` as given (the parser has checked
/// it names `rayline`), else the running binary.
fn install_path(option: Option<&Path>) -> Result<PathBuf, UpdateError> {
    match option {
        Some(path) => resolve_install_path(path.to_path_buf()),
        None => {
            let running = std::env::current_exe().map_err(|error| {
                UpdateError::Install(format!("failed to locate the running executable: {error}"))
            })?;
            resolve_install_path(running)
        }
    }
}

/// `current_exe()` may return the invoking symlink rather than its target on
/// some platforms (macOS). Self-update must replace the real binary, not the
/// symlink. A path that does not exist yet
/// (an `--install-path` naming a file to create) passes through unchanged;
/// any other failure to resolve it is an error naming the path.
fn resolve_install_path(path: PathBuf) -> Result<PathBuf, UpdateError> {
    match fs::canonicalize(&path) {
        Ok(resolved) => Ok(resolved),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path),
        Err(error) => Err(io_failed("resolve", &path)(error)),
    }
}

/// True when `install_path` is under Homebrew's prefix. Without `brew` on
/// `PATH` nothing is Homebrew-managed; a `brew --prefix` that fails is an
/// error.
pub(crate) fn is_homebrew_managed(install_path: &Path) -> Result<bool, UpdateError> {
    Ok(match command_on_path("brew") {
        Some(brew) => path_is_under_prefix(install_path, &brew_prefix(&brew)?),
        None => false,
    })
}

fn path_is_under_prefix(install_path: &Path, prefix: &Path) -> bool {
    let install_path =
        fs::canonicalize(install_path).unwrap_or_else(|_| install_path.to_path_buf());
    let prefix = fs::canonicalize(prefix).unwrap_or_else(|_| prefix.to_path_buf());
    install_path.starts_with(prefix)
}

/// The prefix `brew --prefix` prints, run with the `brew` at `brew`.
fn brew_prefix(brew: &Path) -> Result<PathBuf, UpdateError> {
    let failed = |reason: String| {
        UpdateError::Install(format!(
            "`brew --prefix` ({}) failed: {reason}",
            brew.display()
        ))
    };
    let output = Command::new(brew)
        .arg("--prefix")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| failed(error.to_string()))?;
    if !output.status.success() {
        return Err(failed(output.status.to_string()));
    }
    let prefix = String::from_utf8(output.stdout)
        .map_err(|_| failed("it printed a prefix that is not UTF-8".to_owned()))?;
    match prefix.trim() {
        "" => Err(failed("it printed no prefix".to_owned())),
        prefix => Ok(PathBuf::from(prefix)),
    }
}

pub(crate) fn replace_binary(downloaded: &Path, install_path: &Path) -> Result<(), UpdateError> {
    replace_file(downloaded, install_path, true)
}

fn replace_file(
    downloaded: &Path,
    install_path: &Path,
    executable: bool,
) -> Result<(), UpdateError> {
    let parent = install_path.parent().ok_or_else(|| {
        UpdateError::Install(format!(
            "install path has no parent directory: {}",
            install_path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        UpdateError::Install(format!(
            "failed to create install directory {}: {error}",
            parent.display()
        ))
    })?;

    let staged = staged_install_path(install_path)?;
    let mut source = fs::File::open(downloaded).map_err(|error| stage_failed(&staged, error))?;
    let staged_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .map_err(|error| stage_failed(&staged, error))?;
    if let Err(error) = fill_staged(&mut source, staged_file, &staged, executable) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }

    let result = move_into_place(&staged, install_path);
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

/// Copy `source`'s bytes into the new `staged_file`, then give it the install
/// permissions. A plain byte copy: on macOS `fs::copy` first tries an APFS
/// clone, which can fail with `EINVAL`.
fn fill_staged(
    source: &mut fs::File,
    mut staged_file: fs::File,
    staged: &Path,
    executable: bool,
) -> Result<(), UpdateError> {
    #[cfg(not(unix))]
    let _ = executable;
    io::copy(source, &mut staged_file).map_err(|error| stage_failed(staged, error))?;
    drop(staged_file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 });
        fs::set_permissions(staged, permissions).map_err(|error| {
            UpdateError::Install(format!(
                "failed to set update permissions at {}: {error}",
                staged.display()
            ))
        })?;
    }
    Ok(())
}

fn stage_failed(staged: &Path, error: io::Error) -> UpdateError {
    UpdateError::Install(format!(
        "failed to stage update at {}: {error}",
        staged.display()
    ))
}

/// Atomically move the staged binary onto `install_path`.
///
/// On Unix `rename(2)` replaces the target even while it is running. Windows
/// locks a running `.exe` and rejects renaming over it, but it *does* allow
/// renaming the running file out of the way — so we move the current binary to
/// a sidecar path first, then move the update into place, restoring the
/// original if the second move fails. On Windows every rename waits briefly
/// while another process holds the file ([`rename_when_released`]).
#[cfg(not(windows))]
pub(crate) fn move_into_place(staged: &Path, install_path: &Path) -> Result<(), UpdateError> {
    fs::rename(staged, install_path).map_err(|error| {
        UpdateError::Install(format!(
            "failed to replace {}: {error}",
            install_path.display()
        ))
    })
}

#[cfg(windows)]
pub(crate) fn move_into_place(staged: &Path, install_path: &Path) -> Result<(), UpdateError> {
    if !install_path.exists() {
        return rename_when_released(staged, install_path).map_err(|error| {
            install_failure(
                &error,
                format!("failed to install {}: {error}", install_path.display()),
            )
        });
    }

    let backup = staged_install_path(install_path)?;
    rename_when_released(install_path, &backup).map_err(|error| {
        install_failure(
            &error,
            format!(
                "failed to move existing binary {} aside: {error}",
                install_path.display()
            ),
        )
    })?;

    match rename_when_released(staged, install_path) {
        Ok(()) => {
            retire_replaced_backup(&backup, install_path);
            Ok(())
        }
        Err(error) => {
            // Restore the original so a failed update never leaves a hole.
            rename_when_released(&backup, install_path).map_err(|restore_error| {
                UpdateError::Install(format!(
                    "failed to replace {}: {error}; restore also failed: {restore_error}. Original preserved at {}",
                    install_path.display(),
                    backup.display()
                ))
            })?;
            Err(install_failure(
                &error,
                format!("failed to replace {}: {error}", install_path.display()),
            ))
        }
    }
}

/// How long a Windows rename waits for another process to close the file.
/// Antivirus and indexing services open a newly written or newly started
/// executable for a moment without delete sharing.
#[cfg(windows)]
const HELD_FILE_WAIT: Duration = Duration::from_secs(2);

/// Windows' sharing and lock violations: another process has the file open.
#[cfg(windows)]
fn held_by_another_process(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32 | 33))
}

/// `fs::rename`, retried with a growing pause for up to [`HELD_FILE_WAIT`]
/// while another process holds the file. Any other error, or a hold that
/// outlasts the wait, is returned as it is.
#[cfg(windows)]
fn rename_when_released(from: &Path, to: &Path) -> io::Result<()> {
    let deadline = std::time::Instant::now() + HELD_FILE_WAIT;
    let mut pause = Duration::from_millis(10);
    loop {
        match fs::rename(from, to) {
            Err(error)
                if held_by_another_process(&error) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(200));
            }
            result => return result,
        }
    }
}

#[cfg(windows)]
fn install_failure(error: &io::Error, message: String) -> UpdateError {
    if held_by_another_process(error) {
        UpdateError::InUse(message)
    } else {
        UpdateError::Install(message)
    }
}

/// Mark a committed replacement's old file for a later update to delete. The
/// running image is normally still locked now; retirement is only housekeeping
/// and must not stop installation of the remaining companion binaries.
#[cfg(any(windows, test))]
fn retire_replaced_backup(backup: &Path, install_path: &Path) {
    let result = sidecar_install_path(install_path, "old").and_then(|retired| {
        fs::rename(backup, &retired).map_err(io_failed_between("move", backup, &retired))
    });
    if let Err(error) = result {
        eprintln!(
            "Updated {}; retained previous file at {} because it could not be marked for cleanup: {error}. Remove it manually after older sessions and installers exit.",
            install_path.display(),
            backup.display()
        );
    }
}

pub(crate) fn staged_install_path(install_path: &Path) -> Result<PathBuf, UpdateError> {
    sidecar_install_path(install_path, "tmp")
}

fn sidecar_install_path(install_path: &Path, kind: &str) -> Result<PathBuf, UpdateError> {
    let parent = install_path.parent().ok_or_else(|| {
        UpdateError::Install(format!(
            "install path has no parent directory: {}",
            install_path.display()
        ))
    })?;
    let file_name = install_path.file_name().ok_or_else(|| {
        UpdateError::Install(format!(
            "install path has no file name: {}",
            install_path.display()
        ))
    })?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| UpdateError::Install(format!("system clock error: {error}")))?
        .as_nanos();
    let mut staged_name = OsString::from(".");
    staged_name.push(file_name);
    staged_name.push(format!(".{kind}-{}-{unique}", std::process::id()));
    Ok(parent.join(staged_name))
}

/// Before an actual update, delete backups from previous successful Windows swaps.
/// Historical .tmp files also name active staging and rollback files, so those
/// require manual cleanup after all installers and Rayline CLI sessions have exited.
#[cfg(any(windows, test))]
fn cleanup_retired_backups(install_path: &Path) -> Result<(), UpdateError> {
    let parent = install_path.parent().ok_or_else(|| {
        UpdateError::Install(format!(
            "install path has no parent: {}",
            install_path.display()
        ))
    })?;
    for entry in fs::read_dir(parent).map_err(|error| {
        UpdateError::Install(format!(
            "cannot inspect update backups in {}: {error}",
            parent.display()
        ))
    })? {
        let entry = entry.map_err(io_failed("list update backups in", parent))?;
        if entry.file_name().to_str().is_some_and(is_retired_backup) {
            match entry.file_type() {
                Ok(kind) if kind.is_file() => remove_retired_backup(&entry.path())?,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_failed("inspect", &entry.path())(error)),
            }
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn is_retired_backup(name: &str) -> bool {
    let Some((installed, suffix)) = name
        .strip_prefix('.')
        .and_then(|rest| rest.split_once(".old-"))
    else {
        return false;
    };
    let ours = [
        "rayline.exe",
        "rayline-core.exe",
        CORE_LICENSE_NAME,
        "rayline-arc-encoder.exe",
        ENCODER_LICENSE_NAME,
    ]
    .contains(&installed)
        || installed.to_ascii_lowercase().ends_with(".dll");
    ours && suffix.split_once('-').is_some_and(|(pid, nonce)| {
        !pid.is_empty()
            && pid.bytes().all(|b| b.is_ascii_digit())
            && !nonce.is_empty()
            && nonce.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

#[cfg(any(windows, test))]
fn remove_retired_backup(path: &Path) -> Result<(), UpdateError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        // Another cleanup may have removed the same committed backup.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error)
            if error.kind() == io::ErrorKind::PermissionDenied
                || (cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33))) =>
        {
            eprintln!(
                "Retained old executable {}: {error}. Close older Rayline CLI sessions; a later update or installer will retry cleanup.",
                path.display()
            );
            Ok(())
        }
        Err(error) => Err(UpdateError::Install(format!(
            "cannot remove retired update backup {}: {error}",
            path.display()
        ))),
    }
}

fn command_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn temp_update_dir() -> Result<PathBuf, UpdateError> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| UpdateError::Install(format!("system clock error: {error}")))?
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "{}-update-{}-{unique}",
        crate::CLI_BIN,
        std::process::id()
    ));
    fs::create_dir_all(&path).map_err(io_failed("create", &path))?;
    Ok(path)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Version {
    normalized: String,
    public: String,
    base: [u64; 3],
    prerelease: Option<(Prerelease, u64)>,
}

/// The pre-release labels releases use, ordered low to high. `main` sits below
/// `dev` because the release ladder is main (rawest, rolling) -> dev (release
/// candidate) -> prod: a main build must never be reported as newer than the
/// dev build it is promoted into. `rc` is a public release candidate.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Prerelease {
    Main,
    Dev,
    Rc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VersionParseError;

impl Version {
    /// `X.Y.Z`, optionally followed by `-<main|dev|rc>.<N>` and `+<build>`.
    /// Every number is a decimal without leading zeros that fits a u64.
    pub fn parse(raw: &str) -> Result<Self, VersionParseError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(VersionParseError);
        }
        let (public, local) = trimmed.split_once('+').unwrap_or((trimmed, ""));
        if public.is_empty() || trimmed.matches('+').count() > 1 {
            return Err(VersionParseError);
        }
        if trimmed.contains('+') && local.is_empty() {
            return Err(VersionParseError);
        }
        if !local.is_empty() {
            validate_version_label(local)?;
        }

        let (base, prerelease) = match public.split_once('-') {
            Some((base, prerelease)) => (base, Some(parse_prerelease(prerelease)?)),
            None => (public, None),
        };

        Ok(Self {
            normalized: trimmed.to_ascii_lowercase(),
            public: public.to_owned(),
            base: parse_base_version(base)?,
            prerelease,
        })
    }

    /// A pre-release sorts below the release it leads up to.
    fn public_cmp(&self, other: &Self) -> Ordering {
        self.base
            .cmp(&other.base)
            .then_with(|| match (self.prerelease, other.prerelease) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(left), Some(right)) => left.cmp(&right),
            })
    }
}

/// Exactly `X.Y.Z`.
fn parse_base_version(base: &str) -> Result<[u64; 3], VersionParseError> {
    let mut parts = base.split('.');
    let base = [
        parse_version_number(parts.next())?,
        parse_version_number(parts.next())?,
        parse_version_number(parts.next())?,
    ];
    match parts.next() {
        Some(_) => Err(VersionParseError),
        None => Ok(base),
    }
}

/// `<main|dev|rc>.<N>`, one spelling per label.
fn parse_prerelease(prerelease: &str) -> Result<(Prerelease, u64), VersionParseError> {
    let (label, number) = prerelease.split_once('.').ok_or(VersionParseError)?;
    let label = match label {
        "main" => Prerelease::Main,
        "dev" => Prerelease::Dev,
        "rc" => Prerelease::Rc,
        _ => return Err(VersionParseError),
    };
    Ok((label, parse_version_number(Some(number))?))
}

/// A decimal without leading zeros that fits a u64.
fn parse_version_number(number: Option<&str>) -> Result<u64, VersionParseError> {
    let number = number.ok_or(VersionParseError)?;
    if number.is_empty()
        || !number.bytes().all(|byte| byte.is_ascii_digit())
        || (number.len() > 1 && number.starts_with('0'))
    {
        return Err(VersionParseError);
    }
    number.parse::<u64>().map_err(|_| VersionParseError)
}

fn validate_version_label(label: &str) -> Result<(), VersionParseError> {
    if label.is_empty()
        || !label.chars().any(|ch| ch.is_ascii_alphanumeric())
        || !label
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
    {
        return Err(VersionParseError);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    #[test]
    fn a_missing_install_path_passes_through_and_other_resolve_failures_name_it() {
        let root = crate::test_dir::TestDir::new("rayline-resolve");
        let missing = root.join("missing").join("rayline");
        assert_eq!(resolve_install_path(missing.clone()).unwrap(), missing);
        // A regular file used as a directory (unix) or a name the file system
        // rejects (Windows) fails for a reason other than absence.
        #[cfg(unix)]
        let unusable = {
            fs::write(root.join("file"), "").unwrap();
            root.join("file").join("rayline")
        };
        #[cfg(windows)]
        let unusable = root.join("bad<name").join("rayline");
        let error = resolve_install_path(unusable.clone())
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(&format!("failed to resolve {}: ", unusable.display())),
            "{error}"
        );
    }

    #[test]
    fn a_failing_brew_prefix_is_an_error_naming_it() {
        let root = crate::test_dir::TestDir::new("rayline-brew");
        let missing = root.join("no-such-brew");
        let error = brew_prefix(&missing).unwrap_err().to_string();
        assert!(
            error.starts_with(&format!("`brew --prefix` ({}) failed: ", missing.display())),
            "{error}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = |name: &str, body: &str| {
                let path = root.join(name);
                fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
                path
            };
            let exits = script("exits", "exit 3");
            assert_eq!(
                brew_prefix(&exits).unwrap_err().to_string(),
                format!(
                    "`brew --prefix` ({}) failed: exit status: 3",
                    exits.display()
                )
            );
            let silent = script("silent", "echo");
            assert_eq!(
                brew_prefix(&silent).unwrap_err().to_string(),
                format!(
                    "`brew --prefix` ({}) failed: it printed no prefix",
                    silent.display()
                )
            );
            let prints = script("prints", "echo /opt/homebrew");
            assert_eq!(
                brew_prefix(&prints).unwrap(),
                PathBuf::from("/opt/homebrew")
            );
        }
    }

    #[test]
    fn an_io_failure_names_the_operation_and_path() {
        let path = std::env::temp_dir().join("rayline-update-no-such-file");
        let error = sha256_file(&path).unwrap_err().to_string();
        assert!(
            error.starts_with(&format!("failed to read {}: ", path.display())),
            "{error}"
        );
    }

    #[test]
    fn cleanup_removes_only_committed_windows_backups() -> TestResult {
        let root = crate::test_dir::TestDir::new("rayline-update-test");
        let retired = [
            ".rayline.exe.old-123-456",
            ".rayline.exe.old-123-abcdef",
            ".rayline-core.exe.old-123-456",
            ".rayline-core-LICENSE.old-123-456",
        ];
        let retained = [
            "rayline.exe",
            ".rayline.exe.tmp-123-456",
            ".rayline-core.exe.tmp-123-456",
            ".rayline.exe.old-manual-backup",
            ".unrelated.exe.old-123-456",
            "rayline.exe.old-0123456789abcdef0123456789abcdef",
            "rayline.exe.old-incomplete",
        ];
        for name in retired.iter().chain(retained.iter()) {
            fs::write(root.join(name), b"payload")?;
        }
        let installed = root.join("rayline.exe");
        let backup = staged_install_path(&installed)?;
        fs::write(&backup, b"previous executable")?;
        retire_replaced_backup(&backup, &installed);
        assert!(!backup.exists());
        let entries = fs::read_dir(&root)?.collect::<io::Result<Vec<_>>>()?;
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.file_name().to_str().is_some_and(is_retired_backup))
                .count(),
            retired.len() + 1,
            "retirement must keep the old file until a later sweep"
        );
        let directory = root.join(".rayline-core.exe.old-123-789");
        fs::create_dir(&directory)?;
        cleanup_retired_backups(&root.join("rayline.exe"))?;
        for name in retired {
            assert!(!root.join(name).exists(), "retired file {name}");
        }
        for name in retained {
            assert_eq!(fs::read(root.join(name))?, b"payload", "retained {name}");
        }
        assert_eq!(fs::read_dir(&root)?.count(), retained.len() + 1);
        assert!(directory.is_dir());
        root.remove()?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_cleanup_retries_locked_retired_backup() -> TestResult {
        use std::os::windows::fs::OpenOptionsExt;

        let root = crate::test_dir::TestDir::new("rayline-update-test");
        let backup = root.join(".rayline.exe.old-123-456");
        fs::write(&backup, b"old executable")?;
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&backup)?;
        cleanup_retired_backups(&root.join("rayline.exe"))?;
        assert!(backup.exists());
        drop(locked);
        cleanup_retired_backups(&root.join("rayline.exe"))?;
        assert!(!backup.exists());
        root.remove()?;
        Ok(())
    }

    /// Opens `path` the way a scanner does: readable, but without delete
    /// sharing, so renaming it fails with a sharing violation while open.
    #[cfg(windows)]
    fn hold_without_delete_sharing(path: &Path) -> io::Result<fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
    }

    #[cfg(windows)]
    #[test]
    fn windows_swap_waits_for_another_process_to_release_the_file() -> TestResult {
        let root = crate::test_dir::TestDir::new("rayline-update-test");
        let installed = root.join("rayline.exe");
        let staged = root.join("staged.exe");
        fs::write(&installed, b"original")?;
        fs::write(&staged, b"new")?;
        let held = hold_without_delete_sharing(&installed)?;
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        move_into_place(&staged, &installed)?;
        release.join().expect("release thread");
        assert_eq!(fs::read(&installed)?, b"new");
        assert!(!staged.exists());
        root.remove()?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_fresh_install_waits_for_another_process_to_release_the_file() -> TestResult {
        let root = crate::test_dir::TestDir::new("rayline-update-test");
        let installed = root.join("rayline.exe");
        let staged = root.join("staged.exe");
        fs::write(&staged, b"new")?;
        let held = hold_without_delete_sharing(&staged)?;
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        move_into_place(&staged, &installed)?;
        release.join().expect("release thread");
        assert_eq!(fs::read(&installed)?, b"new");
        assert!(!staged.exists());
        root.remove()?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_swap_held_past_the_wait_is_an_in_use_error_and_changes_nothing() -> TestResult {
        let root = crate::test_dir::TestDir::new("rayline-update-test");
        let installed = root.join("rayline.exe");
        let staged = root.join("staged.exe");
        fs::write(&installed, b"original")?;
        fs::write(&staged, b"new")?;
        let held = hold_without_delete_sharing(&installed)?;
        let Err(UpdateError::InUse(message)) = move_into_place(&staged, &installed) else {
            panic!("a file held past the wait must be an in-use error");
        };
        drop(held);
        assert!(
            message.starts_with(&format!(
                "failed to move existing binary {} aside: ",
                installed.display()
            )),
            "{message}"
        );
        assert!(message.contains("(os error 32)"), "{message}");
        assert_eq!(fs::read(&installed)?, b"original");
        assert_eq!(fs::read(&staged)?, b"new");
        root.remove()?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_swap_defers_cleanup_and_preserves_failed_swap() -> TestResult {
        let root = crate::test_dir::TestDir::new("rayline-update-test");
        let installed = root.join("rayline.exe");
        let staged = root.join("staged.exe");
        fs::write(&installed, b"original")?;
        fs::write(&staged, b"new")?;
        move_into_place(&staged, &installed)?;
        assert_eq!(fs::read_dir(&root)?.count(), 2);
        assert_eq!(fs::read(&installed)?, b"new");
        assert!(move_into_place(&root.join("missing-staged.exe"), &installed).is_err());
        cleanup_retired_backups(&installed)?;
        assert_eq!(fs::read(&installed)?, b"new");
        assert_eq!(fs::read_dir(&root)?.count(), 1);
        root.remove()?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_retirement_failure_preserves_committed_install_and_backup() -> TestResult {
        use std::os::windows::fs::OpenOptionsExt;

        let root = crate::test_dir::TestDir::new("rayline-update-test");
        let installed = root.join("rayline.exe");
        let backup = staged_install_path(&installed)?;
        fs::write(&installed, b"committed")?;
        fs::write(&backup, b"previous")?;
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&backup)?;
        retire_replaced_backup(&backup, &installed);
        assert!(backup.exists());
        assert_eq!(fs::read(&installed)?, b"committed");
        drop(locked);
        assert_eq!(fs::read(&backup)?, b"previous");
        root.remove()?;
        Ok(())
    }

    #[test]
    fn core_license_installs_as_verified_nonexecutable_data() -> TestResult {
        let root = crate::test_dir::TestDir::new("rayline-update-test");
        let source = root.join("rayline-core-LICENSE");
        let destination = root.join("installed/rayline-core-LICENSE");
        let content = b"Pinned license fixture\n";
        fs::write(&source, content)?;
        let sums = format!("{:x}  rayline-core-LICENSE\n", Sha256::digest(content));
        verify_checksum(&sums, "rayline-core-LICENSE", &source)?;
        replace_file(&source, &destination, false)?;
        assert_eq!(fs::read(&destination)?, content);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&destination)?.permissions().mode() & 0o777,
                0o644
            );
        }
        fs::write(&source, b"tampered license")?;
        assert!(verify_checksum(&sums, "rayline-core-LICENSE", &source).is_err());
        assert_eq!(fs::read(&destination)?, content);
        root.remove()?;
        Ok(())
    }

    #[test]
    fn a_failed_staging_leaves_no_staged_file() -> TestResult {
        let root = crate::test_dir::TestDir::new("rayline-update-test");
        let installed = root.join("rayline");
        fs::write(&installed, b"installed")?;
        // A missing source fails before the staged file exists; a directory
        // fails after it is created on Unix and on opening it on Windows.
        let directory = root.join("downloaded-directory");
        fs::create_dir(&directory)?;
        for downloaded in [root.join("no-such-download"), directory] {
            let error = replace_binary(&downloaded, &installed)
                .unwrap_err()
                .to_string();
            assert!(error.starts_with("failed to stage update at "), "{error}");
            let staged: Vec<_> = fs::read_dir(&root)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<Result<_, _>>()?;
            assert!(
                !staged
                    .iter()
                    .any(|name| name.to_string_lossy().starts_with(".rayline.tmp-")),
                "{staged:?}"
            );
            assert_eq!(fs::read(&installed)?, b"installed");
        }
        root.remove()?;
        Ok(())
    }

    #[test]
    fn a_version_newer_than_the_channels_latest_names_that_release() {
        let current = version("0.5.4-dev.176+abc");
        let latest = version("0.5.3+3bc25bc98f31");
        let result = evaluate_check(&current, &latest, UpdateChannel::Prod, false, false);
        assert_eq!(
            result.message,
            "Already up to date: 0.5.4-dev.176 is newer than the latest prod release (0.5.3+3bc25bc98f31). To install that release instead, run: rayline update --version 0.5.3+3bc25bc98f31\n"
        );
        assert_eq!(result.exit_code, 0);
        // The same version, or one asked for, says it is already installed.
        let same = evaluate_check(
            &latest,
            &version("0.5.3+3bc25bc98f31"),
            UpdateChannel::Prod,
            false,
            false,
        );
        assert_eq!(same.message, "Already on latest (0.5.3).\n");
        let asked = evaluate_check(
            &latest,
            &version("0.5.3+3bc25bc98f31"),
            UpdateChannel::Prod,
            true,
            false,
        );
        assert_eq!(asked.message, "Already on latest (0.5.3).\n");
        // Forced, it still names the older release instead of installing it:
        // a replayed older `latest.txt` can't roll back.
        let forced = evaluate_check(&current, &latest, UpdateChannel::Prod, false, true);
        assert_eq!(forced, result);
        // Forced on the latest itself, it reinstalls it.
        let reinstall = evaluate_check(
            &latest,
            &version("0.5.3+3bc25bc98f31"),
            UpdateChannel::Prod,
            false,
            true,
        );
        assert_eq!(reinstall.exit_code, 1);
    }

    fn version(raw: &str) -> Version {
        Version::parse(raw).unwrap_or_else(|_| panic!("test version {raw} parses"))
    }

    #[test]
    fn versions_order_along_the_release_ladder() {
        // Low to high: the main and dev ladder builds, a public release
        // candidate, then the release.
        let ladder = [
            "0.2.9+ee1afb00b6cd",
            "0.3.0-main.7+ee1afb00b6cd",
            "0.3.0-main.100+ee1afb00b6cd",
            "0.3.0-dev.1+ee1afb00b6cd",
            "0.3.0-dev.42+ee1afb00b6cd",
            "0.3.0-rc.1",
            "0.3.0-rc.2",
            "0.3.0+ee1afb00b6cd",
            "0.3.1-main.1+ee1afb00b6cd",
        ];
        for (index, lower) in ladder.iter().enumerate() {
            for higher in &ladder[index + 1..] {
                assert_eq!(
                    version(higher).public_cmp(&version(lower)),
                    Ordering::Greater,
                    "{higher} > {lower}"
                );
                assert_eq!(
                    version(lower).public_cmp(&version(higher)),
                    Ordering::Less,
                    "{lower} < {higher}"
                );
            }
        }
        assert_eq!(
            version("0.2.0-rc.1").public_cmp(&version("0.2.0-rc.1")),
            Ordering::Equal
        );
        assert_eq!(
            version("0.3.0+aaaaaaaaaaaa").public_cmp(&version("0.3.0+bbbbbbbbbbbb")),
            Ordering::Equal,
            "the build label does not order releases"
        );
    }

    #[test]
    fn a_pre_release_is_main_dev_or_rc_with_one_number() {
        for raw in [
            "0.0.0-sigtest1",
            "1.2.3-alpha.1",
            "1.2.3-beta.1",
            "1.2.3-pre.1",
            "1.2.3-post.1",
            "1.2.3-rc",
            "1.2.3-rc1",
            "1.2.3-rc.",
            "1.2.3-rc.01",
            "1.2.3-rc.1.2",
            "1.2.3-rc_1",
            "1.2.3-RC.1",
            "1.2.3-Dev.1",
            "1.2.3rc.1",
            "1.2.3-",
            "1.2.3-rc.18446744073709551616",
        ] {
            assert_eq!(Version::parse(raw), Err(VersionParseError), "{raw}");
        }
        assert_eq!(
            version("1.2.3-rc.18446744073709551615").prerelease,
            Some((Prerelease::Rc, u64::MAX))
        );
        assert_eq!(version("1.2.3-rc.0").prerelease, Some((Prerelease::Rc, 0)));
    }

    #[test]
    fn a_base_version_is_exactly_three_numbers() {
        for raw in [
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.",
            ".1.2",
            "1..2",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "1.2.3a",
            "v1.2.3",
            "18446744073709551616.0.0",
        ] {
            assert_eq!(Version::parse(raw), Err(VersionParseError), "{raw}");
        }
        assert_eq!(version("0.0.0").base, [0, 0, 0]);
        assert_eq!(version("10.20.30").base, [10, 20, 30]);
        assert_eq!(version("18446744073709551615.0.0").base, [u64::MAX, 0, 0]);
    }

    // ── Signature verification tests ─────────────────────────────────────────

    // Test public key (matches the .minisig in tests/fixtures/SHA256SUMS.minisig)
    // Generated for testing only — NOT the production key.
    const TEST_PUBKEY: &str = "RWRrxu13/uJmEyk8KO8zSLW71qzyxIAYFeYo1nde3MpRDaEwS7yVv2U1";

    // Untrusted public key (different keypair, not in the pinned set)
    const UNTRUSTED_PUBKEY: &str = "RWQR3fP7y6Yexmbshr2vDmkWJeKelP8y80oyaUgE9AjMymImLFrUu0Dy";

    // Test public key for the latest.txt pointer fixtures (matches
    // tests/fixtures/latest.txt.minisig). Testing only — NOT the production key.
    const LATEST_TEST_PUBKEY: &str = "RWTORxdK/+7+ayF6kfI5TjZjEMLnbo81yDtE94iGpdP7yJHQ0F+HGm0l";

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {name} unreadable: {e}"))
    }

    /// (a) Valid sig + matching pinned key → Ok
    #[test]
    fn verify_signature_valid_key_accepts() {
        let sums = fixture("SHA256SUMS");
        let sig = fixture("SHA256SUMS.minisig");
        assert!(
            verify_signature(&sums, &sig, &[TEST_PUBKEY]).is_ok(),
            "valid signature must verify against pinned key"
        );
    }

    /// (b) Tampered SHA256SUMS → Err (signature won't match different content)
    #[test]
    fn verify_signature_tampered_content_rejects() {
        let sums = fixture("SHA256SUMS.tampered");
        let sig = fixture("SHA256SUMS.minisig");
        let result = verify_signature(&sums, &sig, &[TEST_PUBKEY]);
        assert!(
            result.is_err(),
            "tampered content must not verify against original signature"
        );
        assert!(matches!(result, Err(UpdateError::Signature(_))));
    }

    /// (c) Signature from an unpinned key → Err
    #[test]
    fn verify_signature_unpinned_key_rejects() {
        let sums = fixture("SHA256SUMS");
        let sig = fixture("SHA256SUMS.minisig");
        // Only the UNTRUSTED key is in the pinned set — it didn't sign this file
        let result = verify_signature(&sums, &sig, &[UNTRUSTED_PUBKEY]);
        assert!(
            result.is_err(),
            "signature under unpinned key must be rejected"
        );
        assert!(matches!(result, Err(UpdateError::Signature(_))));
    }

    /// (d) Empty sig_bytes → Err (fail closed — no .minisig means reject)
    #[test]
    fn verify_signature_empty_sig_fails_closed() {
        let sums = fixture("SHA256SUMS");
        let result = verify_signature(&sums, &[], &[TEST_PUBKEY]);
        assert!(result.is_err(), "absent/empty signature must fail closed");
        let Err(UpdateError::Signature(msg)) = result else {
            panic!("expected Signature error");
        };
        assert!(
            msg.contains("empty"),
            "error message should mention empty: {msg}"
        );
    }

    /// (e) Multiple pinned keys — succeeds when at least one matches
    #[test]
    fn verify_signature_accepts_when_one_of_multiple_keys_matches() {
        let sums = fixture("SHA256SUMS");
        let sig = fixture("SHA256SUMS.minisig");
        // List has the untrusted key first, then the real test key
        assert!(
            verify_signature(&sums, &sig, &[UNTRUSTED_PUBKEY, TEST_PUBKEY]).is_ok(),
            "must succeed when any pinned key matches"
        );
    }

    // ── latest.txt version-pointer verification ──────────────────────────────

    /// Signed pointer + matching pinned key → verifies and parses the version.
    #[test]
    fn latest_pointer_valid_signature_parses() {
        let pointer = fixture("latest.txt");
        let sig = fixture("latest.txt.minisig");
        let version = parse_verified_latest(&pointer, &sig, &[LATEST_TEST_PUBKEY])
            .expect("valid signed pointer should verify and parse");
        assert_eq!(version.public, "99.0.0");
    }

    // Tamper / missing-signature / unpinned-key rejection is covered by the
    // verify_signature tests above; parse_verified_latest delegates to that same
    // function, so those paths are not re-tested against the pointer payload.

    // ── update against a fake release server ─────────────────────────────────

    // Signs tests/fixtures/release-*SHA256SUMS (`release-SHA256SUMS.minisig`
    // and `release-no-core-SHA256SUMS.minisig` with `version:99.0.0`,
    // `.unversioned.minisig` without the field, `.v98.minisig` with
    // `version:98.0.0`) and latest-old.txt. Testing only — NOT the
    // production key.
    const RELEASE_TEST_PUBKEY: &str = "RWRQa4k3QozRjJ+bh4kqT2y92XNLSXAe9OS0J0mfYsfCIHWPFzDAUS2p";
    const TEST_RELEASE: &str = "99.0.0";
    const LAUNCHER: &[u8] = b"fixture launcher\n";
    const CORE: &[u8] = b"fixture core\n";
    const LICENSE: &[u8] = b"fixture license\n";

    /// A fixture as it was signed: a checkout may have turned its LF into CRLF.
    fn signed_fixture(name: &str) -> Vec<u8> {
        let mut bytes = fixture(name);
        bytes.retain(|byte| *byte != b'\r');
        bytes
    }

    type Files = std::collections::HashMap<String, Vec<u8>>;

    /// An HTTP release server on 127.0.0.1 serving `files` by path, recording
    /// every path it is asked for.
    struct FakeReleaseServer {
        base_url: String,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakeReleaseServer {
        fn start(files: Files) -> Self {
            use std::io::{BufRead, Write};

            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake server");
            let base_url = format!("http://{}", listener.local_addr().expect("server address"));
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let log = std::sync::Arc::clone(&requests);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let Ok(reader) = stream.try_clone() else {
                        continue;
                    };
                    let mut reader = io::BufReader::new(reader);
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).is_err() {
                        continue;
                    }
                    let mut header = String::new();
                    while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
                        header.clear();
                    }
                    let path = request_line.split_whitespace().nth(1).unwrap_or("");
                    let (status, body) = match files.get(path) {
                        Some(body) => ("200 OK", body.as_slice()),
                        None => ("404 Not Found", &[][..]),
                    };
                    log.lock().expect("request log").push(path.to_owned());
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(body);
                }
            });
            Self { base_url, requests }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().expect("request log").clone()
        }
    }

    /// Release files for TEST_RELEASE on this platform, with `sums` naming the
    /// SHA256SUMS fixture (signed with RELEASE_TEST_PUBKEY).
    fn release_files(sums: &str) -> Files {
        release_files_signed(TEST_RELEASE, sums, &format!("{sums}.minisig"))
    }

    /// Release files for `release` on this platform: the `sums` fixture with
    /// the signature fixture `signature`.
    fn release_files_signed(release: &str, sums: &str, signature: &str) -> Files {
        let release = format!("/cli/v{release}");
        let mut files = Files::from([
            (format!("{release}/SHA256SUMS"), signed_fixture(sums)),
            (
                format!("{release}/SHA256SUMS.minisig"),
                signed_fixture(signature),
            ),
            (format!("{release}/{CORE_LICENSE_NAME}"), LICENSE.to_vec()),
        ]);
        let platform = detect_platform_tag().expect("supported test platform");
        for (name, bytes) in [(crate::CLI_ASSET, LAUNCHER), ("rayline-core", CORE)] {
            files.insert(format!("{release}/{name}-{platform}"), bytes.to_vec());
        }
        files
    }

    /// GitHub release downloads whose `latest.txt` names TEST_RELEASE.
    fn releases() -> FakeReleaseServer {
        let pointer = format!("/v{TEST_RELEASE}/latest.txt");
        FakeReleaseServer::start(Files::from([
            (
                format!("{pointer}.minisig"),
                signed_fixture("latest.txt.minisig"),
            ),
            (pointer, signed_fixture("latest.txt")),
        ]))
    }

    /// The keys of the test release's SHA256SUMS and its `latest.txt`.
    const KEYS: &[&str] = &[RELEASE_TEST_PUBKEY, LATEST_TEST_PUBKEY];

    /// For a run that resolves no version.
    const UNUSED_RELEASES: &str = "http://127.0.0.1:9";

    /// A home directory without `.rayline`: no encoder to refresh.
    fn no_home() -> PathBuf {
        std::env::temp_dir().join("rayline-update-test-no-home")
    }

    fn exe(dir: &Path, name: &str) -> PathBuf {
        dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
    }

    /// An install directory holding only `rayline`, and a request that
    /// replaces it from `server`.
    fn install_from(
        server: &FakeReleaseServer,
    ) -> TestResult<(crate::test_dir::TestDir, UpdateRequest)> {
        let install = crate::test_dir::TestDir::new("rayline-update-test");
        let installed = exe(&install, crate::CLI_BIN);
        fs::write(&installed, b"installed")?;
        let request = UpdateRequest {
            pinned_version: Some(version(TEST_RELEASE)),
            force: false,
            check_only: false,
            dry_run: false,
            base_url: Some(Url::parse(&format!("{}/", server.base_url)).unwrap()),
            install_path: Some(installed),
        };
        Ok((install, request))
    }

    #[tokio::test]
    async fn check_reads_the_channel_pointer_from_the_base_url() -> TestResult {
        for (channel, pointer) in [
            (UpdateChannel::Prod, "/cli/latest.txt"),
            (UpdateChannel::Dev, "/cli/latest-dev.txt"),
            (UpdateChannel::Main, "/cli/latest-main.txt"),
        ] {
            let server = FakeReleaseServer::start(Files::from([
                (pointer.to_owned(), signed_fixture("latest.txt")),
                (
                    format!("{pointer}.minisig"),
                    signed_fixture("latest.txt.minisig"),
                ),
            ]));
            let (install, request) = install_from(&server)?;
            fs::write(exe(&install, "rayline-core"), b"installed core")?;
            let request = UpdateRequest {
                pinned_version: None,
                check_only: true,
                ..request
            };
            let result = run_on(
                &request,
                channel,
                &no_home(),
                UNUSED_RELEASES,
                &[LATEST_TEST_PUBKEY],
            )
            .await?;
            assert_eq!(result.exit_code, 1, "{}", result.message);
            assert!(
                result.message.contains(&format!("\u{2192} {TEST_RELEASE}")),
                "{}",
                result.message
            );
            assert_eq!(
                server.requests(),
                [pointer.to_owned(), format!("{pointer}.minisig")]
            );
            assert_eq!(fs::read(exe(&install, crate::CLI_BIN))?, b"installed");
            install.remove()?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn update_installs_the_release_at_the_install_path() -> TestResult {
        let server = FakeReleaseServer::start(release_files("release-SHA256SUMS"));
        let (install, request) = install_from(&server)?;
        let installed = exe(&install, crate::CLI_BIN);

        let dry_run = UpdateRequest {
            dry_run: true,
            ..request.clone()
        };
        let result = run_on(
            &dry_run,
            UpdateChannel::Prod,
            &no_home(),
            &releases().base_url,
            KEYS,
        )
        .await?;
        assert!(
            result.message.starts_with("Would install"),
            "{}",
            result.message
        );
        assert_eq!(
            fs::read_dir(&install)?.count(),
            1,
            "a dry run installs nothing"
        );
        assert_eq!(fs::read(&installed)?, b"installed");

        let result = run_on(
            &request,
            UpdateChannel::Prod,
            &no_home(),
            &releases().base_url,
            KEYS,
        )
        .await?;
        assert_eq!(result.exit_code, 0, "{}", result.message);
        assert!(result.message.contains(TEST_RELEASE), "{}", result.message);
        assert_eq!(fs::read(&installed)?, LAUNCHER);
        assert_eq!(fs::read(exe(&install, "rayline-core"))?, CORE);
        assert_eq!(fs::read(install.join(CORE_LICENSE_NAME))?, LICENSE);
        // Windows keeps the replaced launcher as a retired backup.
        let mut installed_names = fs::read_dir(&install)?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<io::Result<Vec<_>>>()?;
        installed_names.retain(|name| !is_retired_backup(name));
        installed_names.sort();
        let mut expected_names = [crate::CLI_BIN, "rayline-core"]
            .map(|name| format!("{name}{}", std::env::consts::EXE_SUFFIX))
            .to_vec();
        expected_names.push(CORE_LICENSE_NAME.to_owned());
        expected_names.sort();
        assert_eq!(installed_names, expected_names);
        // A pinned version reads no channel pointer.
        let release = format!("/cli/v{TEST_RELEASE}/");
        assert!(
            server
                .requests()
                .iter()
                .all(|path| path.starts_with(&release)),
            "{:?}",
            server.requests()
        );
        install.remove()?;
        Ok(())
    }

    /// `verify_release_sums` of the release-SHA256SUMS fixture with the
    /// signature fixture `signature`, for `target` from `running`.
    fn release_sums_check(signature: &str, target: &str, running: &str) -> Result<(), String> {
        verify_release_sums(
            &signed_fixture("release-SHA256SUMS"),
            &signed_fixture(signature),
            &[RELEASE_TEST_PUBKEY],
            &version(target),
            &version(running),
        )
        .map_err(|error| error.to_string())
    }

    #[test]
    fn a_release_signature_must_name_the_version_fetched() {
        let signed = "release-SHA256SUMS.minisig";
        let unversioned = "release-SHA256SUMS.unversioned.minisig";
        assert_eq!(release_sums_check(signed, "99.0.0", "0.5.10"), Ok(()));
        // Another release's valid signature, served under 99.0.0.
        assert_eq!(
            release_sums_check("release-SHA256SUMS.v98.minisig", "99.0.0", "0.5.10"),
            Err(
                "SHA256SUMS for 99.0.0 is signed for version 98.0.0 \u{2014} refusing to install"
                    .to_owned()
            )
        );
        // The field binds old releases too when it is there.
        assert!(release_sums_check(signed, "0.5.4+abc", "0.6.0").is_err());
        // Without it: an update, the encoder check, or any release from 0.5.10
        // on (pre-releases of 0.5.10 included).
        let unsigned = |target: &str| {
            format!(
                "SHA256SUMS for {target} is signed without its version \u{2014} refusing to install"
            )
        };
        for (target, running) in [
            ("99.0.0", "0.5.10"),
            ("0.5.10+abc", "0.6.0"),
            ("0.5.10-dev.3+abc", "0.6.0"),
            ("0.5.9+abc", "0.5.3"),
            ("0.5.9+abc", "0.5.9+abc"),
        ] {
            assert_eq!(
                release_sums_check(unversioned, target, running),
                Err(unsigned(target)),
                "{target} from {running}"
            );
        }
        // Only an explicit downgrade to a release before 0.5.10 may lack it.
        assert_eq!(
            release_sums_check(unversioned, "0.5.9+abc", "0.5.10+def"),
            Ok(())
        );
        assert_eq!(
            release_sums_check(unversioned, "0.5.4-dev.2+abc", "0.5.10-dev.7+def"),
            Ok(())
        );
    }

    /// 0.5.9 and older check only `verify_signature`: the `version:` field
    /// the release workflow now signs must not break them.
    #[test]
    fn a_signature_naming_its_version_verifies_for_older_clients() {
        assert!(
            verify_signature(
                &signed_fixture("release-SHA256SUMS"),
                &signed_fixture("release-SHA256SUMS.minisig"),
                &[RELEASE_TEST_PUBKEY],
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn update_refuses_another_releases_sums_under_its_version() -> TestResult {
        let server = FakeReleaseServer::start(release_files_signed(
            TEST_RELEASE,
            "release-SHA256SUMS",
            "release-SHA256SUMS.v98.minisig",
        ));
        let (install, request) = install_from(&server)?;
        let error = run_on(
            &request,
            UpdateChannel::Prod,
            &no_home(),
            &releases().base_url,
            KEYS,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("is signed for version 98.0.0"), "{error}");
        assert_eq!(fs::read(exe(&install, crate::CLI_BIN))?, b"installed");
        assert!(!exe(&install, "rayline-core").exists());
        install.remove()?;
        Ok(())
    }

    #[tokio::test]
    async fn update_installs_an_older_release_signed_without_its_version() -> TestResult {
        let old = "0.1.0+0123456789ab";
        let server = FakeReleaseServer::start(release_files_signed(
            old,
            "release-SHA256SUMS",
            "release-SHA256SUMS.unversioned.minisig",
        ));
        let (install, request) = install_from(&server)?;
        let request = UpdateRequest {
            pinned_version: Some(version(old)),
            ..request
        };
        let result = run_on(
            &request,
            UpdateChannel::Prod,
            &no_home(),
            UNUSED_RELEASES,
            &[RELEASE_TEST_PUBKEY],
        )
        .await?;
        assert_eq!(result.exit_code, 0, "{}", result.message);
        assert_eq!(fs::read(exe(&install, crate::CLI_BIN))?, LAUNCHER);
        install.remove()?;
        Ok(())
    }

    #[tokio::test]
    async fn force_doesnt_install_an_older_channel_latest() -> TestResult {
        let server = FakeReleaseServer::start(Files::from([
            (
                "/cli/latest.txt".to_owned(),
                signed_fixture("latest-old.txt"),
            ),
            (
                "/cli/latest.txt.minisig".to_owned(),
                signed_fixture("latest-old.txt.minisig"),
            ),
        ]));
        let (install, request) = install_from(&server)?;
        let request = UpdateRequest {
            pinned_version: None,
            force: true,
            ..request
        };
        let result = run_on(
            &request,
            UpdateChannel::Prod,
            &no_home(),
            UNUSED_RELEASES,
            &[RELEASE_TEST_PUBKEY],
        )
        .await?;
        assert_eq!(result.exit_code, 0, "{}", result.message);
        assert!(
            result.message.starts_with("Already up to date: ")
                && result
                    .message
                    .contains("run: rayline update --version 0.1.0+0123456789ab"),
            "{}",
            result.message
        );
        assert_eq!(
            server.requests(),
            ["/cli/latest.txt", "/cli/latest.txt.minisig"]
        );
        assert_eq!(fs::read(exe(&install, crate::CLI_BIN))?, b"installed");
        install.remove()?;
        Ok(())
    }

    const ENCODER: &[u8] = b"fixture encoder\n";
    const ENCODER_LICENSE: &[u8] = b"fixture encoder license\n";
    const DLL: &[u8] = b"fixture cublas\n";

    /// A fake release serving the encoder, its license and a DLL for
    /// `platform`, and the SHA256SUMS lines for them.
    fn encoder_release(platform: &str) -> (FakeReleaseServer, String) {
        let release = format!("/cli/v{TEST_RELEASE}");
        let server = FakeReleaseServer::start(Files::from([
            (
                format!("{release}/{ENCODER_NAME}-{platform}"),
                ENCODER.to_vec(),
            ),
            (
                format!("{release}/{ENCODER_LICENSE_NAME}"),
                ENCODER_LICENSE.to_vec(),
            ),
            (format!("{release}/cublas64_12.dll"), DLL.to_vec()),
            (
                format!("{release}/SHA256SUMS.minisig"),
                b"signature".to_vec(),
            ),
        ]));
        let sums = format!(
            "{:x}  {ENCODER_NAME}-{platform}\n{:x}  {ENCODER_LICENSE_NAME}\n{:x}  cublas64_12.dll\n",
            Sha256::digest(ENCODER),
            Sha256::digest(ENCODER_LICENSE),
            Sha256::digest(DLL)
        );
        (server, sums)
    }

    #[tokio::test]
    async fn the_encoder_is_fetched_into_its_version_directory() -> TestResult {
        for platform in ["linux_x86_64", ENCODER_DLL_PLATFORM] {
            let (server, sums) = encoder_release(platform);
            let root = crate::test_dir::TestDir::new("rayline-update-test");
            let dir = root.join("arc").join(TEST_RELEASE);
            let target = version(TEST_RELEASE);
            let lens = std::cell::RefCell::new(Vec::new());
            let done = std::cell::Cell::new(0);
            fetch_encoder_with_sums(
                &server.base_url,
                &target,
                platform,
                &sums,
                &dir,
                &Report {
                    on_len: &|len| lens.borrow_mut().push(len),
                    on_bytes: &|_| {},
                    on_done: &|| done.set(done.get() + 1),
                },
            )
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
            let dlls = usize::from(platform == ENCODER_DLL_PLATFORM);
            assert_eq!(fs::read(dir.join(encoder_file_name()))?, ENCODER);
            assert_eq!(fs::read(dir.join(ENCODER_LICENSE_NAME))?, ENCODER_LICENSE);
            assert_eq!(dir.join("cublas64_12.dll").exists(), dlls == 1);
            assert_eq!(done.get(), 2 + dlls);
            assert_eq!(lens.borrow().len(), 2 + dlls);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = fs::metadata(dir.join(encoder_file_name()))?
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o755);
            }
            // No staging directory is left.
            let left = fs::read_dir(root.join("arc"))?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<io::Result<Vec<_>>>()?;
            assert_eq!(left, vec![std::ffi::OsString::from(TEST_RELEASE)]);
            root.remove()?;
        }
        Ok(())
    }

    #[test]
    fn a_fetched_encoder_is_used_only_when_every_listed_file_matches() -> TestResult {
        let platform = ENCODER_DLL_PLATFORM;
        let (_, sums) = encoder_release(platform);
        let dir = crate::test_dir::TestDir::new("rayline-update-test");
        let sha_of = |path: &Path| sha256_file(path).unwrap_or_default();
        let matches = |path: &Path, sha: &str| sha_of(path) == sha;
        assert_eq!(fetched_files(&dir, &sums, platform, &matches), None);
        fs::write(dir.join(encoder_file_name()), ENCODER)?;
        fs::write(dir.join(ENCODER_LICENSE_NAME), ENCODER_LICENSE)?;
        // The DLL the release lists is still missing.
        assert_eq!(fetched_files(&dir, &sums, platform, &matches), None);
        fs::write(dir.join("cublas64_12.dll"), DLL)?;
        assert_eq!(
            fetched_files(&dir, &sums, platform, &matches),
            Some(dir.join(encoder_file_name()))
        );
        fs::write(dir.join(ENCODER_LICENSE_NAME), b"changed")?;
        assert_eq!(fetched_files(&dir, &sums, platform, &matches), None);
        // A release without this platform's encoder has none to use.
        assert_eq!(
            fetched_files(&dir, &sums, "linux_aarch64", &|_, _| true),
            None
        );
        // An unsigned manifest is never trusted.
        fs::write(dir.join("SHA256SUMS"), &sums)?;
        fs::write(dir.join("SHA256SUMS.minisig"), b"not a signature")?;
        assert_eq!(
            fetched_encoder(
                &dir,
                &version(TEST_RELEASE),
                platform,
                &[RELEASE_TEST_PUBKEY],
                &|_, _| true
            ),
            None
        );
        dir.remove()?;
        Ok(())
    }

    #[tokio::test]
    async fn update_refreshes_the_encoder_only_when_one_was_fetched() -> TestResult {
        let server = FakeReleaseServer::start(release_files("release-SHA256SUMS"));
        let (install, request) = install_from(&server)?;
        let home = crate::test_dir::TestDir::new("rayline-update-test");
        let old = arc_dir(&home).join("0.5.3");
        fs::create_dir_all(&old)?;
        fs::write(old.join(encoder_file_name()), b"old encoder")?;
        assert!(has_encoder(&arc_dir(&home)));
        assert!(!has_encoder(&arc_dir(&no_home())));

        let dry_run = UpdateRequest {
            dry_run: true,
            ..request.clone()
        };
        let result = run_on(
            &dry_run,
            UpdateChannel::Prod,
            &home,
            &releases().base_url,
            KEYS,
        )
        .await?;
        let new = arc_dir(&home).join(TEST_RELEASE);
        assert!(
            result
                .message
                .contains(&format!("and the ARC encoder into {}", new.display())),
            "{}",
            result.message
        );
        // The release lists no encoder for this platform: the update stops
        // before anything is replaced, and the fetched encoder stays.
        let error = run_on(
            &request,
            UpdateChannel::Prod,
            &home,
            &releases().base_url,
            KEYS,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("no sha256 entry for rayline-arc-encoder-"),
            "{error}"
        );
        assert_eq!(fs::read(exe(&install, crate::CLI_BIN))?, b"installed");
        assert!(old.join(encoder_file_name()).is_file());

        // After a successful fetch, other versions' encoders are removed,
        // but not a download in progress.
        fs::create_dir_all(&new)?;
        let staging = arc_dir(&home).join(".fetch-1-2");
        fs::create_dir_all(&staging)?;
        remove_other_encoders(&new);
        assert!(!old.exists());
        assert!(new.exists());
        assert!(staging.exists());
        install.remove()?;
        home.remove()?;
        Ok(())
    }

    #[tokio::test]
    async fn a_truncated_download_fails_and_leaves_no_file() -> TestResult {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let serve = std::thread::spawn(move || -> std::io::Result<()> {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept()?;
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request)?;
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\npartial")?;
            Ok(())
        });
        let dir = crate::test_dir::TestDir::new("rayline-update-test");
        let path = dir.join("encoder");
        let result = download_to(&format!("http://{address}/encoder"), &path).await;
        assert!(matches!(result, Err(UpdateError::Network(_))), "{result:?}");
        assert!(!path.exists(), "a truncated download was promoted");
        serve.join().expect("server thread")?;
        dir.remove()?;
        Ok(())
    }

    #[tokio::test]
    async fn downloads_stream_to_disk_as_the_body_arrives() -> TestResult {
        // The server sends the first part of the body, then holds the rest
        // until that part is on disk: a download that buffered the whole body
        // in memory would never write it, and the server would report so.
        const FIRST: usize = 64 * 1024;
        let body: Vec<u8> = (0..4 * 1024 * 1024).map(|i: u32| (i % 251) as u8).collect();
        let dir = crate::test_dir::TestDir::new("rayline-update-test");
        let path = dir.join("big");
        let part = path.with_extension("part");
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let served = body.clone();
        let watched = part.clone();
        let serve = std::thread::spawn(move || -> std::io::Result<bool> {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept()?;
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request)?;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                served.len()
            )?;
            stream.write_all(&served[..FIRST])?;
            stream.flush()?;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut streamed = false;
            while std::time::Instant::now() < deadline {
                if fs::metadata(&watched).is_ok_and(|meta| meta.len() >= FIRST as u64) {
                    streamed = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            stream.write_all(&served[FIRST..])?;
            Ok(streamed)
        });
        download_to(&format!("http://{address}/big"), &path).await?;
        let streamed = serve.join().expect("server thread")?;
        assert!(
            streamed,
            "the first part was not written before the rest arrived"
        );
        assert_eq!(fs::read(&path)?, body);
        assert!(!part.exists());
        dir.remove()?;
        Ok(())
    }

    #[tokio::test]
    async fn update_refuses_a_release_without_the_core() -> TestResult {
        let server = FakeReleaseServer::start(release_files("release-no-core-SHA256SUMS"));
        let (install, request) = install_from(&server)?;
        let result = run_on(
            &request,
            UpdateChannel::Prod,
            &no_home(),
            &releases().base_url,
            KEYS,
        )
        .await;
        let Err(UpdateError::Checksum(message)) = result else {
            panic!("a release without the core must fail closed: {result:?}");
        };
        assert!(message.contains("rayline-core-"), "{message}");
        assert_eq!(fs::read_dir(&install)?.count(), 1);
        assert_eq!(fs::read(exe(&install, crate::CLI_BIN))?, b"installed");
        install.remove()?;
        Ok(())
    }

    #[tokio::test]
    async fn update_refuses_a_release_without_a_signature() -> TestResult {
        let mut files = release_files("release-SHA256SUMS");
        let signature = format!("/cli/v{TEST_RELEASE}/SHA256SUMS.minisig");
        files.remove(&signature);
        let server = FakeReleaseServer::start(files);
        let (install, request) = install_from(&server)?;
        let result = run_on(
            &request,
            UpdateChannel::Prod,
            &no_home(),
            &releases().base_url,
            KEYS,
        )
        .await;
        let Err(UpdateError::Network(message)) = result else {
            panic!("a release without SHA256SUMS.minisig must fail closed: {result:?}");
        };
        assert!(message.contains("SHA256SUMS.minisig"), "{message}");
        assert_eq!(
            server.requests(),
            [format!("/cli/v{TEST_RELEASE}/SHA256SUMS"), signature],
            "no artifact is downloaded before the signature checks out"
        );
        assert_eq!(fs::read_dir(&install)?.count(), 1);
        assert_eq!(fs::read(exe(&install, crate::CLI_BIN))?, b"installed");
        install.remove()?;
        Ok(())
    }

    #[tokio::test]
    async fn update_refuses_a_release_signed_by_another_key() -> TestResult {
        let server = FakeReleaseServer::start(release_files("release-SHA256SUMS"));
        let (install, request) = install_from(&server)?;
        let result = run_on(
            &request,
            UpdateChannel::Prod,
            &no_home(),
            &releases().base_url,
            crate::MINISIGN_PUBLIC_KEYS,
        )
        .await;
        assert!(
            matches!(result, Err(UpdateError::Signature(_))),
            "{result:?}"
        );
        assert_eq!(fs::read_dir(&install)?.count(), 1);
        assert_eq!(fs::read(exe(&install, crate::CLI_BIN))?, b"installed");
        install.remove()?;
        Ok(())
    }

    #[tokio::test]
    async fn a_bare_version_resolves_its_build_label_from_the_release() -> TestResult {
        let pointer = |version: &str| format!("/v{version}/latest.txt");
        let releases = FakeReleaseServer::start(Files::from([
            (pointer(TEST_RELEASE), b"99.0.0+0123abcd\n".to_vec()),
            (
                format!("{}.minisig", pointer(TEST_RELEASE)),
                b"signature".to_vec(),
            ),
            (pointer("98.0.0"), signed_fixture("latest.txt")),
            (
                format!("{}.minisig", pointer("98.0.0")),
                signed_fixture("latest.txt.minisig"),
            ),
        ]));
        let signed = FakeReleaseServer::start(Files::from([
            (pointer(TEST_RELEASE), signed_fixture("latest.txt")),
            (
                format!("{}.minisig", pointer(TEST_RELEASE)),
                signed_fixture("latest.txt.minisig"),
            ),
        ]));
        let keys = &[LATEST_TEST_PUBKEY];
        let running = version("1.0.0+running");
        // The running version, named without its label, is itself; nothing
        // is fetched.
        assert_eq!(
            full_version(UNUSED_RELEASES, &version("1.0.0"), &running, keys).await?,
            running
        );
        // A signed `latest.txt` gives the full version.
        assert_eq!(
            full_version(&signed.base_url, &version(TEST_RELEASE), &running, keys).await?,
            version(TEST_RELEASE)
        );
        assert_eq!(
            signed.requests(),
            [
                pointer(TEST_RELEASE),
                format!("{}.minisig", pointer(TEST_RELEASE))
            ]
        );
        // A full version is used as given, with nothing fetched.
        let full = version("99.0.0+0123abcd");
        assert_eq!(
            full_version(UNUSED_RELEASES, &full, &running, keys).await?,
            full
        );
        // An unsigned `latest.txt` is refused.
        assert!(matches!(
            full_version(&releases.base_url, &version(TEST_RELEASE), &running, keys).await,
            Err(UpdateError::Signature(_))
        ));
        // A release whose `latest.txt` names another version is refused.
        assert_eq!(
            full_version(&releases.base_url, &version("98.0.0"), &running, keys)
                .await
                .unwrap_err(),
            UpdateError::Network("release 98.0.0 names version 99.0.0".to_owned())
        );
        // A release that doesn't exist says so.
        assert_eq!(
            full_version(&releases.base_url, &version("97.0.0"), &running, keys)
                .await
                .unwrap_err(),
            UpdateError::Network("release 97.0.0 does not exist".to_owned())
        );
        Ok(())
    }
}
