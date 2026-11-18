//! The relay a `rayline run claude -p` launch puts Claude Code's stderr
//! through (docs/spec.md section 3.2 item 14), run as the installed binary.
#![cfg(unix)]

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const OWN: &str =
    "[claude-code:unrecognized_model] {\"model\":\"arc-sage-1.0\",\"query_source\":\"sdk\"}\n";
const TYPO: &str =
    "[claude-code:unrecognized_model] {\"model\":\"arc-sagee-1.0\",\"query_source\":\"sdk\"}\n";

struct Relay {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    /// What the relay writes, read by a thread so a wait can time out.
    out: mpsc::Receiver<Vec<u8>>,
}

impl Relay {
    fn start(ids: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rayline"))
            .arg("__claude-stderr")
            .args(ids)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("relay starts");
        let mut stdout = child.stdout.take().expect("stdout");
        let (send, out) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(read @ 1..) = stdout.read(&mut buf) {
                if send.send(buf[..read].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            out,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.stdin
            .as_mut()
            .expect("stdin open")
            .write_all(bytes)
            .expect("write");
    }

    /// The next `bytes` bytes the relay writes, within a few seconds.
    fn expect(&self, bytes: usize) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while got.len() < bytes {
            let left = deadline.saturating_duration_since(Instant::now());
            got.extend(
                self.out
                    .recv_timeout(left)
                    .expect("the relay held output back"),
            );
        }
        got
    }

    /// Closes the relay's input and returns the rest of its output once it
    /// has exited successfully.
    fn finish(mut self) -> Vec<u8> {
        drop(self.stdin.take());
        assert!(self.child.wait().expect("relay exits").success());
        self.out.iter().flatten().collect()
    }
}

#[test]
fn the_relay_drops_only_the_notices_for_its_ids() {
    let mut relay = Relay::start(&["arc-sage-1.0", "arc-swift-1.0"]);
    relay.write(
        &[
            "Error: something\n".as_bytes(),
            OWN.as_bytes(),
            TYPO.as_bytes(),
            b"\xff\xfe binary\n",
            b"no newline",
        ]
        .concat(),
    );
    assert_eq!(
        relay.finish(),
        [
            "Error: something\n".as_bytes(),
            TYPO.as_bytes(),
            b"\xff\xfe binary\n",
            b"no newline",
        ]
        .concat()
    );
}

#[test]
fn the_relay_passes_a_partial_line_on_before_its_newline() {
    let mut relay = Relay::start(&["arc-sage-1.0"]);
    relay.write(b"Working...");
    assert_eq!(relay.expect(10), b"Working...");
    assert!(relay.finish().is_empty());
}

#[test]
fn the_relay_outlives_a_keyboard_interrupt_until_its_input_ends() {
    let mut relay = Relay::start(&["arc-sage-1.0"]);
    relay.write(b"ready\n");
    assert_eq!(relay.expect(6), b"ready\n");
    for signal in ["-INT", "-QUIT", "-HUP"] {
        let sent = Command::new("kill")
            .args([signal, &relay.child.id().to_string()])
            .status()
            .expect("kill runs");
        assert!(sent.success());
    }
    // What Claude Code writes as it stops still comes through.
    relay.write(b"Interrupted\n");
    assert_eq!(relay.expect(12), b"Interrupted\n");
    assert!(relay.finish().is_empty());
}
