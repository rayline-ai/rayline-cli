//! `rayline local stop` stops a local process that was never ready: one
//! whose `rayline local start` was killed before the process wrote its ready
//! file leaves only `supervisor.json` behind.
#![cfg(unix)]

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("rayline-stop-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&path).expect("temporary home");
        Self(path)
    }

    fn state(&self) -> PathBuf {
        self.0.join(".rayline").join("local")
    }

    /// Record `pid` as the local process, with no ready file.
    fn record(&self, pid: u32) {
        fs::create_dir_all(self.state()).expect("state dir");
        fs::write(
            self.state().join("supervisor.json"),
            format!(r#"{{"pid":{pid},"config":"/settings.json","env":null}}"#),
        )
        .expect("supervisor.json");
    }

    fn stop(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rayline"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("RAYLINE_") {
                command.env_remove(name);
            }
        }
        command
            .args(["local", "stop"])
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .output()
            .expect("run rayline")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A long-lived child, in its own session when `detached`, as `rayline local
/// start` starts the local process.
fn sleeper(detached: bool) -> Child {
    let mut command = Command::new("sleep");
    command.arg("60").stdin(Stdio::null());
    if detached {
        // SAFETY: setsid is async-signal-safe and touches no Rust state.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.spawn().expect("start sleep")
}

#[test]
fn stop_ends_a_local_process_that_never_became_ready() {
    let home = Home::new();
    let mut process = sleeper(true);
    home.record(process.id());
    let output = home.stop();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Local process stopped.\n"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while process.try_wait().expect("wait").is_none() {
        assert!(Instant::now() < deadline, "the process is still running");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!home.state().join("supervisor.json").exists());
}

#[test]
fn stop_leaves_alone_a_reused_pid_that_is_not_a_local_process() {
    let home = Home::new();
    // Not a session leader: not a process `rayline local start` started.
    let mut process = sleeper(false);
    home.record(process.id());
    let output = home.stop();
    let alive = process.try_wait().expect("wait").is_none();
    let _ = process.kill();
    let _ = process.wait();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Local process: not running\n"
    );
    assert!(alive, "stop signalled a process it did not start");
    assert!(!home.state().join("supervisor.json").exists());
}
