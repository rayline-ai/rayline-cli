# Rayline CLI specification

This is the contract for the `rayline` CLI: every command, option, config key, env var and file it has. Code implements exactly this. Anything not listed here does not exist: no extra flags, keys, env vars, aliases or files.

Sections marked **Internal** are real but stay out of public docs and `--help`. Sections marked **Kept for review** keep today's behaviour unchanged for now and get their own review later; they may not grow.

## 1. Rules

1. **Fail fast.** Rayline validates `settings.json`, env overrides and flags once, at load, before any command runs. Every command that uses `credentials.json` (`rayline auth status`, `rayline auth logout`, a harness launch, starting the local process) loads and validates it before it acts; `rayline local status`, `rayline local stop`, `rayline top` and `rayline update` don't read it. A local-mode launch that reuses a running local process reads only that process's state (`supervisor.json`, `gateway-token`), not `credentials.json`. `rayline auth login` replaces an invalid `credentials.json` with the new session and says so, because signing in is how the user repairs it (section 6). Otherwise an invalid value stops Rayline with a clear error that names the key or flag. After load, the code trusts the parsed config: no checks, defaults or fallbacks at call sites.
2. **No silent fallback.** A missing or invalid value is an error. It is never skipped, guessed or replaced by another source. One exception: with `local` decisions, a `local.inference.models` entry the ARC release can't route to is skipped with a warning (section 5.4 item 3).
3. **One spelling, one value.** Each key, flag and env var has one name and each meaning has one value. No aliases, no alternative spellings, no deprecated names.
4. **No compatibility code.** No migration of old files, keys, names or install layouts, except `settings.json` (section 5.9). Old files are ignored, not read and not moved.
5. **No hard-coded models in the local process.** The candidates ARC picks from, their context windows, output limits and prices come from ARC or from `settings.json`, never from constants in code, except the three cloud models in section 5.3 item 2, which stay until the Rayline router publishes ARC's model set.
6. **No new surface.** Nothing is added that this spec does not list.

## 2. Command name

The installed binary is `rayline` (`~/.rayline/bin/rayline`). It has no other name: no alias, symlink or link to it is installed.

## 3. Public CLI

```
Usage: rayline [OPTIONS] COMMAND [ARGS]...

Options:
  --version        Show version
  --help           Show this message and exit

Commands:
  auth             Sign in, sign out, show status, print a token
  run              Launch a coding harness on ARC
  local            Start, stop or inspect the local Rayline process
  top              Show live request metrics
  update           Check for or install a Rayline update

Usage: rayline run <HARNESS> [--mode <cloud|local>] [--config <file>] [HARNESS ARGS]...

Harnesses:
  claude           Claude Code
  codex            Codex CLI
  opencode         opencode
  pi               pi
  omp              oh-my-pi
  hermes           Hermes Agent
  openclaw         OpenClaw

Options:
  --mode <cloud|local>  The mode for this run, over the config's `mode`;
                        local runs through the local Rayline process
  --config <file>       Use <file> instead of ~/.config/rayline/settings.json
  --isolated            claude only: use a separate Claude Code config folder

Codex:
  rayline run codex app [WORKSPACE]   Launch the Codex desktop app
  rayline run codex configure         Write a Rayline profile for Codex

Arguments after the harness name, or after --, go to the harness.
```

### 3.1 `rayline auth`

| Command | Does |
|---|---|
| `rayline auth login` | Sign in with Rayline session login in the browser. Writes `credentials.json`. |
| `rayline auth logout` | Revoke the stored session and remove the stored router key, also from the harness configs Rayline wrote. |
| `rayline auth status` | Show the signed-in account and environment. |
| `rayline auth token` | Print a valid account bearer token. |

`rayline auth login` options are **kept for review** (section 9).

### 3.2 `rayline run <HARNESS>`

```
rayline run <HARNESS> [--mode <cloud|local>] [--config <file>] [HARNESS ARGS]...
```

1. The config comes from `~/.config/rayline/settings.json`, or from `<file>` with `--config <file>`, which replaces it (never merges). The mode is `--mode`, else `RAYLINE_MODE`, else the config's `mode` (section 5). `--mode` and `--config` combine.
2. **Cloud mode**: no local process. The harness talks to Rayline Platform directly, with the models in `cloud` (section 5.1), which the config must have. When a local process is running, the run first prints to stderr `Note: a local process is running; this run uses Rayline Platform. Add --mode local to use the local process.`
3. **Local mode**: the harness talks to the local process, which is started if it isn't running. The config must have `local.routing` (section 5.4), except while a local process started from the `--router` defaults runs (section 3.3 item 11).
4. A `--mode` value other than `cloud` or `local` is a usage error, and so is `--local`.
5. Rayline options come first, right after the harness name. The first argument that is not a Rayline option, or everything after `--`, goes to the harness unchanged, including `--help`. When the harness arguments are exactly one of `--help`, `-h`, `--version` and `-v`, for `claude`, `codex` and every harness in the list above, Rayline runs the harness with that argument alone and does nothing else: it reads no config or credentials, signs nobody in, starts no local process and doesn't check Claude Code's daemon. A harness not on `PATH` fails with ``Error: `<harness>` not found on PATH``.
6. `rayline run --help` prints the usage above. `rayline run <HARNESS>` with an unknown harness is a usage error.
7. Each harness keeps today's behaviour in both modes, apart from what this spec removes.
8. Local mode refuses these kept options and commands (section 9) before anything starts: `rayline run claude --isolated`, `rayline run codex app` and `rayline run codex configure`. A lone `--help`, `-h`, `--version` or `-v` (item 5) runs the harness first, so `rayline run claude --mode local --isolated --help` prints Claude Code's help. `app` and `configure` are Rayline commands only as the word right after `rayline run codex`. After Rayline's options they are codex arguments, so `rayline run codex --mode local app` passes `app` to codex.
9. For `claude`, `codex`, `pi` and `opencode`, local mode refuses a command line that resumes or forks a saved session, before anything starts. It looks at the harness arguments before any `--`, in both the `--flag` and `--flag=value` forms, and refuses these:
   - `claude`: `-r`, `-c`, `--resume`, `--continue`, `--fork-session`, `--resume-session-at`;
   - `codex`: `resume` or `fork` as the first argument, or `exec resume` as the first two;
   - `pi`: `-r`, `-c`, `--resume`, `--continue`, `--fork`, `--session`, `--session-id`;
   - `opencode`: `-c`, `-s`, `--continue`, `--session`, `--fork`.

   The arguments of other harnesses are not checked: `hermes --resume` and `openclaw --session-id`, for example, are passed on.
10. Local mode refuses OpenClaw container mode, before anything starts: `rayline run openclaw` when `OPENCLAW_CONTAINER` is set to a non-empty value, or when any harness argument is `--container` or `--container=<value>`.
11. Before every `rayline run hermes` launch, cloud or local mode, on every OS, Rayline runs `hermes --version` with the launch's environment (`HERMES_HOME` set to Rayline's Hermes home). When Hermes refuses and names `hermes pm repair` (a home with no dependency environment), Rayline runs `hermes pm repair` with that home once, then goes on; later launches of the same Hermes on an unchanged home run neither. When `hermes --version` can't run, or fails without naming the repair, the launch stops with that error or Hermes' last output and the home. A failed repair stops it with Hermes' last output and the command to run by hand with that home. In cloud mode this runs before the version check below.

    Before a cloud-mode launch, on every OS, Rayline runs `hermes pm install --extra anthropic` with the launch's environment (`HERMES_HOME` set to Rayline's Hermes home), once per Hermes install and home; later launches skip it. If the install fails or cannot run, the launch stops with Hermes' last output and the command to run by hand with that home. Before that install, `hermes --version` must print a `Hermes Agent v<version>` line naming 0.21.6 or later, the first Hermes with `hermes pm`; when it prints none, or an older version, the launch stops with an error that names the minimum version and the install commands `curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash` and, in Windows PowerShell, `iex (irm https://hermes-agent.nousresearch.com/install.ps1)`. Local mode doesn't run the install or this check.
12. Local mode refuses `rayline run pi` before anything starts when `pi` isn't on `PATH`, or when `pi --version` can't run, exits non-zero, prints no version or prints one older than 0.87.1 (an older pi sends no session id). When pi is older, the error names the minimum version and `npm install -g @earendil-works/pi-coding-agent`.
13. `rayline run openclaw`, in cloud and local mode, refuses before writing its config when `openclaw --version` can't run, exits non-zero, prints no version or prints one older than 2026.9.6 (an older OpenClaw rejects the whole config over fields it doesn't know). The error names the minimum version and `npm install -g openclaw`.
14. Rayline sets each launch up for ARC and passes the harness arguments through unread, a harness's own model flag included. The endpoint decides what a model id means:
    - cloud mode: the Rayline router serves the model id the harness sends, or refuses it;
    - local mode: the declared model is `local/<id>`, where `<id>` is `local.routing.decisions_model.id` (for example `local/arc-sage-1.0`), and every harness offers it in its model picker; a model id the harness sends is a request, and ARC decides which model runs;
    - a harness may change its own request for the model it named, and the endpoint answers that request. Codex sends GPT reasoning fields and non-function tools for a model id it recognises as a GPT reasoning model (for example `codex -m openai/gpt-6.1-sol`); in local mode the core refuses that request with 400 naming the field, before anything runs. OpenClaw sends its session header only for a model Rayline declares; in local mode any other `--model` is refused by the core with 400 session_identity_required.
    - a harness's own config pins override the environment Rayline sets, for example a subagent model pinned in Claude Code's `settings.json`.
14. `rayline run claude` with `-p` or `--print`, in both modes, on Linux and macOS: Claude Code's stderr goes to a relay process, which passes it on to Rayline's stderr as it arrives, except each line that starts with `[claude-code:unrecognized_model] ` and whose JSON `model` is a model id the launch set: `ANTHROPIC_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`, `ANTHROPIC_DEFAULT_FABLE_MODEL`, and the declared `local/<id>` in local mode. Claude Code then runs in Rayline's place, in local mode too, so stdin, stdout, signals and the exit status are Claude Code's own. The relay runs from the `rayline` binary through an internal entry point that is not a user command and is not in `--help`. It ignores SIGINT, SIGQUIT and SIGHUP and exits when Claude Code's stderr ends, so it may write Claude Code's last lines just after Claude Code exits. When the relay can't be started, the launch goes on with Claude Code's stderr not read. Without `-p`, and on Windows, Claude Code's stderr is not read.

Harness-specific options and subcommands are **kept for review** (section 9).

### 3.3 `rayline local`

| Command | Does |
|---|---|
| `rayline local start [--config <file>] [--router <cloud\|local>]` | Start the local process from the `local` section of the default config file, or of `<file>`, whatever `mode` says. The config must have `local.routing`, except with `--router` (item 11). While it waits for the process, stderr shows `Starting the local process (ARC encoder and execution core)...` with `router` `local`, else `Starting the local process...`, and the ready line ends `(started in <s> s)`. |
| `rayline local status [--json]` | Show whether the local process is running, its address, the config file and the environment it was started from. `--json` prints it as one JSON object (item 12). |
| `rayline local stop` | Stop the local process. External inference servers keep running. |

1. One local process per machine, with its state in `~/.rayline/local/`.
2. `start` while it is running is an error that names the running process and its config file.
3. A local-mode run whose config file differs from the one the running process was started from, or whose `--env` differs, or whose `local.routing.router` (and, for `local`, ARC release: its repo, commit, directory and manifest sha256) differs, or whose `local.routing.decisions_model.id` differs (harnesses declare `local/<id>` from it), is an error. It never silently uses the running process's config.
4. On macOS, `rayline local` and local-mode runs require macOS 13 or later.
5. Starting the local process downloads the tokenizer of each `local.inference.models` entry that names one (section 5.3) into the Hugging Face cache (section 8.2). When Hugging Face doesn't answer within 20 seconds or a file fails to download, it uses the last complete copy in the cache and says that it is the cached copy. With no complete cached copy, start fails with an error that names the config file, the `models` entry and the tokenizer.
6. The token counter reads the tokenizer repo's files in the layouts Hugging Face models publish:
   - `tokenizer.json`;
   - the chat template: `chat_template.jinja`, else the `chat_template` key of `tokenizer_config.json` (a string, or named templates `default` and `tool_use`);
   - image sizing: `processor_config.json`, else `preprocessor_config.json`. In its `image_processor` object, else at its top level: `patch_size` and `merge_size`, and the image limits `min_image_tokens` and `max_image_tokens`, else `min_pixels` and `max_pixels`, else `size.min_pixels` and `size.max_pixels`, else `size.shortest_edge` and `size.longest_edge`. Gemma 4's processor gives `patch_size` and `max_soft_tokens` and no `merge_size`: every image counts as `max_soft_tokens` plus its begin-image and end-image tokens.

   An image given only by URL, or whose bytes don't decode as an image, counts as the processor's maximum.
7. The local process passes each request on with a session id and a branch id:
   - session: the request's `x-rayline-session-id`, else the first of `x-conversation-id`, `x-parent-session-id`, `x-session-affinity`, `x-session-id`, `x-claude-code-session-id` that it carries, else none;
   - branch: the request's `x-rayline-branch-id`, else `thread-id`, else `x-claude-code-agent-id`, else, only when the request carries `x-parent-session-id`, its `x-session-affinity`, else its `x-session-id`; else `main`.
8. When the core fails a turn before any output because its call to the routing service failed, the local process resends the turn once, and only when the service was unavailable (no answer, or HTTP 408, 409 or 5xx) or busy (HTTP 429). It doesn't resend when the service says `x-should-retry: false` or a `retry-after` over 30 seconds. Before the resend it waits the service's `retry-after` (at least 1 second), else about 5 seconds, jittered between 2.5 and 7.5 so sessions don't resend in step.

   When the turn still fails, or isn't resent, the harness gets the error with `x-should-retry: false`, so its SDK doesn't resend it again:
   - unavailable: 503, and busy: 429. Both carry `retry-after`: the service's (which the core caps at 300 seconds, and the local process raises to at least 1), else 30 seconds.
   - a rejected router key: 403, and any other refusal: 400. Neither is resent, and neither carries `retry-after`.

   The core passes on the service's two headers separately: `Retry-After` only as delta-seconds, and `X-Should-Retry` only as exactly `true` or `false`. A refusal can carry one without the other, and the local process uses the one it gets. A missing header takes its own default, and only for an unavailable or busy service. Without `retry-after`, the local process waits about 5 seconds for its resend and tells the harness `retry-after: 30`. Without `x-should-retry`, the turn may be resent. When the service doesn't answer, the core passes on neither.

   With `router` `local`, the error says local ARC refused or couldn't decide the turn on this machine, and a refusal points at `local.routing.decisions_model` and `local.inference.models`.
9. When the core fails a turn before any output because it couldn't reach the endpoint the decision selected, the local process doesn't resend it, and the harness gets 502 with `x-should-retry: false`. The error names Rayline Platform when every model is Rayline Platform's, else "Rayline Platform, or an inference server in `local.inference.endpoints`", and says to check the network and any `HTTPS_PROXY` the local process was started with.
10. `runtime.log` gets a line, never with the request's content, for every request the local process refuses (an untrusted origin, a missing token, a body over 64 MiB: status and path) and every error the core answers with (status, path and error type; a 502 before any output as item 8 or 9 classifies it).
11. `--router` sets `local.routing.router`, over the file and `RAYLINE_LOCAL_ROUTING_ROUTER`. With `--router` and no `local.routing` in the config, start uses `local.routing` with that `router`, `decisions_model` `{ "id": "arc-swift-1.0" }`, `policy.preset` `arc-policy.prefer-local.v1` and no `classifier_model`, for `cloud` and `local` alike; the `RAYLINE_LOCAL_ROUTING_*` overrides apply to those defaults as to a file's `local.routing`. The process's `supervisor.json` records that it started from them (`"routing_defaults": true`), and while it runs, a local-mode `rayline run <harness>` whose config has no `local.routing` uses the same defaults and runs on it. Whenever the started `local.routing` (these defaults or a file's) has no `classifier_model`, start ends with a note: Claude Code needs `local.routing.classifier_model`, followed by a `settings.json` in local mode with that `local.routing` and `"classifier_model": "<model id>"`.
12. `rayline local status --json` prints `{"running": false}` when nothing runs. Otherwise:

   | Key | Value |
   |---|---|
   | `running` | `true` |
   | `pid` | The local process's pid. |
   | `url` | The gateway address, as `status` prints it. |
   | `config` | The config file it was started from. |
   | `environment` | `prod` or the `--env` name. |
   | `router` | `cloud` or `local`. |
   | `decisions_model` | `local.routing.decisions_model.id`. |
   | `routes` | With `local` decisions only: `url`, the local Routes API (`POST /v1/routes`), and `token_file`, the file holding its bearer token. |

   The token is never printed. Other local programs (pi-arc, for one) read it from `token_file` and send `Authorization: Bearer <token>`. It admits `POST /v1/routes` only; any other method or path is refused 403, so it can't send anything to Rayline Platform. The local process writes the file (mode 0600) when it starts with `local` decisions and removes it when it stops; a new start makes a new token.

### 3.4 `rayline top`

```
rayline top [--json]
```

1. Shows live request metrics from the local process.
2. `--json` prints one snapshot as JSON and exits.
3. The live view (a terminal, without `--json`) waits for a local process: while none runs it shows `Local process: not running` and checks again every refresh. When the local process restarts, it follows the new one. `--json`, or output that isn't a terminal, with no local process running exits with the error ``no local process is running. Start one with `rayline local start` or `rayline run <harness> --mode local`.``
4. The live view's header has five lines:
   - `Rayline top`, then right-aligned `local process · up <uptime> · <HH:MM>`;
   - `Routing   <ARC name> decides <where> · policy: <preset name>`, where `<ARC name>` is the decisions model as a product name (`arc-swift-1.0` → `ARC Swift`), `<where>` is `on this Mac` (macOS), `on this PC` (Linux, Windows) or `in Rayline Platform`, and `<preset name>` is the preset in words (`arc-policy.prefer-local.v1` → `prefer local`; an unknown preset shows its id);
   - `Ran on    Rayline Platform <n> · your servers <n>`;
   - `Tokens    <in> in (<p>% cached) · <out> out`, where `<p>` is all cache reads over all input tokens;
   - `Decisions <n> · median <ms> · <n> errors`.
   Request counts, tokens and errors are since the local process started. The decision count and median cover the requests in the list.
5. Below the header is one list, newest first. Requests in flight are at the top, with a spinner and their elapsed time in place of the result columns. Columns:
   `TIME` (`HH:MM`), `TOOL` (the harness, or `—`), `TURN` (the decision event in words: `task start`, `tool result`, ...; `—` for none), `DECIDED`, `MODEL`, `RAN ON`, `IN`, `CACHED`, `OUT`, `TTFT`, `TOK/S`.
   - `DECIDED`: `Mac <ms> →` or `this PC <ms> →` for a local decision, `Platform <ms> →` for a cloud one (`Platform →` when the core reports no time), `—` for a request no decision routed, such as a classifier call.
   - `MODEL`: the model that served the request.
   - `RAN ON`: `Platform`, or the key of the `local.inference.endpoints` entry it ran on, in the config the local process started from.
   - `CACHED`: that request's cache reads over its input tokens, as a percentage.
   Token counts of 1,000 or more show as `12.1k`; times under 1 s as `122 ms`, others as `0.9 s`. A value the request doesn't have is blank.
6. The selected row has a `▸ why:` line beneath it, giving the decision's reason in words:
   - `prefer_local_threshold` → `local model good enough (pass <p_pass> ≥ <min_p_pass>)`;
   - `utility_fallback` → `best value for this task`, plus `· no local model configured` when `local.inference.models` is empty or missing in the config the local process started from;
   - `schedule` → `scheduled`.
   A thinking level other than `none` adds `· thinking <level>`.
7. Keys: `↑`/`↓` select, `Enter` opens the details of the selected request (`Esc` closes them), `f` cycles the list's filter through all requests and each harness that has requests, `q` or `Ctrl-C` quits. The footer shows them.
8. The details view of a request:
   ```
    <HH:MM:SS>  <harness> · <turn> · <request id>
    Decided   <ARC name> on this Mac in <ms> (<restored> of <encoded> prompt tokens reused)
    Picked    <model> · thinking <level> · reason: <why>
    Ran on    <Rayline Platform | server> · TTFT <s> · <out> tokens at <tok/s> tok/s
    Input     <in> · cache read <n> · cache write <n> · uncached <n>
   ```
   For a cloud decision, `Decided` reads `<ARC name> in Rayline Platform in <ms>`, with no reuse count. Request ids appear only here. ARC's prompt-token reuse appears only here and is never counted in `CACHED`, which is the provider's prompt cache.
9. When the terminal is too narrow for every column, columns are dropped in this order until the rows fit: `TOK/S`, `TTFT`, `CACHED`, `IN`, `TOOL`. Rows never wrap; a cell too long for its column is cut with `…`.
10. With no local process, the live view shows `Local process: not running` (item 3).
11. `--json` prints the snapshot. Its fields are only ever added; none is renamed or removed. Each request row has:
    - `router`: `local`, `cloud` or `null` (for a request no decision routed);
    - `decision_ms`: how long the decision took, local or cloud (`null` when the core reports none);
    - `decision_event`;
    - and the fields in item 12.
    The top-level `router` is `{"kind": "local" | "cloud", "model": <local.routing.decisions_model.id>, "preset": <local.routing.policy.preset>}`.
12. Fields added in 0.5.7:
    - row `harness`: `claude`, `codex`, `opencode`, `pi`, `omp`, `hermes`, `openclaw`, or `null`;
    - row `thinking_level`: the level the decision picked, or `null`;
    - row `decision_reason`: `prefer_local_threshold`, `utility_fallback`, `schedule`, or `null`;
    - row `p_pass` and `min_p_pass`: numbers with `prefer_local_threshold`, else `null`;
    - row `arc_encoded_tokens` and `arc_restored_tokens`: local decisions only, else `null`;
    - row `cache_write_tokens`: provider cache writes, or `null` when the provider reports none. `prompt_cache_tokens` remains the cache reads;
    - `totals.cache_read_tokens`;
    - `router.preset`.

### 3.5 `rayline update`

```
rayline update
rayline update --check
```

Its options are **kept for review** (section 9). The channel comes from `update.channel` (section 5.6).

With no `--version` or `--force`, when the running version is newer than the channel's latest release, `rayline update` and `rayline update --check` replace nothing, exit 0 and print ``Already up to date: <version> is newer than the latest <channel> release (<latest>). To install that release instead, run: rayline update --version <latest>``, where `<version>` is the running version without its build label and `<latest>` is the release's full version. When the running version is the latest, they print ``Already on latest (<version>).``

`--version` takes a full version (`0.5.3+3bc25bc98f31`) or one without its build label (`0.5.3`). One without that names the running version is the running version. For any other, `rayline update` reads the full version from the signed `latest.txt` of the GitHub release `v<version>` (`https://github.com/rayline-ai/rayline-cli/releases/download/v<version>/latest.txt` and its `.minisig`), checked against the same keys as the channel pointer. It fails with ``release <version> does not exist`` when that release has none, and with ``release <version> names version <full>`` when its `latest.txt` names another version.

`rayline update` installs the ARC encoder (section 5.4 item 2) only when `~/.rayline/arc/` already holds one: it then downloads the new version's encoder files into `~/.rayline/arc/<version>/`, each checked against the release's signed `SHA256SUMS`, and after a successful update, while no local process runs, removes every other version directory in `~/.rayline/arc/` (a running local process restarts its encoder from its own). Otherwise it downloads no encoder. Downloads stream to disk; one fails when connecting takes over 30 seconds or no data arrives for 60 seconds.

`rayline update` refuses Homebrew installs: with any of its options (`--check`, `--version`, `--force`, `--dry-run`), it replaces nothing and exits 0. When the binary it would replace is under `brew --prefix` (with `brew` on `PATH`), it prints ``Installed via Homebrew. Run `brew upgrade rayline` to update.``

When `brew` is on `PATH` but `brew --prefix` fails or prints nothing, `rayline update` fails: it replaces nothing, prints ``Error: `brew --prefix` (<brew>) failed: <reason>`` to stderr and exits 1. `<brew>` is the path of the `brew` it ran, and `<reason>` is the exit status, the error from starting it, ``it printed no prefix`` or ``it printed a prefix that is not UTF-8``.

### 3.6 Installers

`scripts/install-rayline.sh` (macOS and Linux) and `scripts/install-rayline.ps1` (Windows) install `rayline`, `rayline-core` and `rayline-core-LICENSE`, and nothing else; the ARC encoder comes with the first `local` start (section 5.4 item 2). Their public options:

1. `install-rayline.sh`:
   - `--version <version>`: the release to install, a bare version such as `0.5.0`. A version with a leading `v` is rejected. Without it, the installer takes the latest GitHub release.
   - `--install-dir <path>`: the directory the files go into. Default `~/.rayline/bin`. It replaces `RAYLINE_INSTALL_DIR`, which is not read.
2. `install-rayline.ps1`: `-Version <version>` and `-InstallDir <path>`, with the same rules.
3. To pass options to a piped installer:

```
curl -fsSL https://get.rayline.ai/install.sh | bash -s -- --version 0.5.0 --install-dir ~/bin
& ([scriptblock]::Create((irm https://get.rayline.ai/install.ps1))) -Version 0.5.0 -InstallDir C:\tools\rayline
```

The hidden `--base-url` option is in section 4.4.

## 4. Internal CLI

1. `--env <name>`, hidden, a global option placed before the command: `rayline --env dev run claude`. It selects `environments.<name>` from the config file and from `credentials.json`. Without it, prod (built in). An unknown name is a load error. `rayline local status`, `rayline local stop` and `rayline top` act on the one running local process, so `--env` with them is a usage error.
2. `rayline update` hidden release-testing options: `--base-url <url>` (where releases are read from) and `--install-path <path>` (which binary is replaced). They replace the env vars `RAYLINE_UPDATE_BASE_URL` and `RAYLINE_UPDATE_INSTALL_PATH`.
3. The local process runs from the `rayline` binary itself, through an internal entry point that `rayline local start` uses. It is not a user command and is not in `--help`. There is no separate `rld` binary.
4. `install-rayline.sh` hidden release-testing option `--base-url <url>`: where the release is read from. With `--version` it is a usage error. It replaces `RAYLINE_RELEASE_BASE_URL`.

## 5. `settings.json`

One file: `~/.config/rayline/settings.json`. `--config <file>` takes a file of the same shape, which replaces it (never merges).

| Key | Type | Required | Meaning |
|---|---|---|---|
| `mode` | `cloud` or `local` | yes | The mode a `rayline run` uses (section 3.2). |
| `cloud` | object (section 5.1) | in cloud mode | The models a cloud-mode run uses. |
| `local` | object: `inference` (`endpoints` and `models`, sections 5.2 and 5.3) and `routing` (section 5.4) | in local mode, and for `rayline local start` | The local process's setup. |
| `environments` | object (section 5.5) | no | Internal environments. |
| `update` | object (section 5.6) | no | The release channel. |

1. A file may hold both `cloud` and `local`. The section a command uses must be present; the other may be absent. A section the file has must be complete, whichever command reads it. `rayline local start` reads `local` whatever `mode` says.
2. The whole file is validated at load, including the section the command doesn't use: an unknown key, a wrong value or a missing required key anywhere is a load error (section 5.8).
3. When `~/.config/rayline/settings.json` doesn't exist, a command that reads it writes the default file (example 1) and prints to stderr ``Created <file> with the default settings (`mode` `cloud`)``. Every command reads it, `auth`, `top`, `update`, `local status` and `local stop` included, except `--version`, `--help`, a usage error and a command with `--config <file>`. A default file with no `mode` is converted the same way (section 5.9).
4. `settings.json` has no `$schema` key. `schema/settings.schema.json` (JSON Schema draft 2020-12) describes the shape for readers and tests. Rayline doesn't read it: load is the only validator.

The three setups:

| Setup | `mode` | `local.routing.router` | ARC decides | Inference runs |
|---|---|---|---|---|
| Cloud mode | `cloud` | – | in Rayline Platform | on Rayline Platform |
| Local mode, cloud ARC | `local` | `cloud` | in Rayline Platform | on the user's servers or Rayline Platform |
| Local mode, local ARC | `local` | `local` | on the user's machine | on the user's servers or Rayline Platform |

Example 1, cloud mode (the default file):

```json
{
  "mode": "cloud",

  "cloud": {
    "model": { "id": "arc-sage-1.0" },
    "smol_model": { "id": "arc-swift-1.0" }
  }
}
```

Example 2, local mode with cloud ARC:

```json
{
  "mode": "local",

  "cloud": {
    "model": { "id": "arc-sage-1.0" },
    "smol_model": { "id": "arc-swift-1.0" }
  },

  "local": {
    "inference": {
      "endpoints": {
        "gpu-box": {
          "base_url": "https://gpu-box.example.com:8443",
          "protocols": ["anthropic_messages", "openai_chat", "openai_responses"],
          "api_key_env": "GPU_BOX_API_KEY"
        }
      },
      "models": {
        "z-ai/glm-5.3-flash": {
          "endpoint": "gpu-box",
          "model": "GLM-5.3-Flash-EXL3",
          "context_window": 1000000,
          "max_output_tokens": 32000,
          "tokenizer": "zai-org/GLM-5.3-Flash"
        }
      }
    },
    "routing": {
      "router": "cloud",
      "decisions_model": { "id": "arc-swift-1.0" },
      "classifier_model": "anthropic/claude-haiku-5.5",
      "policy": {
        "preset": "arc-policy.prefer-local.v1",
        "overrides": {
          "selection": { "min_p_pass": 0.5 },
          "schedule": { "model": ["cache_boundaries", "user_messages"] }
        }
      }
    }
  },

  "environments": {
    "dev": {
      "base_url": "https://api.dev.example.com",
      "login_url": "https://platform.dev.example.com/cli-auth"
    }
  }
}
```

Example 3, local mode with local ARC:

```json
{
  "mode": "local",

  "cloud": {
    "model": { "id": "arc-sage-1.0" },
    "smol_model": { "id": "arc-swift-1.0" }
  },

  "local": {
    "inference": {
      "endpoints": {
        "gpu-box": {
          "base_url": "https://gpu-box.example.com:8443",
          "protocols": ["anthropic_messages", "openai_chat", "openai_responses"],
          "api_key_env": "GPU_BOX_API_KEY"
        }
      },
      "models": {
        "z-ai/glm-5.3-flash": {
          "endpoint": "gpu-box",
          "model": "GLM-5.3-Flash-EXL3",
          "context_window": 1000000,
          "max_output_tokens": 32000,
          "tokenizer": "zai-org/GLM-5.3-Flash"
        }
      }
    },
    "routing": {
      "router": "local",
      "decisions_model": { "id": "arc-swift-1.0" },
      "classifier_model": "anthropic/claude-haiku-5.5",
      "policy": {
        "preset": "arc-policy.prefer-local.v1",
        "overrides": {
          "selection": { "min_p_pass": 0.5 },
          "schedule": { "model": ["cache_boundaries", "user_messages"] }
        }
      }
    }
  },

  "environments": {
    "dev": {
      "base_url": "https://api.dev.example.com",
      "login_url": "https://platform.dev.example.com/cli-auth"
    }
  }
}
```

Examples 2 and 3 differ in one line, `router`. Their `cloud` section is optional; with it, `--mode cloud` works on the same file. `classifier_model` is needed only to run Claude Code in local mode (section 5.4).

With `router` `cloud`, `decisions_model` can ask for ARC's answer at a `v_value` (section 5.4):

```json
"decisions_model": { "id": "arc-1.0", "v_value": 2 }
```

### 5.1 Rayline Platform and `cloud`

Rayline Platform (prod) is built in and has no entry. It serves every protocol a harness speaks. Login is always Rayline session login.

`cloud` holds the two models a cloud-mode run uses. A model is an object, `{ "id": "<ARC model id>" }`.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `model` | model object | yes | The harness's main model. |
| `model.id` | non-empty string | yes | Its ARC model id. |
| `smol_model` | model object | no, default `model` (the whole object) | The harness's small model. |
| `smol_model.id` | non-empty string | with `smol_model` | Its ARC model id. |

1. A model object in `cloud` has only `id`. Any other key, `v_value` included, is a load error, like any unknown key.
2. Rayline checks the shape of a model object, not its id: load holds no list of ids and reads none. It doesn't replace an id with the key's Contributor or zero data retention id. The id goes to the API as written, and the API validates it when the model is called (for example 400 `value_required`, 400 `tier_value_immutable`, 403 `zdr_model_required`).
3. A cloud-mode launch reads the key's model listing to set up the harness. The opencode, pi, omp, OpenClaw and Hermes configs list every model the listing has, with its name, limits and input types. Rayline doesn't check `cloud.model.id` or `cloud.smol_model.id` against the listing: an id the listing doesn't have is added to those configs by its id alone, with no limits (OpenClaw, which requires a name, gets the id as its name), and the harness uses its own defaults for it. Claude Code starts with `--settings` holding a `modelPicker` with `replaceBuiltInOptions`, so its `/model` shows its Default row and one row per model the listing has, `cloud.model.id` first, each labelled with its `display_name` and described by its id, and none of Claude Code's own models. A `cloud.model.id` or `cloud.smol_model.id` the listing doesn't have also gets a row, by its id alone, as in the other harnesses' configs. On Windows, when `claude` on `PATH` is npm's `claude.cmd`, `cmd.exe` runs it and limits the whole command line, this `--settings` included, to 8,191 characters: with around 100 listed models (fewer with long ids and names) it is longer, `cmd.exe` refuses it and Claude Code doesn't start. A native `claude.exe` has no such limit. The `--settings` comes before the harness arguments: a `--settings` the user passes replaces it, and `/model` then shows Claude Code's own rows. Claude Code's `opus` alias (`ANTHROPIC_DEFAULT_OPUS_MODEL`, also its default) is `cloud.model.id`, `haiku` (`ANTHROPIC_DEFAULT_HAIKU_MODEL`) is `cloud.smol_model.id`, and `sonnet` (`ANTHROPIC_DEFAULT_SONNET_MODEL`) and `fable` (`ANTHROPIC_DEFAULT_FABLE_MODEL`) take the listing's other models in its order, or `cloud.model.id` when none is left. Codex's `/model` lists only Codex's own models.
4. Where each harness uses the two models:

   | Harness | `model` | `smol_model` |
   |---|---|---|
   | Claude Code | `ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_OPUS_MODEL` | `CLAUDE_CODE_SUBAGENT_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL` |
   | opencode | `model` | `small_model` |
   | omp | roles `default`, `slow`, `plan` | role `smol` |
   | Codex | `model` | not used |
   | pi | `defaultModel` | not used |
   | Hermes | `model.default` | not used |
   | OpenClaw | `agents.defaults.model.primary` | not used |

5. Every launch, in either mode, sets these. pi's `settings.json` and omp's `config.yml` also hold settings the harness saves itself, so in those two files a launch sets only Rayline's keys (pi `defaultProvider` and `defaultModel`; omp `modelRoles` `default`, `smol`, `slow` and `plan`) and keeps every other entry as the harness wrote it. A model picked in pi or omp is replaced at the next launch. A file Rayline can't update that way (invalid JSON, or YAML laid out other than omp writes it, such as a non-empty flow collection or an anchor) is an error that names the file, and Rayline leaves it unchanged. Rayline doesn't otherwise check that `config.yml` is valid YAML: an invalid escape in a quoted value, for example, is kept as written.

### 5.2 `local.inference.endpoints`

`local.inference` is optional. Without it, local mode runs every model ARC picks on Rayline Platform.

Your own inference servers, keyed by id.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `base_url` | string (URL) | yes | The server's address. |
| `protocols` | list of `anthropic_messages`, `openai_chat`, `openai_responses` | yes, non-empty | The wire protocols it serves. |
| `api_key_env` | string | no | Name of the env var holding its key. No secrets in the file. |

In local mode each harness speaks one protocol, the only one its Rayline config sets up: `claude`, `pi`, `omp` and `opencode` speak `anthropic_messages`; `codex` and `hermes` speak `openai_responses`; `openclaw` speaks `openai_chat`. When the endpoint lists the harness's protocol, requests pass straight through with no translation. When it doesn't, the config is wrong and Rayline refuses to run. In cloud mode the config Rayline writes for `hermes` defaults to `anthropic_messages`, with an `openai_responses` provider beside it, and the one for `openclaw` adds an `openai_responses` provider beside `openai_chat`. Every other harness uses the same protocol in both modes.

### 5.3 `local.inference.models`

Models ARC can pick that are not served by Rayline Platform, keyed by the model id ARC returns.
The action ARC identifies is the model id after its provider prefix (`qwen/qwen3.8-27b` -> `qwen3.8-27b`).

| Key | Type | Required | Meaning |
|---|---|---|---|
| `endpoint` | string | yes | An id in `local.inference.endpoints`. |
| `model` | string | yes | The name that server knows the model by. |
| `context_window` | integer > 0 | yes, when the local process starts (item 5) | Context window in tokens. |
| `max_output_tokens` | integer > 0 | yes, when the local process starts (item 5) | Output ceiling in tokens. |
| `tokenizer` | string | no | Hugging Face tokenizer repo for token counting (item 7). |
| `price` | object `input`, `output`, `cache_read`, `cache_write` (numbers >= 0) | no | Overrides ARC's price for this model only. |

1. When ARC picks a model in `models`, the local process sends the request to its endpoint under its `model` name.
2. The only models ARC can pick that are not in `models` are these three Rayline Platform models. When ARC picks one, it goes to Rayline Platform. A request over Messages carries its Rayline Platform id, and one over Chat Completions or Responses carries its id in the core's registry:

   | Rayline Platform id | Id in the core's registry | Context window | Output ceiling | Input modalities |
   |---|---|---|---|---|
   | `claude-opus-5-5` | `anthropic/claude-opus-5.5` | 1,000,000 | 128,000 | `["text"]` |
   | `claude-sonnet-5-5` | `anthropic/claude-sonnet-5.5` | 1,000,000 | 128,000 | `["text"]` |
   | `claude-haiku-5-5` | `anthropic/claude-haiku-5.5` | 1,000,000 | 128,000 | `["text"]` |

   A `models` entry keyed by one of the registry ids replaces that cloud model. ARC prices the cloud models. This list goes when the Rayline router publishes ARC's model set (section 10.4 item 14).
3. Nothing marks a model as "local". Every entry works the same way.
4. Main/subagent routes are not supported.
5. ARC doesn't publish the models it knows yet, so it knows no model's limits. Every entry must state `context_window` and `max_output_tokens`, and the local process refuses to start without them (section 5.8 item 13).
6. In local mode Rayline tells every harness a context window of 1,000,000 tokens, whichever model serves the turn.
7. With a `tokenizer`, the local process sizes each request it sends to the model, over any protocol, in the model's own tokens, with the repo's tokenizer and chat template. A request holding content its chat template doesn't render as text, such as a document or a server tool, is sized from its bytes instead. Start refuses a tokenizer repo that publishes no chat template, as a base model's repo often doesn't; its instruct or chat variant does (section 5.8 item 15).

### 5.4 `local.routing`

How the local process picks a model. Required in local mode and for `rayline local start`, except with the `--router` defaults (section 3.3 item 11).

| Key | Type | Required | Meaning |
|---|---|---|---|
| `router` | `cloud` or `local` | yes | Where the local process gets its decisions. `cloud`: the environment's Rayline router decides. `local`: ARC runs inside the local process from the ARC release (item 1); no decision goes to Rayline Platform. |
| `decisions_model` | model object `{ "id": ..., "v_value": ... }` | yes | Which ARC model makes decisions. No default in code, except `rayline local start --router <cloud|local>` without `local.routing` (section 3.3 item 11). |
| `decisions_model.id` | non-empty string | yes | Its ARC model id (for example `arc-swift-1.0`). With `router` `local`, it must be `arc-swift-1.0` or `arc-sage-1.0` (or their short and data-mode spellings, such as `arc-swift` or `arc-sage-zdr-1.0`); any other value fails load naming the key. With `router` `cloud`, Rayline doesn't check it (section 5.1 item 2). |
| `decisions_model.v_value` | number >= 0 | no | With `router` `cloud`, the local process asks the Rayline router for ARC's `custom` answer at this value, sent as `routing.v_value`; without it, ARC's `top` answer. With `router` `local`, it is a load error. |
| `classifier_model` | string | to run Claude Code | The model that serves Claude Code's auto-mode safety classifier calls, sent to Rayline Platform. No default in code. A local-mode `rayline run claude` without it fails before anything starts, naming the key and showing a `settings.json` with the run's `local.routing` and `"classifier_model": "<model id>"`, and so does one that would use a local process started without it; such a process refuses classifier calls. |
| `policy.preset` | non-empty string | yes | ARC's preset id (`arc-policy.package-default.v1`, `arc-policy.hybrid-default.v1` or `arc-policy.prefer-local.v1`). Sent to ARC unchanged; no short names. The same exception as `decisions_model` applies. |
| `policy.overrides` | object | no | ARC's own `routing.policy.overrides` shape, sent to ARC unchanged. ARC validates it. |
| `arc_release.repo` | string `<owner>/<name>` | with `arc_release` | The Hugging Face repo of the ARC release `local` decisions download instead of the built-in one. |
| `arc_release.revision` | 40 lowercase hex digits | with `arc_release` | The repo's commit. |
| `arc_release.manifest_sha256` | 64 lowercase hex digits | with `arc_release` | The sha256 of the release's `manifest.json`. |
| `arc_release.dir` | relative path of plain names joined by `/` | no, default the repo root | The repo directory that holds the release. |

1. With `router` `local`, starting the local process loads the ARC runtime release at a pinned commit, before it reports ready:
   - The pin is `arc_release`, else the built-in release (`rayline-ai/ARC-1.0` at a commit, directory and manifest sha256 compiled into Rayline).
   - `manifest.json` must match the pinned sha256 (and be at most 1 MiB), and every file it lists the size and sha256 it lists. A cached file that matches is not downloaded again; one that doesn't is downloaded again. A cached file is hashed again on every start, except on macOS and Linux when it is unchanged since it last hashed to the pinned sha256: after a hash, Rayline keeps a stamp of the file it hashed (real path, size, modification and inode change times, device and inode) in `~/.rayline/arc-verified/<sha256>`, but only when both times were at least 2 seconds old before the hash began (so after downloading anything, start waits 2 seconds and then hashes the release's files, the encoder weights and the ARC encoder again, recording their stamps), and hashes again when any of them differs, the stamp is missing, unreadable or not a regular file, or the file changed while it was hashed. A download that doesn't match is removed from the cache, and start fails naming the file; so does a file that can't be downloaded. A `prices.json` the manifest doesn't list fails start naming it.
   - A private repo needs a Hugging Face token (section 7.1 item 6); without one, start fails saying to set `HF_TOKEN`.
   - Start shows the download on stderr as one transfer of the release's files, the encoder weights and, when it downloads it, the ARC encoder (item 2), in MB with its rate. On a terminal it is one line that updates in place. Elsewhere it prints a line when the download starts, at each further tenth or 10 seconds after the last line, `Verifying the ARC release...` once the last bytes are in, and `ARC release downloaded: <MB> in <s> s (<rate>)`. The rate counts only bytes this start downloaded. When nothing needs downloading, it prints only `ARC release cached`. A closed stderr doesn't stop start.
   - The encoder weights (about 1 GB) come from `rayline-ai/ARC-1.0` at the built-in release's commit and a sha256 compiled into Rayline, whatever `arc_release` says, without a token, by the same rules. A release in `arc_release` must be built for those weights; Rayline can't check that.
   - When neither the release nor the `local.inference.models` prices it takes price any model, start fails saying so.
   - Start still contacts the environment's Rayline router and checks each `models` entry's `tokenizer` (section 5.3), as with `cloud`.
2. With `router` `local`, the local process runs the ARC encoder `~/.rayline/arc/<version>/rayline-arc-encoder` (`rayline-arc-encoder.exe` on Windows), where `<version>` is the running `rayline`'s version, and looks for it nowhere else; an encoder beside `rayline` is not used. Releases ship it for macOS arm64, Linux (x86_64 and aarch64) and Windows x86_64. Before downloading anything, start checks the platform: on another platform it fails saying releases don't ship the encoder there; on Linux when `libcuda.so.1` can't be loaded, and on Windows when `nvcuda.dll` is missing, it fails saying the NVIDIA driver is required. These errors, and the one below for an encoder that would run on the CPU, end with the same fix: ``to use the cloud router instead, run `rayline local start --router cloud` ``. macOS arm64 needs no driver. The directory holds the encoder, `rayline-arc-encoder-LICENSE`, on Windows every `.dll` the release's `SHA256SUMS` lists (the cuBLAS DLLs the encoder loads), and that `SHA256SUMS` with its minisign signature. Each start checks the signature and each file's sha256, with the same stamps as the release's files (item 1). When any file is missing or doesn't match, start downloads it from that version's release, with the base URL and minisign-verified `SHA256SUMS` of `rayline update`; each download must match its sha256 and is moved into place by rename, the encoder with mode 0755 on macOS and Linux. After a download they are checked again with the release's files (item 1), which records their stamps. Start also removes the other versions' directories in `~/.rayline/arc/`. When the encoder finds no GPU it can use and would run on the CPU, start fails with an error that says so.
3. With `router` `local`, a `local.inference.models` entry the ARC release doesn't know, or that neither its `price` nor the release prices, is skipped: the local process doesn't route to it, and `rayline local start` prints one warning per skipped entry, naming it and why. Turns route among the remaining models. When every `models` entry is skipped, start fails with an error naming them. Item 5 is checked first.
4. With `router` `local`, every turn routes by a local ARC decision; there is no fallback. A turn the decider can't decide fails with the decider's error code: one whose request holds more than the decider reads (over a million JSON values; over 16 bytes of text, images not counted, per token of the encoder's largest context of 270,336 tokens; over 1,024 tools; or over 64 working directories), or holds content the decider can't encode yet (images and documents), and any turn while the encoder is down. `rayline run codex`, `hermes` and `openclaw` (harnesses that don't send Anthropic Messages) fail before anything starts, with an error saying to use `cloud`.
5. With `router` `local`, a `models` entry's `price` needs a release that takes caller prices; with one that doesn't, start fails naming each `local.inference.models.<id>.price` to remove.
6. With `router` `local`, when the encoder exits, the local process logs it and starts it again after 2, 4, then 8 seconds. After 3 restarts within 10 minutes it logs an error and stops restarting until `rayline local stop`.

### 5.5 Internal: `environments`

Top-level and optional. Not in public docs. Prod is built in. Internal users add other environments and pick one with `--env`.

```json
"environments": {
  "dev": {
    "base_url": "https://<dev API host>",
    "login_url": "https://<dev platform host>/cli-auth"
  }
}
```

| Key | Type | Required | Meaning |
|---|---|---|---|
| `base_url` | string (URL) | yes | The Rayline API for that environment: `https`, or `http` on a loopback host, with no `user:password@`. Any other URL is a load error naming the key and the URL. |
| `login_url` | string (URL) | yes | The page `rayline auth login` opens. |

An environment name is one or more ASCII letters, digits, `-` and `_`. The same rule applies to `--env`, to the names under `environments`, and to the environment slots in `credentials.json`. Any other name is an error that names it.

### 5.6 Internal: `update`

Top-level and optional. Not in public docs.

```json
"update": { "channel": "dev" }
```

| Key | Type | Required | Meaning |
|---|---|---|---|
| `channel` | `prod`, `dev` or `main` | no, default `prod` | The release channel `rayline update` follows: `prod` is public releases, `dev` release candidates, `main` a build from every merge to main. |

The channel the binary was built on is not used as a default.

### 5.7 Env overrides

Each scalar key Rayline defines can be overridden by one env var named `RAYLINE_<SECTION>_<KEY>`, built from its path with single underscores:

| Key | Env var |
|---|---|
| `mode` | `RAYLINE_MODE` |
| `cloud.model.id` | `RAYLINE_CLOUD_MODEL_ID` |
| `cloud.smol_model.id` | `RAYLINE_CLOUD_SMOL_MODEL_ID` |
| `local.routing.router` | `RAYLINE_LOCAL_ROUTING_ROUTER` |
| `local.routing.decisions_model.id` | `RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_ID` |
| `local.routing.decisions_model.v_value` | `RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE` |
| `local.routing.classifier_model` | `RAYLINE_LOCAL_ROUTING_CLASSIFIER_MODEL` |
| `local.routing.policy.preset` | `RAYLINE_LOCAL_ROUTING_POLICY_PRESET` |
| `update.channel` | `RAYLINE_UPDATE_CHANNEL` |

1. Precedence, lowest to highest: built-in defaults, the config file, env overrides, flags.
2. An override value is validated like the file value; an invalid one is a load error.
3. An override applies to the section a command uses. It replaces a value the file has; it never gives a required key the file lacks (section 5.8 item 5), though it may give an optional one (`cloud.smol_model.id`, `local.routing.decisions_model.v_value`, `local.routing.classifier_model`, `update.channel`). One for a section the file lacks is a load error for a command that uses that section (section 5.8 item 7), and is not read by one that doesn't.
4. Whole entries (`local.inference.endpoints.*`, `local.inference.models.*`, `environments.*`, `local.routing.arc_release`) and ARC's `policy.overrides` have no env override. Put them in a file and pass it with `--config`.
5. An env var named from a key path of the earlier shape (`RAYLINE_ROUTING_*`) is not read. No other `RAYLINE_*` env var changes Rayline's behaviour (section 7).

### 5.8 Load errors

Load refuses, naming the key:

1. An unknown key at any level Rayline defines, anywhere in the file, including the section the command doesn't use. The earlier shape's top-level `endpoints`, `models` and `routing` are unknown keys in a file with `mode`.
2. A wrong type, an empty string, or a value out of range.
3. In a local-mode run, a `local.inference.endpoints` entry that doesn't list the protocol of the harness being run. Cloud mode doesn't use endpoints, and `rayline local start` has no harness, so neither checks this.
4. A `local.inference.models` entry whose `endpoint` is not in `local.inference.endpoints`.
5. A file without `mode`, unless it is the default file in the earlier shape (section 5.9). A cloud-mode run without `cloud`. Local mode or `rayline local start` without `local` or `local.routing`. When the file has no `local` section, the error says `local` is required in local mode and ends ``See https://docs.rayline.ai/rayline-cli/local-mode``; it prints no section, model id, preset or endpoint. Which sections must be present depends on the command; `auth`, `top`, `update`, `local status` and `local stop` need neither. A section the file has missing a required key in the file, for every command: `cloud` without `model` or `model.id`, a `cloud.smol_model` without `id`, a `local.routing` without `router`, `decisions_model`, `decisions_model.id` or `policy.preset`. An env override doesn't give a missing required key.
6. `--env <name>` with no `environments.<name>` entry. `prod` gets no special case.
7. An env override for a section the file lacks, for a command that uses that section: `RAYLINE_CLOUD_*` without `cloud`, and `RAYLINE_LOCAL_ROUTING_*` without `local.routing`. `rayline local start --router <cloud|local>` with no `local.routing`, and a local-mode `rayline run <harness>` with no `local.routing` while a local process started from those defaults runs, are the exceptions: the overrides apply to the built-in defaults (section 3.3 item 11).
8. A `--config <file>` that doesn't exist. Only the default file is optional.
9. A key repeated in one JSON object, an empty map key, a URL that isn't absolute `http` or `https`, a duplicate in `protocols`, or a `price` missing any of its four fields.
10. A `local.routing.decisions_model.v_value` with `router` `local`, and, with `router` `local`, a `decisions_model.id` local decisions don't make (section 5.4). These are checked whichever section the command uses.
11. A `--config <file>` with no `mode`: it is not converted (section 5.9), and its keys of the earlier shape are unknown keys.

When the local process starts, before it serves anything, it also refuses:

12. An `api_key_env` naming an env var that isn't set.
13. A model entry that is missing `context_window` or `max_output_tokens`. ARC knows no model today, so this applies to every entry (section 5.3 item 5).
14. An `api_key_env` naming `RAYLINE_ROUTER_API_KEY`, which holds the Rayline router key.
15. A `tokenizer` repo that publishes no chat template: no `chat_template.jinja`, and in `tokenizer_config.json` no `chat_template` string or named `default` template. The error names the model's `tokenizer` key.

### 5.9 Migration from the earlier shape

1. A command that reads the default file (section 5 item 3) converts a default file that is in the earlier shape to this shape and writes it back. A file with no `mode` key is in the earlier shape, the shape of `settings.json` in the releases before this one.
2. The original is kept as `settings.json.bak` beside it, replacing any earlier `settings.json.bak`. The command prints to stderr ``Converted <file> from the earlier shape (`mode` `<mode>`); the original is <backup>``.
3. The conversion carries everything the file holds: every endpoint, model, `routing` key, environment and `update`. It drops nothing:

   | Earlier shape | This shape |
   |---|---|
   | `endpoints` | `local.inference.endpoints` |
   | `models` | `local.inference.models` |
   | `routing` | `local.routing` |
   | `routing.router` `hosted` | `local.routing.router` `cloud` |
   | `routing` with no `router` | `local.routing.router` `cloud` |
   | `routing.decisions_model` `"<id>"` | `local.routing.decisions_model` `{ "id": "<id>" }` |
   | `environments`, `update` | the same |
   | – | `cloud` of the default file (example 1) |
   | – | `mode`: `local` when the file has `routing` (the result has the shape of example 2 or 3), `cloud` when it has none |

4. Only a file that is valid in the earlier shape is converted. A file with no `mode` that holds a key the earlier shape doesn't have, or a value it refuses, is a load error that names the key as the earlier shape spells it and says the file is read as the earlier shape; nothing is written.
5. A `--config` file in the earlier shape is a load error that names the key (section 5.8 item 11). It is not converted.
6. Flags and env vars are not converted: `--local` and `--router hosted` are usage errors, and the earlier shape's env names are not read (section 5.7 item 5).

## 6. `credentials.json`

One file: `~/.config/rayline/credentials.json`. `rayline auth login` writes `session` and `router_key` together (it mints the router key at sign-in), `rayline auth logout` revokes the session, then removes them, cuts the router key out of the harness configs Rayline wrote (`~/.rayline/harness/*/` and the Codex `rayline` profile) and deletes Hermes' copies of its config in Rayline's Hermes home (`~/.rayline/harness/hermes/backups/config/`), and a token refresh rewrites `session` only, because refresh tokens rotate. When the router refuses the revoke (HTTP 401), logout still does all of this, then exits 1 with ``Logged out of <environment> on this machine. The router refused to revoke the session (HTTP 401<: body>), so the router key may still be valid on the router.`` Nothing else writes it. It is the only source for Rayline's session and router key: no env var or flag overrides it.

```json
{
  "session": {
    "email": "you@example.com",
    "subject": "uid_abc123",
    "access_token": "eyJ...",
    "access_token_expires_at": "2026-11-12T11:00:00Z",
    "refresh_token": "eyJ...",
    "refresh_token_expires_at": "2026-12-12T10:00:00Z",
    "signed_in_at": "2026-11-12T10:00:00Z"
  },
  "router_key": { "api_key": "rlk-...", "subject": "uid_abc123", "saved_at": "2026-11-12T10:00:05Z" },
  "environments": {
    "dev": {
      "session": { "...same shape..." },
      "router_key": { "...same shape..." }
    }
  }
}
```

1. Prod sits at the top level. `environments.<name>` holds the same two objects for an internal environment.
2. snake_case only. No `version` field, no `authKind`.
3. Each slot (the top level for prod, or `environments.<name>`) is either absent, meaning signed out, or holds both objects with every field shown. A slot with only one object, or a missing or invalid field, is a load error that tells the user to run `rayline auth login`. A missing file means signed out everywhere.
4. A file in today's shape is invalid: it is a load error that tells the user to run `rayline auth login`. It is not migrated.

## 7. Env vars

### 7.1 Read by Rayline

1. The settings overrides in section 5.7.
2. The env var each endpoint's `api_key_env` names.
3. OS: `PATH`, and the home directory. One function reads it, from `HOME` on unix and `USERPROFILE` on Windows. When that var is unset or empty, it is an error that names the var. There is no other source for the home directory.
4. Harness homes, read to find a harness's own files: `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `OPENCLAW_CONTAINER`.
5. `CLAUDE_CODE_DISABLE_AGENT_VIEW`, passed on to Claude Code unchanged. When it is `1`, `true`, `yes` or `on` (trimmed, any case, as Claude Code reads it), a cloud `rayline run claude` skips the reuse and conflict steps of the daemon check below: it never prompts or fails because a daemon is running. Rayline still reads `daemon.lock` and checks its `pid`, to keep the records of a running daemon's starter (last point below), so a failed liveness check is still an error.

   The daemon check, before each cloud `rayline run claude`:
   - A Claude Code daemon is running when `daemon.lock` in the launch's Claude config folder (`CLAUDE_CONFIG_DIR`, else `~/.claude`; with `--isolated`, `~/.rayline/harness/claude-code/`) names a `pid` that is alive. A missing or unreadable `daemon.lock`, or one that isn't JSON naming a positive `pid`, counts as no daemon running (Claude Code writes it non-atomically). When checking whether that process is alive fails, the launch fails with an error that names the pid.
   - Rayline reuses the running daemon when either:
     - its environment, where Rayline can read it (Linux and macOS), holds the launch's values of `RAYLINE_ENV_NAME`, `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, `CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK`, `CLAUDE_CODE_DISABLE_REFUSAL_RETRY`, `CLAUDE_CODE_TETHER_LIVE`, `CLAUDE_CODE_SUBAGENT_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL_FORCE`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`, `ANTHROPIC_DEFAULT_FABLE_MODEL` and `CLAUDE_CODE_MAX_CONTEXT_TOKENS`. `ANTHROPIC_BASE_URL` is compared without trailing slashes. An empty value of one of the last ten counts as unset, so an empty `ANTHROPIC_DEFAULT_HAIKU_MODEL` matches a launch that removes it; or
     - a record in `daemon-launches.json` (section 8.2) is for the process that started it (`spawnedBy.pid` in `daemon.lock`), with the same environment, router URL, auto-compact window and those switches, and the daemon started within 60 seconds after the record.
   - Any other running daemon conflicts. When stdin, stdout and stderr are all terminals, Rayline asks whether to stop it (`claude daemon stop --any`) and go on, to run isolated instead (offered only without `--isolated`; the isolated folder gets the same check), or to cancel. Rayline trims the answer and matches it ignoring ASCII case. An empty answer (Enter alone, whitespace only, or end of input such as Ctrl-D) runs isolated when that is offered, and otherwise stops it. `s`, `y` and `yes` stop it. `i` or `isolated` runs isolated, when offered. Any other answer cancels. When not all three are terminals, on cancel, or when flushing the prompt to stderr or reading the answer fails, the launch fails with an error. Its first lines name the daemon's pid and say whether it is a Rayline daemon (with its environment), a daemon that isn't Rayline's, or one Rayline couldn't verify. It goes on:

     ```
     To switch:
       1. Stop the current daemon:   <stop>
       2. Re-run:                    rayline [--env <name> ]run claude [--isolated ]<args>
     ```

     `<args>` are the launch's Claude Code arguments, else `agents`, and `--env <name>` is there when the launch passed it. In the isolated folder, `<stop>` is `claude daemon stop --any (with CLAUDE_CONFIG_DIR=<folder>)`, naming the isolated folder, and the rerun keeps `--isolated`. Otherwise `<stop>` is `claude daemon stop --any`, the rerun has no `--isolated`, and the message ends with `Or run alongside it in an isolated config dir (~/.rayline/harness/claude-code):` and that rerun with `--isolated`.

     When the answer is to stop it, Rayline runs `claude daemon stop --any` with `CLAUDE_CONFIG_DIR` set to the checked folder, and goes on when it succeeds. When it exits non-zero, the launch fails with ``` `claude daemon stop --any` exited with code <code>. Stop the daemon manually and re-run. ```, where `<code>` is `-1` when the process has no exit code. When it can't be run, the launch fails with ``` Could not run `claude daemon stop --any`: <error>. Stop the daemon manually and re-run. ```
   - Each cloud launch adds its record to `daemon-launches.json`. It drops each existing record that has no `ts` or is 24 hours old or older, except that it keeps, whatever their age, every record whose `pid` is a `spawnedBy.pid` that a check read from a live daemon's `daemon.lock`, before that check's prompt. That is the check of the launch's config folder or, when the user chose isolated at the prompt, both the default folder's check and the isolated folder's, since the default folder's daemon keeps running. A record stays kept when the user then stops its daemon at the prompt. A missing `daemon-launches.json` holds no records, and recording the launch creates it. One that exists but can't be read, or isn't a JSON array of objects, is an error. So is failing to write it: creating its folder, writing the temporary file, or renaming that into place. Each of these fails the launch.
6. Shared Hugging Face cache: `HF_HOME`, `HF_HUB_CACHE`, and the Hub's address `HF_ENDPOINT` (default `https://huggingface.co`; a value that isn't an absolute `http` or `https` URL, or that carries a user name, password, query or fragment, is an error naming the var and not echoing its value). The ARC release download reads a token for private repos from `HF_TOKEN`, else the file `$HF_HOME/token` (else `<home>/.cache/huggingface/token`). It sends the token only to the Hub's address, never to a host a download redirects to, and never prints it. It sends it only over `https`, or over plain `http` to a loopback address: with a token, any other plain-`http` `HF_ENDPOINT`, or a redirect to one, fails the download with an error saying so. A set but empty `HF_TOKEN` is an error naming it.

7. `GGML_METAL_TENSOR_ENABLE`: whether it is set. With `local` decisions on an Apple M4 (its CPU name, from `sysctl`), the ARC encoder starts with it set to `1` unless it is already set.

### 7.2 Set by Rayline for a harness

1. Claude Code: `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, in cloud mode `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL` and `ANTHROPIC_DEFAULT_FABLE_MODEL` (section 5.1 item 3), `CLAUDE_CODE_SUBAGENT_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL_FORCE`, `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK`, `CLAUDE_CODE_DISABLE_REFUSAL_RETRY`, `CLAUDE_CODE_TETHER_LIVE`, `RAYLINE_ENV_NAME`.
   - `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, in cloud mode: set to the router's auto-compact window for the main ARC model (the `ANTHROPIC_MODEL` value), or removed when the router gives none.
   - `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, in cloud mode: set to the main model's `context_length` from `GET /v1/models` when the router gives an auto-compact window (Claude Code doesn't know the id and would assume 200K) and the listing has `cloud.model.id`, removed otherwise; in local mode: the admitted context window.
   - `CLAUDE_CODE_AUTO_MODE_SERVER`, in local mode: always `0`.
2. Other harnesses: `CODEX_HOME` (cloud-mode Codex and the Codex desktop app, set to Rayline's own folder for it, section 8.2), `HERMES_HOME`, `PI_CODING_AGENT_DIR`, `OPENCODE_CONFIG`, `OPENCLAW_CONFIG_PATH`.
3. Removed from the Claude Code environment: `ANTHROPIC_API_KEY`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY`.
4. Removed from the `omp` environment: `CLAUDE_CONFIG_DIR`, `PI_PROFILE`, `OMP_PROFILE`.
5. Removed from every harness command Rayline starts: `RAYLINE_ROUTER_API_KEY`.
6. Removed in local mode: `RAYLINE_EXEC_AUTH_TOKEN`, and the env var each endpoint's `api_key_env` names, unless the harness's Rayline config sets that var to the local process's gateway token.
7. In local mode (`rayline run <harness> --mode local`, or a `"mode": "local"` file), the gateway address a harness gets (`ANTHROPIC_BASE_URL` for Claude Code, the provider base URL in the other harnesses' Rayline config) is the harness's own path on the local process's gateway, `<gateway>/h/<harness>`. The gateway serves `/h/<harness>/<path>` as `/<path>` and records the harness for `rayline top` (section 3.4 item 12). `/h/` with any other name is refused with HTTP 404.

### 7.3 Internal: handoff between Rayline's own processes

Set by one Rayline process and read only by a child it starts. Never read from the user's shell:

1. `RAYLINE_EXEC_AUTH_TOKEN`, `RAYLINE_EXEC_PARENT_PIPE`, `RAYLINE_TOKEN_COUNTER_KEY`, `RAYLINE_HYBRID_GATEWAY_TOKEN`, `RAYLINE_ROUTER_API_KEY`.

### 7.4 Build time only

`RAYLINE_VERSION`. Not read at runtime.

### 7.5 Test builds only

`RAYLINE_TEST_DETACH_HELPER`. Compiled only into test builds, not into the shipped binary.

### 7.6 Development only

Read by the shipped binary for developing local ARC decisions, and not supported: `RAYLINE_ARC_MODEL` and `RAYLINE_ARC_ENCODER` (the encoder weights and binary), and `RAYLINE_ARC_RELEASE` (a release directory). They replace the pinned, checked release and weights with files Rayline doesn't check.

Every other env var Rayline reads today is gone (section 10).

## 8. Files

### 8.1 `~/.config/rayline`

Only these files:

1. `settings.json`: written by the user, and by Rayline when it is missing (section 5 item 3) or in the earlier shape (section 5.9).
2. `settings.json.bak`: the file in the earlier shape that a conversion replaced (section 5.9).
3. `credentials.json`: written by `rayline auth login` and `rayline auth logout`.

### 8.2 `~/.rayline`

```
~/.rayline/
  bin/                          rayline, rayline-core, rayline-core-LICENSE
  arc/<version>/                with local decisions: the ARC encoder of
                                Rayline <version> (section 5.4 item 2):
                                rayline-arc-encoder (.exe on Windows),
                                rayline-arc-encoder-LICENSE; on Windows also
                                the cuBLAS .dll files; SHA256SUMS and
                                SHA256SUMS.minisig
  arc-verified/<sha256>         on macOS and Linux, owner-only: stamps of the
                                cached ARC files already hashed (section 5.4)
  local/                        local process state
    core-config.json            config handed to the core
    core-config.counted.json    the same config with Rayline's token counter added
    gateway-token               secret shared by the CLI and the local process
    ready.json                  local process address once it is up
    ready.arc-core.json         with local decisions: the core config pointed at
                                the local routes service
    routes-token                with local decisions: the token other local
                                programs present to the local Routes API
    supervisor.json             what is running
    runtime.lock                one local process at a time
    runtime.log                 its log
    harness/<name>/             files Rayline generates per harness in local mode:
                                pi, hermes, omp, opencode, openclaw
  harness/<name>/               files Rayline generates per harness in cloud mode: pi, hermes, omp,
                                opencode, openclaw, codex, codex-app,
                                claude-code (only with --isolated)
  harness/claude-code/daemon-launches.json
                                Claude Code daemons Rayline started
```

Each mode has its own harness folder because each writes its own endpoint, key and model list there.

Rayline writes nothing else under `~/.rayline` or `~/.config/rayline`. Outside them it writes only:
- the harness's own files named in 9.4;
- tokenizer files, and the ARC release and encoder weights of local decisions, in the Hugging Face cache: `$HF_HUB_CACHE`, else `$HF_HOME/hub`, else `<home>/.cache/huggingface/hub`;
- with `rayline run codex configure` (9.4), `rayline.config.toml` in the user's Codex home (`CODEX_HOME`, else `~/.codex`, Codex's own default);
- with `rayline run claude --isolated` (9.3), in the user's Claude config folder (`CLAUDE_CONFIG_DIR`, else `~/.claude`): the folders `projects/`, `sessions/`, `plans/` and `paste-cache/` and an empty `history.jsonl`, each only if missing, which the isolated folder links to.

Tests use a temporary home folder.

## 9. Kept for review

These keep today's behaviour for now, under the new command shape. Their aliases and extra values are still removed (section 10.4).

1. `rayline auth login`: `-b, --no-browser` (device-code login) and `-p, --paste` (paste the callback URL). `--paste` with `--no-browser` behaves as today (`--paste` wins). Launching a harness while signed out signs in on the spot with the default flow when stdin and stdout are both terminals. Otherwise the launch fails as signed out. A local-mode launch that reuses a running local process doesn't read the credentials, so it runs even when signed out.
2. `rayline update`: `--check`, `--version <version>`, `--force`, `--dry-run`.
3. `rayline run claude --isolated`: gives Claude Code its own config folder (`~/.rayline/harness/claude-code/`) so it can run beside other sessions.
4. `rayline run codex`: the subcommands `rayline run codex app [WORKSPACE]` (launches the Codex desktop app on its own `CODEX_HOME`, `~/.rayline/harness/codex-app/`) and `rayline run codex configure` (writes a Codex profile, `rayline.config.toml`, to the user's Codex home: `CODEX_HOME`, else `~/.codex`, Codex's own default). On Windows, the `config.toml` of each Rayline Codex home (`rayline run codex`'s and the app's) gets `[windows] sandbox` when it has none: the user's own value from `config.toml` in their Codex home, else `unelevated`, which needs no admin setup. A value already in the Rayline home is kept. A user `config.toml` that can't be read or parsed is an error naming it.

## 10. Removed

Everything below is deleted outright: code, tests, docs and help text.

### 10.1 Commands and options

1. `rayline router` and all its subcommands (`start`, `status`, `logs`, `top`, `stop`).
2. `rayline hybrid` and all its subcommands and options (`start`, `status`, `stop`, `run`, `--core`, `--config`, `--env`, `--local-context-window`, `--classifier-model`, `--cell`, `--port`, `--startup-timeout-secs`, `--state-dir`). `rayline local` and a local-mode `rayline run` replace it.
3. Every subcommand of today's `rayline local`: `models`, `download`, `use`, `remove`, `custom`, `show`, `test`, `clear`, `on`, `off`, `onboard`, `subagents`, with `--url` and `--base-url`.
4. Top-level harness commands (`rayline claude`, `rayline codex`, `rayline claude-desktop`, `rayline pi`, ...) and the `rayline claude run` form. `rayline run <HARNESS>` replaces them.
5. `rayline claude` options: `-m, --model`, `--auto-compact-window`, `--local-provider`, `--route`, `--via`, `--local-injector-port`, `--statusline`, `--no-statusline`, `--diagnose`, `--upstream-ca-path`, `--router-config-path`, `--routing-mode`, `--no-proxy`, `--local-router`, `--no-telemetry`.
6. `--auth-token`, at the top level and on every command.
7. Top-level `rayline status`. `rayline auth status` is the only status command.
8. `top` options `--legacy`, `--hybrid`, `--all`, `--state-dir`, and the `a` key on the live screen.
9. `update --channel`. `update.channel` and `RAYLINE_UPDATE_CHANNEL` replace it.
10. The `rl` alias.
11. `rayline run claude-desktop` and all its commands and options (`start`, `status`, `validate`, `stop`, `install-service`, `uninstall-service`, `off`, `uninstall`, `--manual`, `--prefixed-ids`, `--desktop-config`), with its LaunchAgent and its state folder and lock. `rayline run claude-desktop` is an unknown harness.
12. `rayline run --local`. `--mode local` and the config's `mode` replace it.

### 10.2 The old local stack

1. The old local router, transparent proxy, injector, adapter and the `rld` binary with its `serve`, `proxy`, `statusline` and `models` commands. The local process gateway moves into `rayline` (section 4.3).
2. Proxy mode and its proxy CA (`HTTPS_PROXY` injection, `NODE_EXTRA_CA_CERTS`).
3. The on-device model manager: downloads, hardware fit, llama.cpp management, Ollama/LM Studio discovery, account `enable_local` switching, subagent discovery and routes.
4. claude-desktop's own router copy (its state folder, ports, pid file, LaunchAgent and `router.json`).
5. Workspace crates used only by the above.

### 10.3 Settings and files

1. Settings keys: `router_url`, `cli_auth_url`, `account_url`, `default_env`, `auth_kind`, `authKind`, `firebase_api_key`, `google_device_client_id`, `google_device_client_secret`, `local_model`, `hybrid` (all of it, including `local_registry_model`, `routing`, `local_tokenizer`, `routing.local_price`, `cloud_output_tokens`, `classifier_model`, `local_context_window`), `subagent_routes`, `onboarding`.
2. Files: `~/.config/rayline/cli.toml`, `router.json`, `router.<env>.json`, `claude-daemon-launches.json` and harness homes under `~/.config/rayline`; `~/.rayline/rld/`, `~/.rayline/hybrid/`, `~/.rayline/cc/`, `~/.rayline/codex-app-home/`, `~/.rayline/claude-desktop/`, `~/.rayline/claude-desktop.lock` and `~/.rayline/harness/claude-desktop/`. Rayline neither reads nor moves them.
3. The old settings file location and its migration.
4. Today's `credentials.json` shape (`version`, camelCase fields, `authKind`, `router_keys.<env>`, `apiKey`).
5. The earlier shape's keys in a file with `mode`: top-level `endpoints`, `models` and `routing`, `routing.router` `hosted` (`cloud` replaces it) and a string `decisions_model`. Only the conversion of section 5.9 reads it.

### 10.4 Aliases, extra values and guesses

1. Firebase login and everything behind `auth_kind`.
2. A missing `refresh_token` replaced by the access token.
3. Sign-in flow guessed from `SSH_CONNECTION`, `DISPLAY`, `WAYLAND_DISPLAY` or `BROWSER`. `--no-browser` is the explicit way.
4. API mode aliases (`claude`, `openai`, `responses`, `openai_responses`): one name per mode.
5. Codex `--auth`, with all its values (`auto`, `subscription`, `none`, `no-auth`, `local`, `codex-subscription`, `chatgpt`, `codex`), and `--subscription`.
6. `n_ctx` as a context-window alias.
7. "Legacy mode" for the `current`/`execution_context_v1` API modes, support for a core without the token-counter contract, and legacy adapter overrides.
8. The Decisions environment guessed from its URL.
9. A missing context window replaced by a default.
10. A failure to get the router key hidden as "no key".
11. A missing classifier key or model silently skipping the classifier route.
12. The model taken from `--model`, then the request, then `ANTHROPIC_MODEL`.
13. Lowercase proxy env var fallbacks.
14. Hard-coded model constants: `DECISIONS_MODEL`, `LOCAL_REGISTRY_MODEL`, `DEFAULT_LOCAL_CONTEXT_WINDOW`, `CLASSIFIER_MODEL`. `CLOUD_MODELS` and `CLOUD_CONTEXT_WINDOW` stay for now: in local mode, the models ARC can pick that are not in `models` are exactly the three Rayline Platform models in section 5.3 item 2, each with a 1,000,000-token window and a 128,000-token output ceiling. They go when the Rayline router publishes ARC's model set.
15. The second copy of ARC's preset list in the core.
16. Install leftovers: cleanup of the old uv install and handling for releases from before the core.

### 10.5 Env vars

1. Overrides of `credentials.json`: `RAYLINE_AUTH_TOKEN`, `RAYLINE_ID_TOKEN`, `RAYLINE_ROUTER_API_KEY` from the user's shell, `RAYLINE_KEY` and its scrubbing.
2. `CLAUDE_SETTINGS_PATH`, `RAYLINE_HARNESS_CONV_ID_FALLBACK`, `RAYLINE_CONV_ID`, `RAYLINE_PI_ALLOW_EMPTY_SIGNATURE`, `ANTHROPIC_MODEL` as a model source, `RAYLINE_CALLER_CWD`.
3. `RAYLINE_CLI_AUTH_URL`, `RAYLINE_MODELS_REGISTRY_URL`. `environments` covers them.
4. `RAYLINE_UPDATE_BASE_URL`, `RAYLINE_UPDATE_INSTALL_PATH`. Hidden `update` options replace them (section 4.2).
5. The old local router, `rld` and proxy mode: `RAYLINE_MODEL_REPO`, `RAYLINE_MODEL_FILE`, `RAYLINE_MODEL_REVISION`, `RAYLINE_MODEL_SHA256`, `RAYLINE_LLAMA_TAG`, `RAYLINE_CTX_SIZE`, `RAYLINE_LOCAL_MODEL_ID`, `RAYLINE_NO_LOCAL_MODEL`, `RAYLINE_ADAPTER_PORT`, `RAYLINE_ADAPTER_UPSTREAM_MODEL`, `RAYLINE_ADAPTER_UPSTREAM_URL`, `RAYLINE_INJECTOR_PORT`, `RAYLINE_PROXY_PORT`, `RAYLINE_ISOLATED_PROXY_PORT`, `RAYLINE_ROUTER_URL`, `RAYLINE_DECISION_PLANE`, `RAYLINE_LOCAL_ROUTER_PORT`, `RAYLINE_ROUTER_CONFIG`, `RAYLINE_DATA_DIR`, `RAYLINE_PROXY_CA_CERT_PATH`, `RAYLINE_PROXY_CA_KEY_PATH`, `RAYLINE_UPSTREAM_CA_FILE`, `RAYLINE_ROUTE_STATUS_PATH`, `RAYLINE_ANTHROPIC_URL`, `RAYLINE_PROXY_ROUTING_MODE`, `RAYLINE_CLAUDE_ROUTING_MODE`, `RAYLINE_METRICS_PORT`, `RAYLINE_METRICS_URL`, `RLD_BIN`; proxy injection of `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`, `NODE_EXTRA_CA_CERTS`; the old router config folder from `APPDATA` and `XDG_CONFIG_HOME`; `OLLAMA_HOST` and `LMSTUDIO_HOST`.
6. Installer overrides: `RAYLINE_INSTALL_DIR` (`--install-dir` replaces it), `RAYLINE_RELEASE_BASE_URL` (the installer's hidden `--base-url` replaces it, section 4.4) and `RAYLINE_MINISIGN_PUBKEY` (the installers check no signature).
7. The settings overrides of earlier releases, `RAYLINE_ROUTING_DECISIONS_MODEL`, `RAYLINE_ROUTING_CLASSIFIER_MODEL`, `RAYLINE_ROUTING_POLICY_PRESET` and `RAYLINE_ROUTING_ROUTER`. The `RAYLINE_LOCAL_ROUTING_*` overrides replace them (section 5.7).

## 11. Done means tested

1. Unit tests cover every load error in section 5.8 and section 6, every usage error, the env override table and the migration in section 5.9. Every example file in this spec and the docs loads and passes `schema/settings.schema.json`, and a set of invalid files fails it.
2. Every harness in section 3 runs in cloud mode and in local mode and completes a real turn, as it does today.
3. The default test suite needs no live credentials or external services. Live runs are documented commands, run before merge.
