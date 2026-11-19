//! `rayline run claude -p`: Claude Code writes `[claude-code:unrecognized_model]
//! {json}` to stderr in print mode for every model id it doesn't know, which
//! includes each ARC model id Rayline sets. Rayline still runs Claude Code in
//! its own place, but with its stderr going to a relay process (Rayline's
//! internal `__claude-stderr` entry point), which drops the lines naming one of
//! those ids and passes every other byte on as it arrives.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::{Command, ExitCode};

const NOTICE_PREFIX: &[u8] = b"[claude-code:unrecognized_model] ";
/// A held line longer than this is not a notice; it is passed on.
const MAX_NOTICE_LEN: usize = 4096;
/// The env vars through which Rayline names models for Claude Code.
const MODEL_ENVS: [&str; 7] = [
    "ANTHROPIC_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_CUSTOM_MODEL_OPTION",
];

/// The model ids `command` sets through [`MODEL_ENVS`], plus `extra`.
fn launch_model_ids(command: &Command, extra: &[&str]) -> Vec<String> {
    let mut ids: Vec<String> = command
        .get_envs()
        .filter(|(key, _)| MODEL_ENVS.iter().any(|name| key == name))
        .filter_map(|(_, value)| value.and_then(|value| value.to_str()))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect();
    ids.extend(extra.iter().map(|id| (*id).to_owned()));
    ids
}

/// Whether `line` (without its newline) is Claude Code's unrecognized-model
/// notice for one of `ids`.
fn is_own_notice(line: &[u8], ids: &[String]) -> bool {
    let Some(json) = line.strip_prefix(NOTICE_PREFIX) else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(json)
        .ok()
        .and_then(|notice| notice.get("model")?.as_str().map(str::to_owned))
        .is_some_and(|model| ids.contains(&model))
}

/// Whether a line starting with `start` can still be a notice: the prefix,
/// then a JSON object.
fn could_start_notice(start: &[u8]) -> bool {
    match start.strip_prefix(NOTICE_PREFIX) {
        Some(payload) => payload.first().is_none_or(|&byte| byte == b'{'),
        None => NOTICE_PREFIX.starts_with(start),
    }
}

/// Removes own notices from a byte stream. A line is held only while it can
/// still become a notice; any other byte is returned by the `feed` call that
/// received it.
struct NoticeFilter {
    ids: Vec<String>,
    /// The start of the current line, while it matches [`NOTICE_PREFIX`].
    held: Vec<u8>,
    /// The current line is known not to be a notice.
    passing: bool,
}

impl NoticeFilter {
    fn new(ids: Vec<String>) -> Self {
        Self {
            ids,
            held: Vec::new(),
            passing: false,
        }
    }

    /// Returns the bytes of `chunk` to pass on now.
    fn feed(&mut self, mut chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len());
        while !chunk.is_empty() {
            let line_end = chunk.iter().position(|&byte| byte == b'\n');
            let (piece, rest) = match line_end {
                Some(index) => chunk.split_at(index + 1),
                None => (chunk, &[][..]),
            };
            chunk = rest;
            if self.passing {
                out.extend_from_slice(piece);
                self.passing = line_end.is_none();
            } else {
                self.held.extend_from_slice(piece);
                let complete = line_end.is_some();
                let could_be_notice =
                    could_start_notice(&self.held) && self.held.len() <= MAX_NOTICE_LEN;
                if !could_be_notice {
                    out.append(&mut self.held);
                    self.passing = !complete;
                } else if complete {
                    let line = self.held.strip_suffix(b"\n").unwrap_or(&self.held);
                    if !is_own_notice(line, &self.ids) {
                        out.extend_from_slice(&self.held);
                    }
                    self.held.clear();
                }
            }
        }
        out
    }

    /// Returns what is held at the end of the stream: a last line without a
    /// newline, unless it is an own notice.
    fn finish(&mut self) -> Vec<u8> {
        let held = std::mem::take(&mut self.held);
        if is_own_notice(&held, &self.ids) {
            Vec::new()
        } else {
            held
        }
    }
}

/// Copies `input` to `output` through a [`NoticeFilter`] until `input` ends.
/// A failed write doesn't stop it, so Claude Code never blocks on a full pipe.
fn relay(mut input: impl Read, mut output: impl Write, ids: Vec<String>) {
    let mut filter = NoticeFilter::new(ids);
    let mut pass = |bytes: Vec<u8>| {
        if !bytes.is_empty() {
            let _ = output.write_all(&bytes).and_then(|()| output.flush());
        }
    };
    let mut buf = [0u8; 8192];
    loop {
        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => pass(filter.feed(&buf[..read])),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    pass(filter.finish());
}

/// Ctrl-C, Ctrl-\ and a terminal hangup are Claude Code's to act on. The
/// relay keeps passing on what Claude Code writes until its stderr ends.
fn ignore_keyboard_signals() {
    for number in [libc::SIGINT, libc::SIGQUIT, libc::SIGHUP] {
        // SAFETY: sets this process's action for `number` to ignore;
        // signal(2) is async-signal-safe.
        unsafe {
            libc::signal(number, libc::SIG_IGN);
        }
    }
}

/// The relay process: `rayline __claude-stderr <id>...` copies its stdin to
/// its stdout, dropping the notices for those ids, until its stdin ends.
pub(crate) fn relay_main(ids: Vec<OsString>) -> ExitCode {
    // Also set before exec by relay_stderr, which covers the relay's start;
    // here it holds for the entry point however it was started.
    ignore_keyboard_signals();
    let ids = ids
        .into_iter()
        .filter_map(|id| id.into_string().ok())
        .collect();
    relay(std::io::stdin().lock(), std::io::stdout().lock(), ids);
    ExitCode::SUCCESS
}

/// Points the stderr of `command`, a `-p` launch of Claude Code, at a new
/// relay process that drops the notices for the models `command` sets and for
/// `extra`, and writes the rest to Rayline's stderr. When the relay can't be
/// started, `command` is left as it was.
pub(crate) fn relay_stderr(command: &mut Command, extra: &[&str]) {
    let ids = launch_model_ids(command, extra);
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Ok((reader, writer)) = std::io::pipe() else {
        return;
    };
    let mut relay = Command::new(exe);
    // SAFETY: the hook runs in the forked child before exec and calls only
    // signal(2), which is async-signal-safe.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut relay, || {
            ignore_keyboard_signals();
            Ok(())
        });
    }
    // Not waited for: Claude Code replaces Rayline next, and the relay exits
    // when Claude Code's stderr ends.
    #[allow(clippy::zombie_processes)]
    let started = relay
        .arg(crate::cli::CLAUDE_STDERR)
        .args(&ids)
        .stdin(reader)
        .stdout(std::io::stderr())
        .spawn()
        .is_ok();
    if started {
        command.stderr(writer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        vec!["arc-sage-1.0".to_owned(), "local/arc-sage-1.0".to_owned()]
    }

    fn filtered(chunks: &[&[u8]]) -> Vec<u8> {
        let mut filter = NoticeFilter::new(ids());
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(filter.feed(chunk));
        }
        out.extend(filter.finish());
        out
    }

    const OWN: &[u8] =
        b"[claude-code:unrecognized_model] {\"model\":\"arc-sage-1.0\",\"query_source\":\"sdk\"}\n";

    #[test]
    fn a_notice_for_an_id_rayline_set_is_dropped() {
        assert_eq!(filtered(&[OWN]), b"");
        let local = b"[claude-code:unrecognized_model] {\"query_source\":\"agent:x\",\"model\":\"local/arc-sage-1.0\",\"new\":1}\n";
        assert_eq!(
            filtered(&[b"before\n", local, b"after\n"]),
            b"before\nafter\n"
        );
    }

    #[test]
    fn a_notice_for_another_id_is_kept() {
        let typo = b"[claude-code:unrecognized_model] {\"model\":\"arc-sagee-1.0\",\"query_source\":\"sdk\"}\n";
        assert_eq!(filtered(&[typo]), typo);
        for kept in [
            &b"[claude-code:unrecognized_model] not json\n"[..],
            b"[claude-code:unrecognized_model] {\"model\":7}\n",
            b"[claude-code:unrecognized_model]{\"model\":\"arc-sage-1.0\"}\n",
            b"x [claude-code:unrecognized_model] {\"model\":\"arc-sage-1.0\"}\n",
        ] {
            assert_eq!(filtered(&[kept]), kept);
        }
    }

    #[test]
    fn other_bytes_pass_unchanged_and_at_once() {
        let mut filter = NoticeFilter::new(ids());
        // A partial line that can't be a notice is passed on before its newline.
        assert_eq!(
            filter.feed(b"Error: \xff\xfe partial"),
            b"Error: \xff\xfe partial"
        );
        assert_eq!(filter.feed(b" rest\n[claude"), b" rest\n");
        // A held start that stops matching the prefix is released whole.
        assert_eq!(filter.feed(b"-code:other] x"), b"[claude-code:other] x");
        assert_eq!(filter.feed(b"\n"), b"\n");
        assert_eq!(filter.finish(), b"");
    }

    #[test]
    fn a_notice_split_across_reads_or_without_newline_is_still_matched() {
        let (head, tail) = OWN.split_at(20);
        assert_eq!(filtered(&[head, tail, b"next\n"]), b"next\n");
        let bytes: Vec<&[u8]> = OWN.chunks(1).collect();
        assert_eq!(filtered(&bytes), b"");
        assert_eq!(filtered(&[b"a\n", &OWN[..OWN.len() - 1]]), b"a\n");
        let unfinished = b"[claude-code:unrecognized_model] {\"model\":\"arc-s";
        assert_eq!(filtered(&[unfinished]), unfinished);
    }

    #[test]
    fn a_prefixed_line_that_is_not_json_is_passed_on_at_once() {
        let mut filter = NoticeFilter::new(ids());
        let line = b"[claude-code:unrecognized_model] not json";
        assert_eq!(filter.feed(line), line);
        let mut filter = NoticeFilter::new(ids());
        assert_eq!(filter.feed(&line[..33]), b"");
        assert_eq!(filter.feed(&line[33..]), &line[..]);
    }

    #[test]
    fn a_long_line_with_the_prefix_is_not_held_forever() {
        let mut filter = NoticeFilter::new(ids());
        let mut long = NOTICE_PREFIX.to_vec();
        long.resize(MAX_NOTICE_LEN + 1, b'x');
        assert_eq!(filter.feed(&long), long);
        assert_eq!(filter.feed(b"y\n"), b"y\n");
    }

    #[test]
    fn relay_copies_until_the_input_ends() {
        let input: Vec<u8> = [&b"one\n"[..], OWN, b"two"].concat();
        let mut output = Vec::new();
        relay(&input[..], &mut output, ids());
        assert_eq!(output, b"one\ntwo");
    }

    #[test]
    fn the_ids_are_the_model_envs_the_launch_sets() {
        let mut command = Command::new("claude");
        command
            .env("ANTHROPIC_MODEL", "arc-sage-1.0")
            .env("ANTHROPIC_DEFAULT_HAIKU_MODEL", "arc-swift-1.0")
            .env("ANTHROPIC_BASE_URL", "https://example.test")
            .env_remove("CLAUDE_CODE_SUBAGENT_MODEL");
        let mut ids = launch_model_ids(&command, &["local/arc-sage-1.0"]);
        ids.sort();
        assert_eq!(ids, ["arc-sage-1.0", "arc-swift-1.0", "local/arc-sage-1.0"]);
    }
}
