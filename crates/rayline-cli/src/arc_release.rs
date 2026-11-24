//! The ARC runtime release local decisions load: downloaded from a pinned
//! Hugging Face repo and commit into the Hugging Face cache, and checked
//! file by file against its `manifest.json`, whose own sha256 is pinned.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::ArcRelease;
use crate::start_progress::Progress;

const MANIFEST_FILE: &str = "manifest.json";
/// The largest `manifest.json` read or downloaded.
const MAX_MANIFEST_BYTES: u64 = 1 << 20;

/// The built-in release: the public ARC repo at a pinned commit.
pub fn built_in() -> ArcRelease {
    ArcRelease {
        repo: "rayline-ai/ARC-1.0".to_owned(),
        revision: crate::hybrid::ARC_COMMIT.to_owned(),
        manifest_sha256: "9664c9cd81535acfa7c1418db202ee13c653fa988731f9baca04cbeab539dfbe"
            .to_owned(),
        dir: Some("runtime".to_owned()),
    }
}

/// The release's directory in the cache at `hub`, with every file its
/// manifest lists present and matching. Files already cached at the pinned
/// commit are not downloaded again, so a complete cache works offline. A
/// cached file that does not match is downloaded once more; a downloaded
/// file that does not match is removed and refused. `token` authorizes a
/// private repo. `progress` is told what is downloaded.
pub fn fetch(
    hub: &Path,
    stamps: &Path,
    hub_url: &str,
    token: Option<&str>,
    release: &ArcRelease,
    progress: &Progress,
) -> Result<PathBuf, String> {
    let in_release = |name: &str| match &release.dir {
        Some(dir) => format!("{dir}/{name}"),
        None => name.to_owned(),
    };
    let pinned = |path: String, max_bytes: u64, sha256: &str| Pinned {
        repo: &release.repo,
        revision: &release.revision,
        path,
        max_bytes,
        sha256: sha256.to_owned(),
    };
    let manifest = pinned(
        in_release(MANIFEST_FILE),
        MAX_MANIFEST_BYTES,
        &release.manifest_sha256,
    );
    // The manifest is small, and only once it is read is the release's
    // size known: it isn't shown.
    let at = fetch_file(hub, stamps, hub_url, token, &manifest, &Progress::silent())?;
    let files = fs::read(&at)
        .map_err(|e| e.to_string())
        .and_then(|raw| rayline_arc_decide::manifest_files(&raw).map_err(|e| e.to_string()))
        .map_err(|e| format!("{} of {}: {e}", manifest.path, release.repo))?;
    let files: Vec<Pinned<'_>> = files
        .iter()
        .map(|(name, want)| pinned(in_release(name), want.bytes, &want.sha256))
        .collect();
    for file in &files {
        expect_unless_cached(hub, file, progress);
    }
    for file in &files {
        fetch_file(hub, stamps, hub_url, token, file, progress)?;
    }
    Ok(rayline_hf::hf_cache_snapshot_path(
        hub,
        &release.repo,
        release.dir.as_deref().unwrap_or_default(),
        &release.revision,
    ))
}

/// Whether the file at `at` has the sha256 `sha256`: unchanged since its
/// stamp in `stamps` says it last did, or hashed now (and stamped when
/// settled), as a cached release file is.
pub fn verified(stamps: &Path, at: &Path, sha256: &str) -> bool {
    stamped(stamps, at, sha256) || hashes_to(stamps, at, sha256, std::time::SystemTime::now)
}

/// Waits until a file written now has settled, so hashing it then records
/// its stamp. Nothing to wait for where there are no stamps.
pub fn settle() {
    #[cfg(unix)]
    std::thread::sleep(SETTLED);
}

/// One file pinned by repo, commit, path and sha256, at most `max_bytes`
/// long.
pub struct Pinned<'a> {
    pub repo: &'a str,
    pub revision: &'a str,
    pub path: String,
    pub max_bytes: u64,
    pub sha256: String,
}

/// Tells `progress` to expect `file`'s bytes when it isn't in the cache at
/// `hub`. A cached copy that turns out not to match is expected when it is
/// discarded.
pub fn expect_unless_cached(hub: &Path, file: &Pinned<'_>, progress: &Progress) {
    if rayline_hf::cached_hf_file(hub, file.repo, &file.path, file.revision).is_none() {
        progress.expect(file.max_bytes);
    }
}

/// The pinned file's path in the cache at `hub`: the cached copy when it
/// matches, else downloaded, telling `progress`. A copy that doesn't match is
/// removed; a download that doesn't match is refused. A cached copy is
/// hashed unless its stamp in `stamps` shows it unchanged since it last
/// hashed to the pin (a blob's name doesn't prove its bytes).
pub fn fetch_file(
    hub: &Path,
    stamps: &Path,
    hub_url: &str,
    token: Option<&str>,
    file: &Pinned<'_>,
    progress: &Progress,
) -> Result<PathBuf, String> {
    let (repo, revision, path) = (file.repo, file.revision, file.path.as_str());
    let matches = |at: &Path| {
        fs::metadata(at).is_ok_and(|meta| meta.len() <= file.max_bytes)
            && (stamped(stamps, at, &file.sha256)
                || hashes_to(stamps, at, &file.sha256, std::time::SystemTime::now))
    };
    if let Some(cached) = rayline_hf::cached_hf_file(hub, repo, path, revision) {
        if matches(&cached) {
            return Ok(cached);
        }
        discard(&cached);
        progress.expect(file.max_bytes);
    }
    let on_bytes = |bytes| progress.on_bytes(bytes);
    let downloaded = rayline_hf::download_to_hf_cache_at(
        hub,
        hub_url,
        rayline_hf::Download {
            token,
            max_bytes: Some(file.max_bytes),
            pinned: true,
            on_bytes: Some(rayline_hf::OnBytes(&on_bytes)),
        },
        repo,
        path,
        revision,
    )
    .map_err(|e| format!("cannot download {path} from {repo} at {revision}: {e}"))?;
    progress.file_done();
    if matches(&downloaded) {
        return Ok(downloaded);
    }
    discard(&downloaded);
    Err(format!(
        "{path} from {repo} at {revision} is not the pinned file; it was removed from the cache"
    ))
}

/// What identifies the file at its real path `real`, as `meta` describes it:
/// size, modification and inode change times, device and inode. On Unix any
/// write, rename over or permission change alters it.
#[cfg(unix)]
fn stamp_of(real: &Path, meta: &fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt as _;
    format!(
        "path {}\nbytes {}\nmtime {}.{}\nctime {}.{}\ndev {}\nino {}\n",
        real.display(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
        meta.dev(),
        meta.ino()
    )
}

/// Whether `at` is unchanged since it last hashed to `sha256`: the stamp in
/// `stamps` equals the file's now. Anything missing, unreadable or not a
/// regular file is no. Only on Unix: elsewhere a file's metadata doesn't
/// show every change, so it is always hashed.
#[cfg(unix)]
fn stamped(stamps: &Path, at: &Path, sha256: &str) -> bool {
    let Ok(real) = fs::canonicalize(at) else {
        return false;
    };
    let Ok(meta) = fs::metadata(&real) else {
        return false;
    };
    let recorded = stamps.join(sha256);
    meta.is_file()
        && fs::symlink_metadata(&recorded).is_ok_and(|m| m.is_file())
        && fs::read_to_string(&recorded).is_ok_and(|r| r == stamp_of(&real, &meta))
}

#[cfg(not(unix))]
fn stamped(_stamps: &Path, _at: &Path, _sha256: &str) -> bool {
    false
}

/// How long a file must have been unchanged, by both its modification and
/// inode change times, before its stamp is recorded. A filesystem that keeps
/// times to 1 or 2 seconds (FAT, exFAT, HFS+) would otherwise give a rewrite
/// in the same second as the hash the stamp's times.
#[cfg(unix)]
const SETTLED: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether `at` hashes to `sha256`. On Unix, when it does, records its stamp
/// in `stamps`, but only when the file hashed (one open handle) is the one
/// `at` resolves to, nothing changed it during the hash, and it had already
/// been unchanged for [`SETTLED`] when `now` is read, before the file is
/// opened: a write after that lands in a later timestamp, even on a
/// filesystem with 1 or 2 second times.
fn hashes_to(
    stamps: &Path,
    at: &Path,
    sha256: &str,
    now: impl FnOnce() -> std::time::SystemTime,
) -> bool {
    let started = now();
    let Ok(real) = fs::canonicalize(at) else {
        return false;
    };
    let Ok(mut file) = fs::File::open(&real) else {
        return false;
    };
    let Ok(before) = file.metadata() else {
        return false;
    };
    if !before.is_file() || sha256_of(&mut file).is_none_or(|actual| actual != sha256) {
        return false;
    }
    #[cfg(unix)]
    {
        let stamp = stamp_of(&real, &before);
        let unchanged = file
            .metadata()
            .is_ok_and(|after| stamp_of(&real, &after) == stamp)
            && fs::metadata(&real).is_ok_and(|now| stamp_of(&real, &now) == stamp);
        if unchanged && settled(&before, started) {
            record_stamp(stamps, sha256, &stamp);
        }
    }
    #[cfg(not(unix))]
    let _ = (stamps, started);
    true
}

/// Whether `meta`'s modification and inode change times are both at least
/// [`SETTLED`] before `now`. A time that can't be read, or is in the future,
/// is not.
#[cfg(unix)]
fn settled(meta: &fs::Metadata, now: std::time::SystemTime) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(now) = now.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    let at = |secs: i64, nanos: i64| {
        Some(std::time::Duration::new(
            u64::try_from(secs).ok()?,
            u32::try_from(nanos).ok()?,
        ))
    };
    [
        at(meta.mtime(), meta.mtime_nsec()),
        at(meta.ctime(), meta.ctime_nsec()),
    ]
    .into_iter()
    .all(|time| time.is_some_and(|time| now.checked_sub(time).is_some_and(|age| age >= SETTLED)))
}

/// Writes a stamp into `stamps` (created owner-only), replacing any before
/// it. A stamp that can't be written only means hashing again next time.
#[cfg(unix)]
fn record_stamp(stamps: &Path, sha256: &str, stamp: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = stamps.join(format!(".{sha256}.{}.tmp", std::process::id()));
    let written = fs::create_dir_all(stamps)
        .and_then(|()| fs::set_permissions(stamps, fs::Permissions::from_mode(0o700)))
        .and_then(|()| fs::write(&tmp, stamp))
        .and_then(|()| fs::rename(&tmp, stamps.join(sha256)));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

/// The SHA-256 of the file at `path`, read in chunks.
fn file_sha256(path: &Path) -> Option<String> {
    sha256_of(&mut fs::File::open(path).ok()?)
}

/// The SHA-256 of what `file` reads from here on, in chunks.
fn sha256_of(file: &mut fs::File) -> Option<String> {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(file, &mut hasher).ok()?;
    Some(format!("{:x}", hasher.finalize()))
}

/// Removes a snapshot file. The blob it links to is removed too when it is
/// corrupt (it doesn't hash to its name) and in the same repo's `blobs`
/// directory: a sound blob may be another revision's, and a link anywhere
/// else is only unlinked.
fn discard(snapshot: &Path) {
    let blobs = snapshot
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == "snapshots"))
        .and_then(Path::parent)
        .and_then(|repo| fs::canonicalize(repo.join("blobs")).ok());
    if let (Ok(blob), Some(blobs)) = (fs::canonicalize(snapshot), blobs) {
        let named = blob
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("sha256-"));
        let corrupt = blob.parent() == Some(blobs.as_path())
            && named.is_some_and(|sha| file_sha256(&blob).is_none_or(|actual| actual != sha));
        if corrupt {
            let _ = fs::remove_file(blob);
        }
    }
    let _ = fs::remove_file(snapshot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest as _;
    use std::collections::BTreeMap;

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", sha2::Sha256::digest(bytes))
    }
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const REVISION: &str = "1111111111111111111111111111111111111111";
    const REPO: &str = "test-org/arc";
    const RELEASE_DIR: &str = "runtime";
    const TOKEN: &str = "hf_test_token";

    /// A fake Hub serving `files` (path under the repo -> bytes) at
    /// `REVISION`, and 404 for anything else. Records each path it serves.
    /// A private one answers 401 to a request without `TOKEN`.
    struct FakeHub {
        url: String,
        served: Arc<Mutex<Vec<String>>>,
    }

    fn fake_hub(files: BTreeMap<String, Vec<u8>>) -> FakeHub {
        serve(files, false)
    }

    fn serve(files: BTreeMap<String, Vec<u8>>, private: bool) -> FakeHub {
        serve_paced(files, private, None)
    }

    /// A fake Hub that sends each body in ten parts, `pace` apart.
    fn serve_paced(
        files: BTreeMap<String, Vec<u8>>,
        private: bool,
        pace: Option<std::time::Duration>,
    ) -> FakeHub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let served = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&served);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let mut authorized = !private;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                    let (name, value) = header.split_once(':').unwrap_or_default();
                    if name.eq_ignore_ascii_case("authorization")
                        && value.trim() == format!("Bearer {TOKEN}")
                    {
                        authorized = true;
                    }
                }
                let target = line.split_whitespace().nth(1).unwrap_or_default();
                let prefix = format!("/{REPO}/resolve/{REVISION}/");
                let body = target
                    .strip_prefix(&prefix)
                    .and_then(|path| files.get(path).map(|body| (path, body)));
                let response = match body {
                    _ if !authorized => {
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                    Some((path, body)) => {
                        log.lock().expect("lock").push(path.to_owned());
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        if let Some(pace) = pace {
                            let _ = stream.write_all(&head);
                            for part in body.chunks(body.len().div_ceil(10).max(1)) {
                                std::thread::sleep(pace);
                                let _ = stream.write_all(part).and_then(|()| stream.flush());
                            }
                            continue;
                        }
                        let mut head = head;
                        head.extend_from_slice(body);
                        head
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&response);
            }
        });
        FakeHub { url, served }
    }

    fn scratch(name: &str) -> crate::test_dir::TestDir {
        crate::test_dir::TestDir::new(&format!("rayline-arc-release-{name}"))
    }

    /// A release's files under `runtime/`, its manifest, and the pin.
    fn release(files: &[(&str, &[u8])]) -> (BTreeMap<String, Vec<u8>>, ArcRelease) {
        release_in(Some(RELEASE_DIR), files)
    }

    fn release_in(
        dir: Option<&str>,
        files: &[(&str, &[u8])],
    ) -> (BTreeMap<String, Vec<u8>>, ArcRelease) {
        let at = |name: &str| match dir {
            Some(dir) => format!("{dir}/{name}"),
            None => name.to_owned(),
        };
        let listed: BTreeMap<&str, serde_json::Value> = files
            .iter()
            .map(|(name, body)| {
                (
                    *name,
                    serde_json::json!({"sha256": sha256_hex(body), "bytes": body.len()}),
                )
            })
            .collect();
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema": "rayline.arc-runtime-manifest.v1", "files": listed,
        }))
        .expect("manifest");
        let pin = ArcRelease {
            repo: REPO.to_owned(),
            revision: REVISION.to_owned(),
            manifest_sha256: sha256_hex(&manifest),
            dir: dir.map(str::to_owned),
        };
        let mut served: BTreeMap<String, Vec<u8>> = files
            .iter()
            .map(|(name, body)| (at(name), body.to_vec()))
            .collect();
        served.insert(at(MANIFEST_FILE), manifest);
        (served, pin)
    }

    const FILES: [(&str, &[u8]); 3] = [
        ("arc-runtime.json", b"{\"runtime\": 1}"),
        ("heads.safetensors", b"heads"),
        ("prices.json", b"{\"models\": {}}"),
    ];

    #[test]
    fn a_slow_download_shows_progress_lines_off_a_terminal_and_a_cached_one_line() {
        let hub = scratch("progress");
        let big = vec![7u8; 3_000_000];
        let (served, pin) = release(&[("heads.safetensors", &big), ("prices.json", b"{}")]);
        let fake = serve_paced(served, false, Some(std::time::Duration::from_millis(30)));
        let out = crate::start_progress::tests::Captured::default();
        let progress = Progress::new(Box::new(out.clone()), false);
        fetch(
            &hub,
            &hub.join("verified"),
            &fake.url,
            None,
            &pin,
            &progress,
        )
        .expect("fetches");
        progress.release_ready();
        let text = out.text();
        assert!(!text.contains('\r'), "{text}");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.first(),
            Some(&"Downloading ARC release: 3 MB"),
            "{text}"
        );
        let updates = lines
            .iter()
            .filter(|line| line.starts_with("Downloading ARC release: ") && line.contains("%)"))
            .count();
        assert!((3..=10).contains(&updates), "{text}");
        assert!(
            lines
                .last()
                .is_some_and(|line| line.starts_with("ARC release downloaded: 3 MB in ")),
            "{text}"
        );

        let out = crate::start_progress::tests::Captured::default();
        let progress = Progress::new(Box::new(out.clone()), false);
        fetch(
            &hub,
            &hub.join("verified"),
            &fake.url,
            None,
            &pin,
            &progress,
        )
        .expect("cached");
        progress.release_ready();
        assert_eq!(out.text(), "ARC release cached\n");
    }

    #[test]
    fn a_pinned_release_downloads_once_and_then_loads_offline() {
        let hub = scratch("good");
        let (served, pin) = release(&FILES);
        let fake = fake_hub(served);
        let dir = fetch(
            &hub,
            &hub.join("verified"),
            &fake.url,
            None,
            &pin,
            &Progress::silent(),
        )
        .expect("fetches");
        for (name, body) in FILES {
            assert_eq!(fs::read(dir.join(name)).expect("cached"), body);
        }
        assert_eq!(fake.served.lock().expect("lock").len(), FILES.len() + 1);

        // Offline: nothing listens at this address, and the cache suffices.
        let offline = TcpListener::bind("127.0.0.1:0").expect("bind");
        let offline_url = format!("http://{}", offline.local_addr().expect("addr"));
        drop(offline);
        assert_eq!(
            fetch(
                &hub,
                &hub.join("verified"),
                &offline_url,
                None,
                &pin,
                &Progress::silent()
            )
            .expect("cached"),
            dir
        );
    }

    #[cfg(unix)]
    #[test]
    fn discarding_a_snapshot_removes_only_a_corrupt_blob_of_its_own_repo() {
        let root = scratch("discard");
        let repo = root.join("models--test-org--arc");
        let snapshot_dir = repo.join("snapshots").join(REVISION).join(RELEASE_DIR);
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::create_dir_all(repo.join("blobs")).unwrap();
        let outside = root.join("unrelated.txt");
        fs::write(&outside, b"keep me").unwrap();
        let link = snapshot_dir.join("manifest.json");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        discard(&link);
        assert!(!link.exists() && fs::symlink_metadata(&link).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"keep me");

        // A sound blob (it hashes to its name) may be another revision's.
        let sound = repo
            .join("blobs")
            .join(format!("sha256-{}", sha256_hex(b"good")));
        fs::write(&sound, b"good").unwrap();
        std::os::unix::fs::symlink(&sound, &link).unwrap();
        discard(&link);
        assert!(sound.exists() && fs::symlink_metadata(&link).is_err());

        // A corrupt one is removed.
        let corrupt = repo
            .join("blobs")
            .join(format!("sha256-{}", sha256_hex(b"good")));
        fs::write(&corrupt, b"bad").unwrap();
        std::os::unix::fs::symlink(&corrupt, &link).unwrap();
        discard(&link);
        assert!(!corrupt.exists());
    }

    #[test]
    fn a_tampered_file_or_manifest_is_refused_and_not_kept() {
        let hub = scratch("tampered");
        let (mut served, pin) = release(&FILES);
        served.insert(
            format!("{RELEASE_DIR}/heads.safetensors"),
            b"other".to_vec(),
        );
        let fake = fake_hub(served.clone());
        let error = fetch(
            &hub,
            &hub.join("verified"),
            &fake.url,
            None,
            &pin,
            &Progress::silent(),
        )
        .expect_err("refused");
        assert!(error.contains("runtime/heads.safetensors"), "{error}");
        assert!(error.contains("not the pinned file"), "{error}");
        let path = format!("{RELEASE_DIR}/heads.safetensors");
        assert!(rayline_hf::cached_hf_file(&hub, REPO, &path, REVISION).is_none());

        // A manifest that is not the pinned one is refused before any file.
        let mut other = pin.clone();
        other.manifest_sha256 = "f".repeat(64);
        let error = fetch(
            &hub,
            &hub.join("verified"),
            &fake.url,
            None,
            &other,
            &Progress::silent(),
        )
        .expect_err("refused");
        assert!(error.contains("runtime/manifest.json"), "{error}");
    }

    #[test]
    fn a_cached_file_that_changed_is_downloaded_again() {
        let hub = scratch("repair");
        let (served, pin) = release(&FILES);
        let fake = fake_hub(served);
        let dir = fetch(
            &hub,
            &hub.join("verified"),
            &fake.url,
            None,
            &pin,
            &Progress::silent(),
        )
        .expect("fetches");
        let heads = fs::canonicalize(dir.join("heads.safetensors")).expect("blob");
        fs::write(&heads, b"changed").expect("tamper");
        fetch(
            &hub,
            &hub.join("verified"),
            &fake.url,
            None,
            &pin,
            &Progress::silent(),
        )
        .expect("repaired");
        assert_eq!(
            fs::read(dir.join("heads.safetensors")).expect("read"),
            b"heads"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_hashed_file_is_stamped_and_any_change_since_voids_the_stamp() {
        use std::os::unix::fs::PermissionsExt as _;
        let hub = scratch("stamp");
        fs::create_dir_all(&hub).expect("hub");
        let stamps = hub.join("verified");
        let file = hub.join("blob");
        fs::write(&file, b"heads").expect("write");
        let sha = sha256_hex(b"heads");
        let other = sha256_hex(b"other");
        // A clock past the settling time; a just-written file at the real
        // time isn't stamped (see the next test).
        let later = || std::time::SystemTime::now() + 2 * SETTLED;
        assert!(!stamped(&stamps, &file, &sha), "no stamp before a hash");
        assert!(!hashes_to(&stamps, &file, &other, later));
        assert!(
            !stamped(&stamps, &file, &other),
            "a failed hash records nothing"
        );
        assert!(hashes_to(&stamps, &file, &sha, later));
        assert!(stamped(&stamps, &file, &sha));
        assert_eq!(
            fs::metadata(&stamps).expect("stamps").permissions().mode() & 0o777,
            0o700
        );
        // The stamp is for this sha256 only.
        assert!(!stamped(&stamps, &file, &other));

        // The same length rewritten, with its modification time put back:
        // the inode change time moved. A real stamp is only recorded once
        // the file's times are SETTLED old, so any later change lands in a
        // later time; the clock above skipped that wait, so wait here, or a
        // filesystem with coarse times gives the rewrite the stamp's time.
        settle();
        let modified = fs::metadata(&file)
            .and_then(|m| m.modified())
            .expect("mtime");
        fs::write(&file, b"HEADS").expect("rewrite");
        fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|f| f.set_modified(modified))
            .expect("set mtime");
        assert!(!stamped(&stamps, &file, &sha));

        // Another file with the same bytes isn't the stamped one, nor is a
        // link that now resolves elsewhere.
        fs::write(&file, b"heads").expect("restore");
        assert!(hashes_to(&stamps, &file, &sha, later));
        let copy = hub.join("copy");
        fs::copy(&file, &copy).expect("copy");
        assert!(!stamped(&stamps, &copy, &sha));
        let link = hub.join("link");
        std::os::unix::fs::symlink(&file, &link).expect("link");
        assert!(stamped(&stamps, &link, &file_sha256(&file).expect("sha")));
        fs::remove_file(&link).expect("unlink");
        std::os::unix::fs::symlink(&copy, &link).expect("relink");
        assert!(!stamped(&stamps, &link, &sha));

        // A garbage stamp, or a stamp that is a link, is no stamp.
        let recorded = stamps.join(&sha);
        let good = fs::read_to_string(&recorded).expect("stamp");
        fs::write(&recorded, b"garbage").expect("stamp");
        assert!(!stamped(&stamps, &file, &sha));
        let elsewhere = hub.join("forged");
        fs::write(&elsewhere, good).expect("forged");
        fs::remove_file(&recorded).expect("remove");
        std::os::unix::fs::symlink(&elsewhere, &recorded).expect("link stamp");
        assert!(!stamped(&stamps, &file, &sha));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_changed_within_the_settling_time_is_not_stamped() {
        let hub = scratch("settle");
        fs::create_dir_all(&hub).expect("hub");
        let stamps = hub.join("verified");
        let file = hub.join("blob");
        fs::write(&file, b"heads").expect("write");
        let sha = sha256_hex(b"heads");
        let written = fs::metadata(&file)
            .and_then(|m| m.modified())
            .expect("mtime");
        // Hashed, but too fresh by either time: no stamp.
        assert!(hashes_to(&stamps, &file, &sha, || written + SETTLED / 2));
        assert!(!stamped(&stamps, &file, &sha));
        // An old modification time doesn't help when the inode changed
        // just now (a chmod, or a rewrite with the time put back).
        fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|f| f.set_modified(written - 10 * SETTLED))
            .expect("set mtime");
        let changed = std::time::SystemTime::now();
        assert!(hashes_to(&stamps, &file, &sha, || changed));
        assert!(!stamped(&stamps, &file, &sha));
        // Settled by both times: stamped.
        assert!(hashes_to(&stamps, &file, &sha, || changed + 2 * SETTLED));
        assert!(stamped(&stamps, &file, &sha));
    }

    #[test]
    fn a_corrupt_blob_behind_a_copied_snapshot_is_replaced() {
        // Where links aren't available the snapshot is a copy of its blob,
        // so removing the snapshot leaves the blob.
        let hub = scratch("copied");
        let (served, pin) = release(&FILES);
        let fake = fake_hub(served);
        let dir = fetch(
            &hub,
            &hub.join("verified"),
            &fake.url,
            None,
            &pin,
            &Progress::silent(),
        )
        .expect("fetches");
        let snapshot = dir.join("heads.safetensors");
        let blob = fs::canonicalize(&snapshot).expect("blob");
        fs::write(&blob, b"changed").expect("tamper");
        fs::remove_file(&snapshot).expect("unlink");
        fs::write(&snapshot, b"changed").expect("copy");
        fetch(
            &hub,
            &hub.join("verified"),
            &fake.url,
            None,
            &pin,
            &Progress::silent(),
        )
        .expect("repaired");
        assert_eq!(fs::read(&blob).expect("blob"), b"heads");
        assert_eq!(fs::read(&snapshot).expect("snapshot"), b"heads");
    }

    #[test]
    fn a_missing_file_names_it() {
        let hub = scratch("missing");
        let (mut served, pin) = release(&FILES);
        served.remove(&format!("{RELEASE_DIR}/prices.json"));
        let fake = fake_hub(served);
        let error = fetch(
            &hub,
            &hub.join("verified"),
            &fake.url,
            None,
            &pin,
            &Progress::silent(),
        )
        .expect_err("missing");
        assert!(
            error.contains("cannot download runtime/prices.json"),
            "{error}"
        );
        assert!(error.contains("404"), "{error}");
    }

    #[test]
    fn a_manifest_path_outside_the_release_is_refused() {
        let hub = scratch("outside");
        let (served, pin) = release(&[("../escape", b"x")]);
        let fake = fake_hub(served);
        let error = fetch(
            &hub,
            &hub.join("verified"),
            &fake.url,
            None,
            &pin,
            &Progress::silent(),
        )
        .expect_err("refused");
        assert!(error.contains("outside the release"), "{error}");
    }

    #[test]
    fn a_private_repo_needs_the_token_and_names_hf_token_without_it() {
        let hub = scratch("private");
        let (served, pin) = release_in(None, &FILES);
        let fake = serve(served, true);
        let error = fetch(
            &hub,
            &hub.join("verified"),
            &fake.url,
            None,
            &pin,
            &Progress::silent(),
        )
        .expect_err("401");
        assert!(error.contains("401"), "{error}");
        assert!(error.contains("set HF_TOKEN"), "{error}");
        assert!(fake.served.lock().expect("lock").is_empty());

        let dir = fetch(
            &hub,
            &hub.join("verified"),
            &fake.url,
            Some(TOKEN),
            &pin,
            &Progress::silent(),
        )
        .expect("with the token");
        // A release at the repo root is the snapshot itself.
        assert!(dir.ends_with(REVISION));
        for (name, body) in FILES {
            assert_eq!(fs::read(dir.join(name)).expect("cached"), body);
        }

        let wrong = scratch("private-wrong");
        let error = fetch(
            &wrong,
            &wrong.join("verified"),
            &fake.url,
            Some("hf_other"),
            &pin,
            &Progress::silent(),
        )
        .expect_err("401");
        assert!(error.contains("has no access"), "{error}");
        assert!(!error.contains("hf_other"), "{error}");
    }
}
