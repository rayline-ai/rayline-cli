//! Local mode's refusal of a harness command line that resumes or forks a
//! saved session (spec section 3.2 item 9). Each harness's forms come from
//! its own CLI: Claude Code 2.1.330, Codex 0.170, pi and OpenCode. Only the
//! arguments before any `--` are read.

use std::ffi::OsString;
use std::io;

/// Refuse `args` when they resume or fork a saved session of `harness`.
pub(crate) fn require_fresh_session(harness: &str, args: &[OsString]) -> io::Result<()> {
    let args: Vec<String> = args
        .iter()
        .take_while(|arg| *arg != "--")
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let resuming = match harness {
        "claude" => any_flag(
            &args,
            &[
                "--resume",
                "--continue",
                "--fork-session",
                "--resume-session-at",
                "--from-pr",
                "--teleport",
                "-r",
                "-c",
            ],
        ),
        "codex" => codex_resumes(&args),
        "pi" => any_flag(
            &args,
            &[
                "--resume",
                "--continue",
                "--fork",
                "--session",
                "--session-id",
                "-r",
                "-c",
            ],
        ),
        "opencode" => any_flag(&args, &["--continue", "--session", "--fork", "-c", "-s"]),
        _ => false,
    };
    if resuming {
        return Err(io::Error::other(
            "local mode requires a fresh harness session; saved sessions do not contain the core's private-append ledger",
        ));
    }
    Ok(())
}

/// Whether an argument is one of `flags`, alone or as `<flag>=<value>`.
fn any_flag(args: &[String], flags: &[&str]) -> bool {
    args.iter()
        .any(|arg| flags.contains(&arg.split('=').next().unwrap_or("")))
}

/// Codex options that take a value, across `codex`, `codex exec` and their
/// subcommands. `-i`/`--image` takes every following argument up to the next
/// option.
const CODEX_VALUE_OPTIONS: &[&str] = &[
    "-c",
    "--config",
    "--enable",
    "--disable",
    "--remote",
    "--remote-auth-token-env",
    "-m",
    "--model",
    "--local-provider",
    "-p",
    "--profile",
    "-s",
    "--sandbox",
    "-C",
    "--cd",
    "--add-dir",
    "-a",
    "--ask-for-approval",
    "--thread-source",
    "--cyber-access-program",
    "--output-schema",
    "--color",
    "-o",
    "--output-last-message",
];
const CODEX_IMAGE_OPTIONS: &[&str] = &["-i", "--image"];

/// Codex 0.170's commands: `codex`'s, then `codex exec`'s.
const CODEX_COMMANDS: &[&str] = &[
    "agents",
    "exec",
    "e",
    "review",
    "login",
    "logout",
    "mcp",
    "plugin",
    "app-server",
    "remote-control",
    "app",
    "completion",
    "update",
    "doctor",
    "sandbox",
    "debug",
    "apply",
    "a",
    "resume",
    "queue",
    "archive",
    "delete",
    "migrate-rollouts",
    "migrate-nfs-db",
    "unarchive",
    "fork",
    "cloud",
    "exec-server",
    "features",
    "help",
];
const CODEX_EXEC_COMMANDS: &[&str] = &["resume", "fork", "review", "help"];

/// Whether Codex's command is `resume` or `fork`, or `exec` (or its alias
/// `e`) with `resume` or `fork` as its command. Codex reads a command as the
/// first or, after a prompt, the second argument that is neither an option
/// nor an option's value, so `codex hello resume` resumes.
fn codex_resumes(args: &[String]) -> bool {
    let Some(command) = codex_command(args, 0, CODEX_COMMANDS) else {
        return false;
    };
    match args[command].as_str() {
        "resume" | "fork" => true,
        "exec" | "e" => codex_command(args, command + 1, CODEX_EXEC_COMMANDS)
            .is_some_and(|sub| matches!(args[sub].as_str(), "resume" | "fork")),
        _ => false,
    }
}

/// Index of Codex's command among `args[start..]`: the first positional
/// argument when it is one of `commands`, else the second, which follows a
/// prompt.
fn codex_command(args: &[String], start: usize, commands: &[&str]) -> Option<usize> {
    let first = codex_positional(args, start)?;
    if commands.contains(&args[first].as_str()) {
        return Some(first);
    }
    codex_positional(args, first + 1)
}

/// Index of the first positional argument at or after `start`.
fn codex_positional(args: &[String], start: usize) -> Option<usize> {
    let mut index = start;
    while let Some(arg) = args.get(index) {
        if !arg.starts_with('-') || arg == "-" {
            return Some(index);
        }
        index += 1;
        if arg.contains('=') || (!arg.starts_with("--") && arg.len() > 2) {
            // `--opt=value`, `-o=value` or `-ovalue`: the value is attached.
            continue;
        }
        if CODEX_IMAGE_OPTIONS.contains(&arg.as_str()) {
            while args.get(index).is_some_and(|value| !value.starts_with('-')) {
                index += 1;
            }
        } else if CODEX_VALUE_OPTIONS.contains(&arg.as_str()) {
            index += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::require_fresh_session;
    use std::ffi::OsString;

    fn refused(harness: &str, args: &[&str]) -> bool {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        require_fresh_session(harness, &args).is_err()
    }

    #[test]
    fn refuses_each_harness_s_resume_and_fork_forms() {
        for (harness, args) in [
            ("claude", &["--resume=saved"][..]),
            ("claude", &["--continue"]),
            ("claude", &["-r"]),
            ("claude", &["--from-pr", "42"]),
            ("claude", &["--from-pr"]),
            ("claude", &["--teleport=abc"]),
            ("codex", &["resume"]),
            ("codex", &["fork", "--last"]),
            ("codex", &["exec", "resume", "id"]),
            ("codex", &["exec", "--skip-git-repo-check", "resume", "id"]),
            ("codex", &["-m", "x", "exec", "-m", "y", "fork", "id"]),
            ("codex", &["-c", "a=b", "--enable", "f", "resume"]),
            ("codex", &["e", "--model=x", "resume", "--last"]),
            ("codex", &["-mx", "exec", "-i", "a.png", "--json", "resume"]),
            ("codex", &["exec", "-ia.png", "resume"]),
            ("codex", &["hello", "resume", "--last"]),
            ("codex", &["exec", "hello", "resume", "--last"]),
            ("codex", &["hello", "exec", "fork", "id"]),
            ("codex", &["hello", "-m", "x", "fork"]),
            ("pi", &["--fork"]),
            ("pi", &["--session=saved"]),
            ("opencode", &["-c"]),
            ("opencode", &["--session=ses_1"]),
        ] {
            assert!(refused(harness, args), "{harness} {args:?}");
        }
    }

    #[test]
    fn passes_fresh_sessions() {
        for (harness, args) in [
            ("claude", &["-p", "Explain resume support"][..]),
            ("codex", &["exec", "resume the build"]),
            ("codex", &["exec", "-i", "a.png", "resume"]),
            ("codex", &["-m", "resume"]),
            ("codex", &["exec", "-c", "resume", "do it"]),
            ("codex", &["exec", "review"]),
            ("codex", &["exec", "review", "resume"]),
            ("codex", &["review", "fork"]),
            ("codex", &["help", "resume"]),
            ("codex", &["hello", "world"]),
            ("codex", &["exec", "hello"]),
            ("codex", &["exec", "--", "resume"]),
            ("codex", &["--", "resume"]),
        ] {
            assert!(!refused(harness, args), "{harness} {args:?}");
        }
    }
}
