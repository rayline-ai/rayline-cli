//! What `rayline local start` shows on stderr while it fetches the ARC
//! release and starts the local process. On a terminal each phase is one
//! line that updates in place; otherwise a download prints a line when it
//! starts, about every 10 seconds or 10 %, and when it ends. A closed
//! stderr is ignored.

use std::cell::{Cell, RefCell};
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

/// How often a terminal line is redrawn.
const REDRAW: Duration = Duration::from_millis(200);
/// How often a non-terminal download prints a line without reaching the
/// next tenth.
const EVERY: Duration = Duration::from_secs(10);

pub struct Progress {
    out: RefCell<Box<dyn Write + Send>>,
    tty: bool,
    /// A terminal line is drawn without its newline.
    open: Cell<bool>,
    state: RefCell<Transfer>,
}

/// The ARC release's bytes: those expected, those of files already done,
/// and those of the file now downloading. `fresh` counts only bytes this
/// start downloaded, not those a resumed download already had.
#[derive(Default)]
struct Transfer {
    total: u64,
    done: u64,
    current: u64,
    /// The file now downloading has reported its bytes on disk.
    base: Option<u64>,
    fresh: u64,
    started: Option<Instant>,
    shown: Option<Instant>,
    tenths: u64,
    verifying: bool,
}

impl Progress {
    /// Progress on stderr, updating in place when stderr is a terminal.
    pub fn stderr() -> Self {
        Self::new(Box::new(io::stderr()), io::stderr().is_terminal())
    }

    /// Progress nobody sees.
    pub fn silent() -> Self {
        Self::new(Box::new(io::sink()), false)
    }

    pub fn new(out: Box<dyn Write + Send>, tty: bool) -> Self {
        Self {
            out: RefCell::new(out),
            tty,
            open: Cell::new(false),
            state: RefCell::new(Transfer::default()),
        }
    }

    /// A finished phase: its own line.
    pub fn line(&self, text: &str) {
        self.open.set(false);
        self.write(&if self.tty {
            format!("\r\x1b[K{text}\n")
        } else {
            format!("{text}\n")
        });
    }

    /// A short phase that only a terminal shows, until the next line
    /// replaces it.
    pub fn status(&self, text: &str) {
        if self.tty {
            self.open.set(true);
            self.write(&format!("\r\x1b[K{text}"));
        }
    }

    /// A phase under way: on a terminal, a line the next one replaces;
    /// elsewhere, its own line.
    pub fn phase(&self, text: &str) {
        self.open.set(self.tty);
        self.write(&if self.tty {
            format!("\r\x1b[K{text}")
        } else {
            format!("{text}\n")
        });
    }

    /// Clears a terminal's unfinished line, so the next output starts clean.
    pub fn clear(&self) {
        if self.open.replace(false) {
            self.write("\r\x1b[K");
        }
    }

    /// `bytes` more will be downloaded. A total that grows after the
    /// download looked complete (a cached file that didn't match) is not
    /// verifying yet, and counts its tenths afresh.
    pub fn expect(&self, bytes: u64) {
        let mut state = self.state.borrow_mut();
        state.total += bytes;
        state.verifying = false;
        state.tenths = ((state.done + state.current) * 10)
            .checked_div(state.total)
            .unwrap_or(0);
    }

    /// The file downloading now has `bytes` on disk.
    pub fn on_bytes(&self, bytes: u64) {
        let line = {
            let mut state = self.state.borrow_mut();
            // The file's first report is what it already had; after that,
            // each rise is new bytes, and a drop is a restart from zero.
            if state.base.replace(bytes).is_some() {
                state.fresh += bytes.checked_sub(state.current).unwrap_or(bytes);
            }
            state.current = bytes;
            let now = Instant::now();
            let started = *state.started.get_or_insert(now);
            let got = state.done + state.current;
            let total = state.total.max(got);
            if state.shown.is_none() {
                state.shown = Some(now);
                Some(if self.tty {
                    format!(
                        "Downloading ARC release: {}",
                        fraction(got, total, state.fresh, started)
                    )
                } else {
                    format!("Downloading ARC release: {} MB", megabytes(total))
                })
            } else {
                let tenths = (got * 10).checked_div(total).unwrap_or(0);
                let due = state
                    .shown
                    .is_some_and(|shown| now - shown >= if self.tty { REDRAW } else { EVERY });
                // The last bytes are always drawn, so a terminal's line ends
                // at 100%.
                let complete = got >= total;
                if due || complete || (!self.tty && tenths > state.tenths) {
                    state.shown = Some(now);
                    state.tenths = tenths;
                    Some(format!(
                        "Downloading ARC release: {}",
                        fraction(got, total, state.fresh, started)
                    ))
                } else {
                    None
                }
            }
        };
        if let Some(line) = line {
            self.phase(&line);
        }
        // The last bytes are in: the files are hashed before start goes on.
        let verifying = {
            let mut state = self.state.borrow_mut();
            let done = !state.verifying && state.done + state.current >= state.total;
            state.verifying |= done;
            done
        };
        if verifying {
            self.phase("Verifying the ARC release...");
        }
    }

    /// The file downloading now is complete.
    pub fn file_done(&self) {
        let mut state = self.state.borrow_mut();
        state.done += state.current;
        state.current = 0;
        state.base = None;
    }

    /// The release is in the cache: says whether it was downloaded, or
    /// was there already.
    pub fn release_ready(&self) {
        let (started, got, fresh) = {
            let state = self.state.borrow();
            (state.started, state.done, state.fresh)
        };
        match started {
            Some(started) => {
                let seconds = started.elapsed().as_secs_f64();
                self.line(&format!(
                    "ARC release downloaded: {} MB in {seconds:.1} s ({})",
                    megabytes(got),
                    rate(fresh, seconds)
                ));
            }
            None => self.line("ARC release cached"),
        }
    }

    #[cfg(test)]
    fn fresh(&self) -> u64 {
        self.state.borrow().fresh
    }

    fn write(&self, text: &str) {
        let mut out = self.out.borrow_mut();
        let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
    }
}

impl Drop for Progress {
    /// A start that fails mid-line leaves the error its own line.
    fn drop(&mut self) {
        self.clear();
    }
}

/// `got / total MB (p%), r MB/s`, the rate of the `fresh` bytes once a
/// second has passed.
fn fraction(got: u64, total: u64, fresh: u64, started: Instant) -> String {
    let percent = (got * 100).checked_div(total).unwrap_or(100);
    let seconds = started.elapsed().as_secs_f64();
    let shown = format!("{} / {} MB ({percent}%)", megabytes(got), megabytes(total));
    if seconds < 1.0 {
        shown
    } else {
        format!("{shown}, {}", rate(fresh, seconds))
    }
}

fn rate(bytes: u64, seconds: f64) -> String {
    if seconds <= 0.0 {
        return "- MB/s".to_owned();
    }
    format!("{:.0} MB/s", bytes as f64 / 1e6 / seconds)
}

/// Whole megabytes (10^6 bytes) with thousands separators.
fn megabytes(bytes: u64) -> String {
    let digits = (bytes / 1_000_000).to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A writer whose bytes the test reads back.
    #[derive(Clone, Default)]
    pub(crate) struct Captured(pub(crate) Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("captured lock")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        pub(crate) fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("captured lock")).into_owned()
        }
    }

    #[test]
    fn a_terminal_line_ends_at_100_percent_and_never_reads_it_early() {
        let out = Captured::default();
        let progress = Progress::new(Box::new(out.clone()), true);
        let total = 1_112_400_000;
        progress.expect(total);
        progress.on_bytes(0);
        // Within a redraw interval of the first frame: drawn only once complete.
        progress.on_bytes(total - 100_000);
        progress.on_bytes(total);
        let text = out.text();
        let frames: Vec<&str> = text.split("\r\x1b[K").filter(|f| !f.is_empty()).collect();
        assert_eq!(
            frames[frames.len() - 2],
            "Downloading ARC release: 1,112 / 1,112 MB (100%)",
            "{text}"
        );
        assert_eq!(frames[frames.len() - 1], "Verifying the ARC release...");
        let started = Instant::now();
        assert!(fraction(total - 100_000, total, 0, started).ends_with("(99%)"));
    }

    #[test]
    fn megabytes_have_thousands_separators() {
        assert_eq!(megabytes(999_999), "0");
        assert_eq!(megabytes(1_018_125_184), "1,018");
        assert_eq!(megabytes(12_345_678_901), "12,345");
    }

    #[test]
    fn off_a_terminal_a_download_prints_a_line_per_tenth_and_no_carriage_returns() {
        let out = Captured::default();
        let progress = Progress::new(Box::new(out.clone()), false);
        progress.expect(100_000_000);
        for step in 1..=100 {
            progress.on_bytes(step * 1_000_000);
        }
        progress.file_done();
        progress.release_ready();
        let text = out.text();
        assert!(!text.contains('\r'), "{text}");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Downloading ARC release: 100 MB");
        assert_eq!(lines.len(), 1 + 10 + 1 + 1, "{text}");
        assert!(lines[10].starts_with("Downloading ARC release: 100 / 100 MB (100%)"));
        assert_eq!(lines[11], "Verifying the ARC release...");
        assert!(lines[12].starts_with("ARC release downloaded: 100 MB in "));
    }

    #[test]
    fn a_resumed_download_rates_only_its_new_bytes() {
        let progress = Progress::new(Box::new(io::sink()), false);
        progress.expect(100);
        progress.on_bytes(90);
        progress.on_bytes(95);
        progress.on_bytes(100);
        assert_eq!(progress.fresh(), 10);

        // A retry that restarts from zero counts every byte it downloads.
        let progress = Progress::new(Box::new(io::sink()), false);
        progress.expect(100);
        for bytes in [90, 95, 0, 100] {
            progress.on_bytes(bytes);
        }
        assert_eq!(progress.fresh(), 105);
    }

    #[test]
    fn a_total_that_grows_after_completing_verifies_again_at_its_end() {
        let out = Captured::default();
        let progress = Progress::new(Box::new(out.clone()), false);
        progress.expect(10_000_000);
        progress.on_bytes(0);
        progress.on_bytes(10_000_000);
        progress.file_done();
        progress.expect(10_000_000);
        progress.on_bytes(0);
        progress.on_bytes(10_000_000);
        let text = out.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("Verifying"))
                .count(),
            2,
            "{text}"
        );
        assert_eq!(
            lines.last(),
            Some(&"Verifying the ARC release..."),
            "{text}"
        );
    }

    #[test]
    fn a_terminal_line_left_open_is_cleared_when_progress_ends() {
        let out = Captured::default();
        drop({
            let progress = Progress::new(Box::new(out.clone()), true);
            progress.status("Checking the ARC release...");
            progress
        });
        assert!(out.text().ends_with("\r\x1b[K"), "{:?}", out.text());
        let out = Captured::default();
        drop(Progress::new(Box::new(out.clone()), true));
        assert_eq!(out.text(), "");
    }

    #[test]
    fn a_cached_release_is_one_line() {
        let out = Captured::default();
        let progress = Progress::new(Box::new(out.clone()), false);
        progress.release_ready();
        assert_eq!(out.text(), "ARC release cached\n");
    }

    #[test]
    fn a_closed_stderr_is_ignored() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
        }
        let progress = Progress::new(Box::new(Closed), true);
        progress.expect(10);
        progress.on_bytes(5);
        progress.release_ready();
    }
}
