# Changelog

User-facing changes in each Rayline release. Install commands and signed
assets are listed on each [GitHub release](https://github.com/rayline-ai/raydock/releases).

## Unreleased

### Changed

- `rayline run hermes` in cloud mode refuses a Hermes older than 0.21.6, the
  first release with `hermes pm`, which Rayline uses to set Hermes up. The
  error names the install commands. Before, the launch failed at
  `hermes pm install` with Hermes' own error. The Hermes on PyPI (0.19.0) is
  older than this.
- **`rayline auth logout` also removes the router key from the harness
  configs Rayline wrote** under `~/.rayline/harness/` and from the Codex
  `rayline` profile; each file keeps its other settings. Logout now revokes
  the session first: if the router can't be reached, you stay logged in and
  can run it again. If the router says the session is already invalid,
  logout still signs you out locally and reports it.
- **`rayline run codex` and `rayline run codex app` on Windows can run shell
  commands.** The Codex config Rayline writes now has a `[windows] sandbox`
  setting: yours from `~/.codex/config.toml` when you set one, else
  `unelevated`, which needs no admin setup. Before, Codex had no sandbox
  setting there and rejected every command as "blocked by policy". A value
  already in the Rayline Codex home is kept.
- Sign-in hints name the exact command, including `--env <name>` for a
  configured environment: "Not signed in to Rayline" (from `rayline auth status` and
  every launch) and the login timeout and expiry messages, which also say to
  run it again.
- `rayline run openclaw` refuses an OpenClaw older than 2026.9.6 with a
  message naming the version to install, instead of handing it a config it
  rejects as invalid.
- **`rayline run claude` gives Claude Code ARC's real context window.**
  Claude Code doesn't know ARC model ids and assumed 200K for them, which
  also capped the auto-compact window, so long sessions compacted at a
  fraction of the window. Rayline now sets `CLAUDE_CODE_MAX_CONTEXT_TOKENS`
  to the main model's window from the router's model list; a `cloud.model.id`
  the list doesn't have keeps Claude Code's own default. A Claude Code
  daemon an older Rayline started doesn't hold this setting, so the first
  launch after updating shows the running-daemon prompt.
- `rayline update` updates every install that Homebrew doesn't manage, and
  `rayline update --help` names Homebrew only.

### Fixed

- `rayline local start` no longer fails when a `local.inference.models` entry
  has a `tokenizer` and its endpoint lists other protocols besides
  `openai_chat`, as in the documented example. The tokenizer counts the
  model's OpenAI Chat requests.
- `rayline local start` refuses a `tokenizer` whose endpoint doesn't list
  `openai_chat`, and a tokenizer repo with no chat template (a base model's),
  with an error naming the model's `tokenizer` key.

## 0.5.6 (2026-11-17)

### Changed

- **`settings.json` has a new shape (breaking).** It holds a required `mode`
  (`cloud` or `local`), a `cloud` section with the two models a cloud-mode run
  uses, and a `local` section with `inference` (`endpoints`, `models`) and
  `routing`. The cloud models (ARC Sage and ARC Swift by default) come from
  `cloud.model` and `cloud.smol_model` instead of from Rayline. When the file
  doesn't exist, Rayline writes the default file and says so. Rayline checks
  the whole file, including the section a run doesn't use: a section the file
  has needs its required keys, and an env override doesn't give a missing
  one. See [Getting started](docs/getting-started.md#settings) and
  [Local mode](docs/local-mode.md)
  ([#348](https://github.com/rayline-ai/rayline-cli/pull/348)).
- A `~/.config/rayline/settings.json` from an earlier release (one with no
  `mode`) is converted the first time a command reads it, and the original
  is kept as `settings.json.bak`. A `--config` file with no `mode` is an
  error and isn't converted.
- **`rayline run <harness> --local` is replaced by `--mode local` (breaking),**
  or `"mode": "local"` in the file. `--mode` and `--config` combine, and a
  `--config` file runs in its own `mode`. `rayline run --help` and the
  messages that say how to start a local process name `--mode local`.
- **`router` `hosted` is now `cloud` (breaking)** in `local.routing.router`,
  `rayline local start --router`, `rayline top` and `rayline local status
  --json`. `local.routing.router` is required. `rayline local start --router
  cloud` with no `local.routing` uses the defaults `--router hosted` used, and
  the `rayline local start` errors end with `rayline local start --router
  cloud`. Stop the local process (`rayline local stop`) before updating from
  an earlier release: this version can't read the record of an earlier
  release's process started with router `hosted`.
- Local mode with no `local` section is an error that says `local` is
  required and links the local mode docs. It no longer prints a
  `settings.json` to paste.
- Every `rayline run pi` and `rayline run omp` sets the models from the
  config: pi's `defaultProvider` and `defaultModel`, and omp's `modelRoles`
  `default`, `smol`, `slow` and `plan`, so a changed `cloud.model` reaches a
  pi or omp that has run before. Every other setting in pi's `settings.json`
  and omp's `config.yml` stays as the harness wrote it. A model picked in pi
  or omp is replaced at the next launch.
- `rayline top` says `cloud` instead of `remote`: `target` `cloud` and
  `totals.cloud_requests` in `--json`, and `cloud=` in the live view's
  totals.
- Rayline doesn't check `cloud.model.id` or `cloud.smol_model.id`; the API
  validates them when the model is called. An id the key's model listing
  doesn't have goes into the opencode, pi, omp, OpenClaw and Hermes configs
  with no limits, and the harness uses its own defaults.
- `local.routing.decisions_model` is an object, `{ "id": "arc-swift-1.0" }`.
  With `router` `cloud` it can carry a `v_value`.
- The env overrides follow the new key paths: `RAYLINE_MODE`,
  `RAYLINE_CLOUD_MODEL_ID`, `RAYLINE_CLOUD_SMOL_MODEL_ID` and
  `RAYLINE_LOCAL_ROUTING_*`. `RAYLINE_ROUTING_*` is no longer read.

### Rollback

To go back to 0.5.5: run `rayline local stop`, then `rayline update --version 0.5.5`. 0.5.5 refuses the new `settings.json` keys, so put `~/.config/rayline/settings.json.bak` back as `settings.json` if you have one, or rename `settings.json`. Plain `rayline update` would install 0.5.6 again.

## 0.5.5 (2026-11-17)

### Added

- `rayline local start --router hosted` with no `routing` section uses the
  same defaults as `--router local` (`arc-swift-1.0`,
  `arc-policy.prefer-local.v1`), so local mode with hosted decisions needs no
  `settings.json`
  ([#347](https://github.com/rayline-ai/rayline-cli/pull/347)).
- `rayline update --version 0.5.5` takes a version without its build label
  and reads the full version from that release's signed version file. A
  version that doesn't exist fails with `release <version> does not exist`,
  and one whose release names another version fails with
  `release <version> names version <full>`.
- `rayline run --help` lists `--local`, `--config`, `--isolated`, the Codex
  subcommands and where harness arguments go. `rayline update --help` gets a
  summary, a `--help` row and a note on Homebrew and Rayline.app installs.

### Changed

- **Faster first `local` decision in a new conversation.** A new conversation
  continues ARC's encode from what it shares with an earlier one: the tool
  definitions, the system prompt and everything else before its first
  message. For Claude Code on Apple M-series, a new conversation's first
  decision takes about 1.3 s instead of about 2 s
  ([#342](https://github.com/rayline-ai/rayline-cli/pull/342),
  [#346](https://github.com/rayline-ai/rayline-cli/pull/346)).
- `rayline run <harness>` with only `--help`, `-h`, `--version` or `-v` runs
  the harness with that flag and nothing else: no sign-in, no config, no
  local process and no Claude Code daemon check. This now applies to every
  harness, also with `--local`.
- Signed out, Rayline says `Not signed in to Rayline. Run: rayline auth login`,
  naming the environment only when `--env` names one.
- `rayline top --json`, or `rayline top` when its output isn't a terminal,
  with no local process says how to start one. State left behind by a local
  process that stopped or crashed counts as no local process, there and in
  the live view ([#349](https://github.com/rayline-ai/rayline-cli/pull/349)).
- Local mode with no `settings.json` prints a `settings.json` to paste and
  links the local mode docs.
- Every `rayline local start` error for a machine that can't run `local`
  decisions ends with the same fix: `rayline local start --router hosted`.
- `rayline local start --help` and the Codex help say where decisions are
  made in Rayline Platform terms.

### Fixed

- In local mode with hosted decisions, a Codex or Hermes turn on a Claude
  model that thinks before it calls a tool now completes
  ([#345](https://github.com/rayline-ai/rayline-cli/pull/345)).

### Docs

- The README and getting-started guide use the current install commands:
  `curl -fsSL https://get.rayline.ai/install.sh | bash` and
  `irm https://get.rayline.ai/install.ps1 | iex`
  ([#343](https://github.com/rayline-ai/rayline-cli/pull/343)).
- The local mode guide describes `rayline top`'s live view and `--json`, and
  how `RAYLINE_ROUTING_*` overrides apply to the `--router` defaults.
- The 0.5.4 notes gain a Removed entry for Rayline's `--model` option of
  `rayline run codex` (#325).

### Known limits

- The first `local` decision of each new conversation still encodes what
  follows the shared part again. For Claude Code on Apple M-series that is
  about 1.3 s, and about 2 s for the first conversation since the local
  process started. Later turns take about 0.1 s.
- On Windows, every `local` start hashes the ARC files again; macOS and Linux
  skip files unchanged since they were last checked.
- A release's signed checksums aren't yet bound to its version
  ([#333](https://github.com/rayline-ai/rayline-cli/issues/333)).

### Rollback

To go back to 0.5.4: `curl -fsSL https://get.rayline.ai/install.sh | bash -s -- --version 0.5.4`. The first `local` start then downloads 0.5.4's ARC encoder. Plain `rayline update` would install 0.5.5 again.

## 0.5.4 (2026-11-17)

### Added

- **Local routing decisions on Windows with an NVIDIA GPU (preview).**
  Releases ship the ARC encoder for Windows x86_64 with the cuBLAS DLLs it
  loads, and the first `local` start downloads them. It needs a GPU of compute capability 7.5, 8.x, 9.0 or 12.0 and
  NVIDIA driver 576 or later, and is about 1 GB. See
  [Local mode](docs/local-mode.md).
- `rayline local start` with `local` decisions fails when the ARC encoder finds
  no GPU it can use, instead of running ARC on the CPU.
- `rayline run <harness> --local` works with no `routing` in `settings.json`
  while a local process started by `rayline local start --router local` from
  its defaults is running: the run uses the same defaults. Claude Code still
  needs `routing.classifier_model`; `rayline local start` and
  `rayline run claude --local` say so with a ready-to-paste snippet.
- A cloud `rayline run` notes when a local process is running: "Note: a local
  process is running; this run uses Rayline Platform. Add --local to use the
  local process."
- `rayline top`'s live view waits for a local process that isn't running yet,
  and follows one that restarts, instead of exiting.

### Changed

- **Installing and updating Rayline no longer download the ARC encoder.** The
  installers fetch only `rayline`, `rayline-core` and `rayline-core-LICENSE`.
  The first `rayline local start` (or `rayline run ... --local` that starts
  the local process) with `routing.router: "local"` downloads the encoder from
  the same release into `~/.rayline/arc/<version>/`, checked against the
  release's signed checksums and shown in the ARC release's download line.
  `rayline update` fetches the new version's encoder only when
  `~/.rayline/arc/` already holds one; older versions are removed by the next update or `rayline local start` while no local process runs.
- `rayline run claude --version` (also `-v`, `--help`, `-h`) no longer asks
  about a running Claude Code daemon; it just prints.
- At the running-daemon prompt of `rayline run claude`, Enter now runs
  isolated (`[i]`) instead of stopping the other daemon; `s` still stops it.
- `local` decisions on Linux and Windows need the NVIDIA driver: without it,
  start fails before downloading anything and says to use `--router hosted`.
- Rayline no longer uses an encoder installed beside `rayline` by 0.5.3 and
  earlier. You can delete `rayline-arc-encoder`, `rayline-arc-encoder-LICENSE`
  and, on Windows, the `cublas*.dll` files from the folder `rayline` is in.
- When the installed `rayline` is newer than the channel's latest release (such
  as a dev build on `prod`), `rayline update` and `rayline update --check` say
  so and name the command that installs that release, instead of "Already on
  latest".
- `rayline run claude --model X` no longer puts subagents and background tasks
  on X: they always run on ARC Swift, unless Claude Code's own settings pin
  a subagent model
  ([#325](https://github.com/rayline-ai/rayline-cli/pull/325)).

### Removed

- Rayline's own `--model` option for `rayline run codex`, `rayline run codex
  app` and `rayline run codex configure`. A `--model` after `rayline run
  codex` now goes to Codex unchanged, also in local mode, which refused it
  before, and `rayline run codex configure --model` is a usage error
  ([#325](https://github.com/rayline-ai/rayline-cli/pull/325)).

### Fixed

- Linux release builds no longer fail when a package mirror is mid-sync.

### Known limits

- On Windows, every `local` start hashes the ARC files again (the release,
  the encoder weights, the encoder and its DLLs); macOS and Linux skip files
  unchanged since they were last checked.
- The first `local` decision of each new conversation encodes its whole
  prompt: about 1 s per 10k prompt tokens on Apple M-series, so about 3 s for
  Claude Code's first turn. Later turns take about 0.1 s.
- A release's signed checksums aren't yet bound to its version
  ([#333](https://github.com/rayline-ai/rayline-cli/issues/333)).

### Rollback

To go back to 0.5.3: `curl -fsSL https://get.rayline.ai/install.sh | bash -s -- --version 0.5.3`, then, for `local` decisions, `rayline update --version 0.5.3+3bc25bc98f31` to install its ARC encoder. Plain `rayline update` would install 0.5.4 again.

## 0.5.3 (2026-11-16)

### Added

- **Local routing decisions on Linux with an NVIDIA GPU (preview).** Releases
  ship the ARC encoder for x86_64 and aarch64 Linux, and the installers,
  `rayline update` and Homebrew install it beside `rayline`. It needs a GPU of
  compute capability 7.5, 8.x, 9.0 or 12.0 on x86_64 (T4 through H100, RTX
  50), or 9.0 or 12.1 on aarch64 (GH200, GB10), and the NVIDIA driver with no
  CUDA toolkit: 525.60.13 or later (570 or later for RTX 50, 580 or later for
  GB10). It is 0.6 to 0.8 GB.
  Other GPUs (Vulkan) and CPU-only machines aren't supported. See
  [Local mode](docs/local-mode.md).

- **`rayline top` says where routing decisions are made**: `Router: local ARC
  (<model>) on this machine` or `Router: hosted ARC (<model>) in Rayline
  Platform`, with each request's decision time and event.

### Changed

- **The Linux `rayline` binary runs on glibc 2.28 or later** (RHEL 8, Debian
  10, Ubuntu 18.10 and newer). It needed glibc 2.39 before.
- **Starting local decisions is faster with a cached release.** A cached ARC
  release file that hasn't changed since it was verified isn't hashed again
  (macOS and Linux; Windows still hashes every start). A cached start on an
  M-series Mac took about 3.1 s and now takes about 1.2 s.
- **Local decisions check for the ARC encoder before downloading anything**,
  and the error says whether releases ship an encoder for your platform (use
  `hosted`) or it just isn't installed (run `rayline update`).
- **A failed `rayline local start` shows the error the local process printed**,
  not only a pointer to its log.
- **Local mode refuses a Pi older than 0.87.1**, whose turns all failed, with
  a message saying to update Pi.
- **A missing `settings.json` in local mode says to create it**, and which
  `routing` keys Claude Code needs.

### Fixed

- **`rayline local stop` could return before the local process had removed
  its state**, leaving a `gateway-token` that made every later `start`,
  `stop` and `status` fail until it was deleted by hand. `stop` now returns
  only once the process's state is gone. If you hit this on 0.5.2, delete
  `~/.rayline/local/gateway-token`, then run `rayline local start`.
- **The ARC encoder is stopped even when the execution core fails to stop.**
- **The download progress line ends at 100%**, and an output rate is shown
  only after half a second of output.
- **On Windows, `rayline local status` shows the config path without the
  `\\?\` prefix.**

## 0.5.2 (2026-11-15)

### Added

- **Routing decisions can run locally (preview).** With `routing.router:
  "local"` (or `rayline local start --router local`), the local process
  decides each turn's route on your Mac with the public ARC 1.0 release
  (`rayline-ai/ARC-1.0`) instead of calling the Rayline Routes API. It uses
  `arc-swift-1.0` or `arc-sage-1.0`. On first use it downloads the release and
  about 1 GB of encoder weights, with no Hugging Face login needed, and checks
  them against their pins. Turns still run on the models you route to. See
  [Local mode](docs/local-mode.md).
- **The local ARC encoder ships with Rayline on macOS arm64.** The installers,
  `rayline update` and Homebrew install `rayline-arc-encoder` beside `rayline`,
  verified against the release's signed `SHA256SUMS`. Homebrew keeps its
  license in `pkgshare`.
- **`rayline local start` shows its progress:** the ARC release download, with
  its size and rate, then verifying it and starting the local process. Without
  a terminal it prints a line at each 10%.
- **`rayline local status --json`** reports whether the local process runs, its
  URL and its decisions. With local decisions it also names a token file
  (mode 0600, removed on stop) that admits only route decisions, so another
  program on the same machine can ask the local process for decisions.
- **`rayline local start --router local` works without a settings file**,
  with `arc-swift-1.0` and the prefer-local policy. `routing.classifier_model`
  is needed only to run Claude Code, and a Claude Code run without it fails
  before starting, naming the key.

### Changed

- **`rayline update` keeps the encoder in line with the release.** It installs
  the encoder and its license when they are missing or differ from the signed
  `SHA256SUMS`, also when Rayline itself is already the latest, and skips
  files that match. Downloads stream to disk with connect and idle timeouts
  instead of a 30-second limit, so a large file on a slow connection finishes.
- **The local-mode start-up check no longer posts an empty route request.**
  It asks the hosted Decisions router for its models, and a 400 no longer
  counts as ready.

### Fixed

- **`arc-1.0` with local decisions is refused when the config loads**, naming
  `routing.decisions_model`, instead of refusing every turn. Errors from local
  ARC now say the decision was made on this machine.
- **An unreachable Rayline Platform fails a turn once**, with a message that names
  Rayline Platform and says to check the network and `HTTPS_PROXY`, instead of
  the harness retrying for minutes.
- **Refusals are logged.** The execution core's refusals, and the gateway's
  own 413, are now logged in `runtime.log` (never the request body).
- **`rayline top` counts cloud turns as remote**, and times output that
  arrives in one burst at the end of a response (such as a tool call) from the
  response's start, instead of showing rates of tens of thousands of tokens per
  second.
- **Claude Code's default thinking display (`updates`) is accepted.** Before,
  each Claude Code start got two refusals and resent without it. The bundled
  execution core (`rayline-core`) is now pinned to `af9b0050`.

## 0.5.1 (2026-11-13)

### Changed

- **The installers no longer need minisign.** `install.sh` and `install.ps1`
  check each file they download against the release's `SHA256SUMS`, and
  refuse to install on a mismatch. `rayline update` still checks the release
  signature itself, with no extra tool.

## 0.4.5 (2026-11-12)

### Fixed

- **A declared local model (GLM-5.3-Flash) keeps its turns until its window
  is really full, and after an image.** Hybrid mode counted such a model's
  prompts by their bytes, several times the real number, and could not size
  a history holding an image at all, so the prefer-local policy sent turns to
  the cloud early and kept them there. `hybrid start` now fetches the model's
  published tokenizer and chat template and `rld` counts each prompt with
  them, images included; a model with an image processor also accepts images.
  New setting `hybrid.local_tokenizer` names the repo for another model, or
  `false` turns counting off. The bundled execution core (`rayline-core`, now
  pinned to `1c89739c`) asks `rld` for these counts.
- **A hybrid target that can't take a request is skipped, not chosen and
  refused.** When the core can't prepare a request for a target (for example,
  the output allowance the client asks for is above that target's limit), it
  now reports the target as unavailable for that turn, so the router picks
  another one instead of selecting it and returning an error.
- **Codex gets each model's real context window from the local router.**
  The router's `/v1/models` catalog told Codex that every model had a
  128,000-token window, so Codex compacted at about 115,000 tokens even on
  the 1M-token ARC models, and a session whose requests stayed above that
  compacted before every request. Each catalog entry now carries the window
  its endpoint reports: `context_length` from the Rayline API's model list,
  a ChatGPT subscription model's own window, and for local runtimes the
  context of a loaded Ollama model, the tag's `num_ctx` (read without loading
  the model) or llama.cpp's `n_ctx`. The default `rayline-local` entry takes
  the window of the model it routes to. A model whose endpoint reports no
  window keeps the 128,000 default.
- **`raydock claude --config` stops when it can't read the config file.**
  A missing or malformed `--config` file used to be treated as a config
  that leaves the main agent on your Claude subscription, so main traffic
  went straight to Anthropic instead of the routing you named. The launch
  now fails at once with `cannot read --config <path>: <reason>`, as
  `raydock codex` does.
- **`raydock hermes` starts on a clean Windows install.** Hermes on Windows
  keeps its Python dependencies per Hermes home and refused to start in
  Raydock's own home ("no dependency environment is committed for this
  install"), while a plain `hermes pm repair` fixed the default home instead.
  Raydock now runs `hermes pm repair` once for its Hermes home before the first
  launch (also under `raydock hybrid run hermes`) and skips it afterwards. If
  the repair fails, it prints the command to run by hand with the right
  `HERMES_HOME`.
- **`raydock codex` finds Codex installed with npm on Windows.** npm installs
  Codex as a `codex.cmd` launcher, and Windows does not apply PATHEXT when a
  program starts another by bare name, so `raydock codex` failed with "failed
  to run codex: program not found". It now looks Codex up on PATH the way
  `raydock hybrid run codex` already did. This also fixes `raydock codex app`
  and the Codex version check.

### Docs

- The hybrid guide points to the official Rayline docs for billing and ARC
  routing-policy details instead of restating them.
- The hybrid platform requirements list Windows 10 and 11, supported natively
  since 0.4.2.
- The routing-modes examples and guides describe only the current modes. The
  `Rcl-Rcl` and `S-Rcl` configs are removed (they route exactly like `Rc-Rc`
  and `S-Rc`), and splitting work between a local model and the cloud turn by
  turn is described as hybrid mode.

## 0.4.4 (2026-11-11)

### New

- **Claude Haiku 5.5 is a third default cloud target in hybrid mode.** The
  default profile pins Haiku 5.5 beside Opus 5.5 and Sonnet 5.5, so a turn
  that leaves the local model (too large for its window, or one the router
  expects it to fail) has a cloud target priced well below Sonnet. Like the
  other two, it allows 128,000 output tokens. A `hybrid.cloud_output_tokens`
  block must now name all three pins; one written for the two earlier pins
  is refused until it has a Haiku line.

### Changed

- **Hybrid mode now prefers your local model by default.** With no
  `hybrid.routing.policy` set and a local model available, Raydock asks for
  `prefer_local`: the local model takes a turn whenever the router predicts
  it will succeed (`min_p_pass`, default 0.65), and other turns are decided
  on value and price as before, which may pick Haiku. Set
  `"policy": "utility"` for the previous behaviour, which can pick a cheap
  cloud model over the local one. Sessions without a local model still use
  `utility`.

### Fixed

- **Claude Code in hybrid mode works with llama.cpp chat templates that
  require the system message first** (such as Qwen3.8-27B). The bundled
  execution core (`rayline-core`, now pinned to `80ce3d26`) moves a system
  message that appears later in the conversation into the user or tool turn
  it follows, as user content, before sending the turn to a local
  OpenAI-chat model, so the template no longer rejects it with "System
  message must be at the beginning." The leading system message is left as
  is, and requests with no later system message are sent unchanged.
  ([#196](https://github.com/rayline-ai/raydock/issues/196))
- **A local model failure no longer ends the turn.** If the local server
  returns a 5xx error or can't be reached before any output, the core asks
  the router once more for that turn with the failed local model excluded,
  which normally sends it to a cloud target. Errors from that second attempt
  are still returned to the agent.
- **Session headers reach the Rayline router through the local router.** A
  client that sends its session id only in a header (`x-session-affinity`,
  `x-claude-code-session-id`, `x-session-id` or `x-conversation-id`) and goes
  through `rld`'s local router to the hosted router, such as Claude Code in
  Claude Desktop (`raydock claude-desktop`), lost that header on the way, so
  the router keyed its turns on an id it derived from the request instead.
  The local router now forwards exactly these four headers, unchanged, on
  Messages, Chat Completions and Responses, and only when the upstream is the
  hosted Rayline router; Anthropic, OpenAI, OpenRouter and custom endpoints
  never receive them. Hybrid's auto-mode classifier calls carry all four as
  well.
- **Session ids sent in the request body survive translation in the local
  router.** When the local router turns a request into another protocol (Chat
  Completions or Responses into Messages, Responses or Messages into Chat
  Completions), the rebuilt body dropped the client's `session_id` and
  `prompt_cache_key`, so separate sessions merged into one conversation. Seen
  with Codex through a gateway with a Messages upstream and with Cline's
  OpenRouter provider. The local router now sends that id to the hosted Rayline
  router as `x-session-affinity`, in the router's own order for those fields:
  Codex's `prompt_cache_key` first, then `session_id`, then a Responses
  `prompt_cache_key`. A session header the client sent still wins, and an id a
  header cannot carry unchanged (non-ASCII) is skipped. Requests forwarded
  without translation are unchanged, and no other upstream receives the
  header.
- **`raydock claude-desktop` says where `--env` goes.** `--env` placed after
  `claude-desktop` or one of its subcommands (`raydock claude-desktop status
  --env dev`) was only reported as rejected. As with `raydock top`, the error
  now says `--env` goes before the command:
  `raydock --env <name> claude-desktop status`.

## 0.4.3 (2026-11-09)

### New

- **Hybrid mode runs on any local model, not just Qwen3.8-27B.** Set
  `hybrid.local_registry_model` in `~/.config/rayline/settings.json` to the
  Rayline model ID of the model you serve, such as `"z-ai/glm-5.3-flash"`.
  ARC then predicts for the model that actually runs. The value must have the
  form `provider/model`; the execution core refuses a model its registry
  doesn't know, and the error names the setting. It can't be combined with
  `--config`, since a profile file sets its own model.
- **`prefer_local` works with any local model.** The bundled execution core
  (`rayline-core`, now pinned to `56e6d443`) counts Qwen3.8-27B prompts exactly
  and bounds other models' text prompts by their byte size (`caller_bounded`),
  so ARC can keep a turn local only when it fits the window. It no longer needs
  a llama.cpp prompt counter. This needs the production router from
  2026-11-09 or later.
- **Auto-mode safety checks go to the cloud.** Claude Code's auto-mode
  classifier calls are sent to Claude Haiku 5.5 through the router instead of
  the local model. Change the model with `hybrid.classifier_model` or
  `--classifier-model`. `raydock hybrid run claude` sets
  `CLAUDE_CODE_AUTO_MODE_SERVER=0` (unless you set it), so Claude Code 2.1.321
  and later keep using these classifier calls rather than the API's in-turn
  checks, which a turn served by the local model can't answer. Without it,
  every checked action in auto mode was denied.

### Changed

- **Hermes lists Rayline as `custom:rayline`.** The Messages provider that
  `raydock hermes` writes into Hermes' `config.yaml` is now named `rayline`
  instead of `rayline-anth`. The next launch removes the old `rayline-anth`
  entry. The Responses provider is still `rayline-resp`.
- **Hybrid's Claude pins use the router's own model ids.** The default
  profile pins `claude-opus-5-5` and `claude-sonnet-5-5`, and the auto-mode
  classifier defaults to `claude-haiku-5-5`, which the router serves directly
  from Anthropic. Before, they pinned the `anthropic/claude-*-5.5` ids, which
  the router sends through OpenRouter. Key `hybrid.cloud_output_tokens` by the
  new ids: settings keyed by the old ones are refused, naming the key. The
  bundled execution core accepts a router id as a pin's wire model.

### Fixed

- **`hybrid start` warns when the local window cannot hold Claude Code's
  opening request.** For a model other than Qwen3.8-27B, the core reserves
  the opening request (about 73,000 tokens with no MCP servers or project
  instructions, more with them) plus up to 32,000 for the answer. Below about
  97,000 tokens no task start fits, so every task started on cloud with no
  sign why. `start` now says so and names the 262,144-token setting.
- **rld logs why a turn failed before any output.** The resend and every
  final 502 from the core are logged with the routing service's HTTP status
  and code (from the core's own Decisions message; any other message, which
  may be a provider's text, is logged only as "other") and the route id, when
  the core passes one on. Before, rld logged only "Decisions unavailable" on a
  resend and nothing on the final failure.
- **Hybrid uses the local runtime's own context window.** With no
  `--local-context-window` or `hybrid.local_context_window`, `hybrid start`
  now uses the window the runtime reports (llama.cpp `/props`, or
  `/v1/models` from vLLM or TensorFold, up to 1,000,000) instead of 98,304.
  Before, Claude Code on a model other than Qwen3.8-27B barely fit: its
  opening request (about 73,000 tokens) plus the 24,576-token answer
  allowance nearly fills 98,304, so any tools or project instructions pushed
  it to cloud. Ollama and MLX, which
  report no window, keep the 98,304 default; a configured window still wins,
  lowered to a smaller loaded one.
- **Hybrid reads the context window from `/v1/models`.** When a server has no
  llama.cpp `/props` endpoint (vLLM, TensorFold), `hybrid start` reads
  `max_model_len` or `context_length` from the served model's entry. As with
  llama.cpp, this can only lower the configured window; a larger loaded window
  prints how to raise the setting.
- **A turn too long for a vLLM or TensorFold server moves to the cloud.** When
  the server states its window in `/v1/models`, the local deployment is marked
  as refusing over-window prompts, and the core asks ARC again after such a
  refusal and moves the turn to a model that fits. llama.cpp (which may
  truncate with `--context-shift`), Ollama and MLX keep the default.
- **Each cloud target declares its own model's output ceiling.** Both Claude
  5.5 cloud deployments now allow 128,000 output tokens instead of a shared
  32,000. Override per model with `hybrid.cloud_output_tokens`.
- **Every deployment lowers an over-cap output allowance.** A request asking
  for more output than a cloud deployment's ceiling is lowered to it instead of
  refused, as the local deployment already did; the stated and sent figures are
  logged. A turn whose thinking budget reaches the ceiling is still refused.
- **A failed routing decision is no longer retried many times.** When Rayline's
  routing service refuses a hybrid turn (for example over a price or model it
  doesn't know), Claude Code now gets a final HTTP 400 that says why, instead
  of a 502 it resent about ten times. When the service is unavailable or busy,
  `rld` resends the turn once, after the service's `retry-after` (only if it
  asks for 30 seconds or less, and never when it sends
  `x-should-retry: false`; about 5 seconds, randomly varied, when it names no
  wait), and then reports HTTP 503 or 429. Every such error carries
  `x-should-retry: false`, so the harness SDK doesn't resend it again, and a
  retryable one carries `retry-after`. A failed turn is now sent at most twice
  instead of about ten times.

### Note

Hybrid routing decisions send the conversation to Rayline's router: only local
inference stays on your machine. For fully local use, run
`raydock claude --local` with a fully local config. See
`docs/hybrid-local-setup.md`, "What leaves your machine".

## 0.4.2 (2026-11-07)

### New

- **Hybrid mode runs on Windows.** `raydock hybrid start`, `status`, `stop`
  and `run` no longer refuse Windows. A second `hybrid start` waits on a real
  start lock, the hybrid state directory and its token, ready, supervisor,
  config and log files are readable by the current user only, and `rld` runs
  detached, so closing the console that started it no longer stops it.
  `hybrid stop` shuts `rld` and the execution core down gracefully and removes
  their token and ready files; the timeout and hard-kill fallback are
  unchanged. `hybrid run codex` and `hybrid run pi` find npm's `.cmd`
  launchers. Windows CI now runs the hybrid lifecycle tests. macOS and Linux
  behaviour is unchanged.
- **`raydock router start` runs `rld` detached on Windows, as hybrid does.**
  The router and proxy that `router start`, `raydock claude --local` and the
  other harness launches start no longer hold the caller's standard handles
  or live in its console, so `raydock router start | cat` and
  `$(raydock router start)` return as soon as start finishes, and closing the
  console that ran `router start` no longer stops `rld`. macOS and Linux
  behaviour is unchanged.

### Fixed

- **`raydock hybrid run claude` works on cloud routes.** The execution core
  (`rayline-core`) is pinned to a newer source commit whose `model_pin`
  Messages transport forwards the `system` rows Claude Code 2.x sends inside
  `messages` and no longer refuses a request body that merely contains the
  string `"xhigh"`. Before, every Claude Code 2.x turn on a cloud route failed
  with `400 model_pin Messages cannot preserve in-array system or developer
  messages`. The root effort fields are still stripped before dispatch, and
  in-array `developer` rows are still refused (see Known issues).
- **Hybrid tells Claude Code the gateway's context window.** `raydock hybrid
  run claude` now sets `CLAUDE_CODE_MAX_CONTEXT_TOKENS` to the window the
  gateway advertises, so Claude Code no longer assumes 200k for
  `rayline-auto`, auto-compacts early or warns about an unrecognized model on
  every launch. Other harnesses already got their window through their config
  files.
- **The local deployment lowers an over-cap output allowance.** A harness that
  asks for more output than the local deployment's cap (a quarter of the local
  window, 24,576 tokens at the default 96k), such as Hermes with its fixed
  128,000, has its local turns lowered to the cap instead of refused with
  `output allowance exceeds the admitted deployment limit`. Cloud deployments
  still refuse an allowance over their ceiling.
- **Streamed Responses turns on an Anthropic route record their cache-read
  tokens.** When a Codex `/v1/responses` turn is served through an
  `anthropic_messages` endpoint and both sides stream, the local router now
  closes the metrics row with the cached part of the prompt as well as the
  prompt and output counts, so `raydock top`'s cache column covers the shape an
  interactive turn normally takes. The row also gains the prompt total the
  cache hit rate is measured against, so that column reads as a rate rather
  than a blank. Those counts were dropped for stream-to-stream turns only; the
  buffered shapes and the Chat Completions and Messages paths already recorded
  them. An upstream that reports no cache tokens still leaves the count unset
  rather than reporting a zero. Response bodies and the `usage` the client
  receives are unchanged.
- **Python 3.13+ clients trust the proxy's certificates.** Proxy leaf
  certificates now carry an Authority Key Identifier, filled from the CA's own
  Subject Key Identifier, so it matches CAs already on disk. Python 3.13+
  rejects a leaf without one (`CERTIFICATE_VERIFY_FAILED: Missing Authority
  Key Identifier`), so any Python client that trusted the proxy CA failed its
  TLS handshake.

### Changed

- **Responses cost details use the upstream spelling.** The execution core
  now names the provider charge's `cost_details` halves
  `upstream_inference_input_cost` and `upstream_inference_output_cost` on
  Responses (Chat Completions keeps the prompt/completions spelling). Raydock
  reads either spelling.

### Docs

- The routing-modes examples follow the `rayline-router` removal: configs pin
  current model ids, the retired `Rcl` modes are documented as behaving like
  `Rc`, and a Hybrid mode section links the setup guide.
- The SDK examples run as shipped: `local/python` builds its client with the
  SDK's own HTTP client, the local examples ask a question a raw API call can
  answer, and the examples allow 2048 output tokens so reasoning models print
  an answer.
- The hybrid setup guide states that Qwen3.8-27B needs a machine with at
  least 32 GB of memory, and that a runtime which reports no model name gets a
  warning and is assumed to be Qwen3.8-27B.

### Known issues

- **Codex through hybrid still fails on cloud `model_pin` Messages routes.**
  The execution core refuses the in-array `developer` messages Codex sends on
  that transport, so `raydock hybrid run codex` turns that land on such a
  route fail. Local turns and Claude Code are not affected.

## 0.4.1 (2026-11-05)

### Changed

- **The Legacy Rules Router is removed (breaking).** The hosted router no
  longer serves `rayline-router`, and Raydock no longer offers or sends it:
  the generated `router.json` lists only `arc-sage-1.0` and `arc-swift-1.0`,
  `raydock codex` no longer pins `rayline-router` to the Legacy model, the
  `--model rayline-router` help text and the "Legacy Rules Router" model label
  are gone, and a may-local `rayline-cloud` route without a `model` is refused
  at `router start` instead of being filled with `rayline-router`. A file or
  setting of yours that still names `rayline-router` for the hosted router is
  not rewritten: its turns fail with the router's 404 until you change them to
  `arc-sage-1.0` or `arc-swift-1.0`. That includes the default `router.json`
  an 0.2.x release generated, which Raydock no longer upgrades. The omp and pi
  migrations of saved Legacy picks to ARC stay.
- **The on-device virtual model is now `rayline-auto` (breaking).** The local
  router, the proxy and the hybrid execution core used `rayline-router` as
  their "route by `routes.main`" model; they now use `rayline-auto`, and no
  longer treat `rayline-router` specially. On the local router it is now an
  unknown model name like any other, so it follows `routes.main`: `raydock
  codex --model rayline-router` and an exported
  `ANTHROPIC_MODEL=rayline-router` on the local plane run on the config's main
  route. The local router and the proxy no longer rewrite the retired
  `claude-rayline-router-*` and `rayline-router-balanced` aliases either.

## 0.4.0 (2026-11-05)

### New

- **Each forwarded request records the cloud router's id.** When Rayline Local
  forwards a request to the router, the id the router gives it
  (`x-rayline-route-id`) is kept with the request: the route log
  line is followed by `… route upstream request_id=… upstream_request_id=rt_…`,
  the metrics snapshot's request rows gain `upstream_request_id`, `raydock top`
  shows it in an `UPSTREAM` column on its wide layout, and the response carries
  it back as `x-rayline-upstream-request-id`. Route log lines now also name the
  local `request_id`, so the two lines can be matched. A request answered on the
  device without a cloud routing hop has no upstream id and shows `-`; one the
  router redirected to the device keeps the router's id.

### Changed

- **V is now `v_value` (breaking).** A profile file sets V under
  `decisions.routing.v_value`, which hybrid routing sends to the Routes API
  as `routing.v_value`, the API's new name for the member. Hybrid routing
  needs this release from the moment the Routes API stops accepting
  `value_usd`.
- **The default hybrid profile decides with ARC Swift and sends no V
  (breaking).** It asks the router for decisions with model `arc-swift-1.0`
  instead of `rayline-router` with a V of 0.5. Swift decides at its own fixed
  V and picks cheap, so it uses the local model most. The `hybrid.routing`
  override in `settings.json` no longer takes a V: `v_value` and `value_usd`
  are refused at `start` (`settings hybrid.routing.v_value is not
  supported`); remove the key. A profile file that wants its own V sets
  `decisions.routing.v_value` with model `arc-1.0`.
- **Hybrid routing is no longer a development-router preview.** A production
  `raydock hybrid start` no longer prints the preview note: hybrid uses the
  production router by default, and `--env dev` selects the development one.
  The hybrid docs now say what a turn costs: every turn bills one ARC decision,
  local turns included, and a cloud turn also bills the provider's
  pass-through price.

### Fixed

- **Hybrid routing decides again on the hosted router.** Since the router moved
  to the Routes API V1 contract (2026-11-04), every `raydock hybrid` decision
  was refused with `routing has unsupported member(s): exclusive` and the turn
  failed. The default profile no longer sends `exclusive` or a precomputed
  `price_sha256`, and the bundled execution core checks the router's V1 price
  acknowledgement. A profile file from before V1 (with `routing.exclusive`,
  `value_usd` or a precomputed `price_sha256`) is refused by the core at
  startup, and the runtime log names the replacement.
- **Chat Completions requests show in `raydock top`.** The local router's
  `/v1/chat/completions` listener now records the same request lifecycle as
  the Messages and Responses listeners: the route when it is decided, the
  first token, the Chat `usage` counts (prompt, completion, cached) and the
  completion, or the error for an upstream failure, a cut-off stream or a
  refused request. Cloud requests from OpenAI-compatible harnesses (OpenClaw,
  Roomote, Cline in OpenAI mode) were missing from `raydock top` and its
  totals; requests served on-device already appeared and are unchanged. On a
  passthrough route, a streaming request that did not ask for usage now asks
  the upstream for it so the row carries real token counts; that extra usage
  chunk is withheld from the client, whose stream is unchanged.
- **Non-streaming Responses turns on an Anthropic route no longer stay active
  in `raydock top`.** When a Codex `/v1/responses` turn is served through an
  `anthropic_messages` endpoint and the translation is not stream-to-stream
  (a `stream: false` turn such as Codex compaction, or a `stream: true` turn
  the upstream answered in one piece), the local router now closes the
  request's metrics row with the upstream's token counts (whole prompt,
  output, cached part) and the selected model. Those turns were left in
  `active` with state `routed` forever, so `raydock top` listed them as
  never finishing, `active_requests` was wrong and the completed and token
  totals missed them. A buffered upstream stream that reported an error or
  stopped early is recorded as an error, not a completion. Response bodies
  are unchanged. A Responses turn served on-device is recorded by the
  adapter, as before, and no longer also leaves a bare duplicate row.

## 0.3.1 (2026-11-02)

### Changed

- Hosted `raydock claude` now picks the model of every Claude Code subagent and
  background task, ignoring the models Claude Code's own agent definitions
  name. Without `--model`, they run on `arc-swift-1.0`; with `--model X`
  (before or after `--`, or `ANTHROPIC_MODEL`) and `--route all`, they run on X
  too, instead of only the main agent. A cloud-only `--config` uses its
  `routes.subagent`, else the main model. Raydock sets
  `CLAUDE_CODE_SUBAGENT_MODEL_FORCE=1` for this, so Claude Code's Explore and
  Plan agents no longer inherit the main agent's model, and a model a custom
  agent names, or the main agent passes when spawning, is overridden too. Under
  `--route subagents`, subagents stay on `arc-swift-1.0` whatever `--model`
  says. Your own `CLAUDE_CODE_SUBAGENT_MODEL` keeps Claude Code's precedence.
  The hosted router still replaces a `claude-*` model on subagent and
  background turns.
- Under `--route subagents` with the hosted router, a routed subagent that
  names a Claude model now runs on its `--config` route's model
  (`routes.subagents.<type>`, then `routes.subagent`), else `arc-swift-1.0`,
  instead of always on the Legacy Rules Router (`rayline-router`).
- **`raydock openclaw` speaks Chat Completions and keys its own episode.**
  The generated `rayline` provider is now `openai-completions` (OpenClaw's
  default protocol) with `compat.sendSessionAffinityHeaders`, so OpenClaw
  sends its session id as
  `x-session-affinity` and a `--session-id` resume keeps its routing episode.
  The launcher no longer sends a per-launch `x-conversation-id` for OpenClaw;
  `RAYLINE_HARNESS_CONV_ID_FALLBACK=1` still forces it. OpenAI-style hosted
  tools (`web_search`) are not carried on Chat Completions; function tools
  are.
- **Harness launchers rely on the router's session keying.**
  `raydock opencode|pi|omp|hermes|openclaw` no longer send a per-launch
  `x-conversation-id` unless `RAYLINE_HARNESS_CONV_ID_FALLBACK=1` forces it.
  The router keys episodes on each harness's own session id on dev and prod
  (prod since 2026-10-23), so `--resume` / `-c` in a new launch keeps the
  episode. Hermes on Messages sends no session id, and the router now derives
  its episode from the conversation instead; with the per-launch id, a
  `delegate_task` subagent's turns and the title side call were all keyed
  into the parent's episode. Under `raydock hybrid run` the Hermes Messages
  provider keeps the per-launch id, since the execution core refuses a request
  with no identity.

### Fixed

- **omp's ARC models no longer appear free.** `raydock omp` supplies Sage and
  Swift's fixed input, output, cache-read and cache-write rates (effective card
  26, 2026-10-27). Hosted ARC entries advertise text input and leave reasoning
  effort to ARC instead of exposing unsupported client thinking controls.
  Existing context/output budgets, saved role choices and hybrid capabilities
  are unchanged. Hybrid prices remain variable; omp versions that label missing
  prices as free must not be used as billing evidence. The
  [omp onboarding guide](docs/getting-started.md#oh-my-pi-omp) now starts with
  the launcher, explains isolation and resuming, and includes a no-inference
  metadata check.
- **omp model labels omit deployment environments:** the picker and composer
  show `ARC Sage` and `ARC Swift` without `(prod)` or other environment
  suffixes. The launch banner still identifies the environment.
- **Hosted omp no longer lists the Legacy Rules Router.** Its model picker
  offers only ARC Sage and ARC Swift. Hybrid retains the execution core's
  active virtual model, which happens to use the same `rayline-router` wire ID.
- **Existing omp sessions survive the Legacy model's removal.** Hosted
  launches migrate retired role and session selections to ARC while preserving
  other settings and historical messages, usage and served-model metadata.
  Journal modification times are retained so `--continue` keeps selecting the
  correct conversation. Migration cooperates with omp's settings lock and
  refuses active writers, concurrent changes and unsafe state instead of
  overwriting it; hybrid state is never migrated by this cutover.
- **Supply-chain checks recognize the reviewed Boost license** for the pinned
  `xxhash-rust` dependency used by omp's native lock compatibility. The exception
  is crate/version-scoped, and distributed notices include the license text.

## 0.3.0 (2026-10-28)

### Changed

- **Rayline Local is now Raydock.** Use `raydock` for new commands; `rayline`
  remains a supported alias. Configuration, credentials, environment variables
  and update channels keep their paths and names. The source repository is now
  [rayline-ai/raydock](https://github.com/rayline-ai/raydock).
- **Harness launches run on ARC by default** instead of the Legacy Rules
  Router. Hosted `raydock claude`, `raydock codex` (default Rc-Rc) and the
  `opencode`, `pi`, `omp`, `hermes` and `openclaw` launchers give the main agent
  `arc-sage-1.0`, and subagents and background tasks `arc-swift-1.0` where the
  harness has a separate setting for them (Claude Code's
  `CLAUDE_CODE_SUBAGENT_MODEL` and `ANTHROPIC_DEFAULT_HAIKU_MODEL`, OpenCode's
  `small_model`, omp's `smol` role, Codex's `subagent` route). The Legacy Rules
  Router stays available by explicit choice: `--model rayline-router`, the
  harness's own model flag or picker (every generated config lists it;
  `raydock codex --model rayline-router` too), or a route model in
  `router.json`. Choices you made are kept: an explicit model,
  your own Claude Code model variables, `--config` routes, and the model pi
  and omp saved (Hermes and OpenClaw configs are rewritten each launch, as
  before, so pick Legacy there per run). Generated defaults that were never edited (the 0.2.x
  `router.json`, pi `settings.json` and omp `config.yml`) move to ARC once.
  Local and hybrid sessions are unchanged.
- A cloud-only `raydock claude --config` launches the main agent with the model
  its `routes.main` names and, with `--route all`, subagents with its
  `routes.subagent` model.
- Under `--route subagents`, a request naming an ARC model (`arc-*`) always
  goes to the router, like `rayline-router`: Anthropic serves neither.

## 0.2.14 (2026-10-27)

### Fixed

- **Hybrid routing checks the local model is Qwen3.8-27B.** `rayline hybrid
  start` reads what the runtime has loaded (llama.cpp `/props`, Ollama
  `/api/show`); another model is refused for local routing and the session
  runs cloud only, with an error naming what was found. Runtimes that do not
  report it get a warning. Release archives now include `NOTICE`, which
  attributes the embedded Qwen3.8-27B tokenizer data (Apache-2.0).
- **`rayline hybrid run opencode` works with OpenCode 2.** Every turn was
  refused with "output allowance exceeds the admitted deployment limit":
  every hybrid harness (OpenCode, omp, OpenClaw, Hermes, Claude Code, Codex)
  now advertises the runtime's admitted output cap and text-only input, as
  Pi already did (#123). OpenCode 2 launches (`rayline opencode` and
  `rayline hybrid run opencode`) add `--standalone`, since OpenCode's shared
  background service never saw Rayline's configuration, and the OpenCode
  config drops the `attachment` and `reasoning` keys OpenCode 2 rejects.
  Hybrid refuses OpenCode `-c` / `--session` / `--fork` like other resumes.
- **Clearer hybrid errors.** `rayline hybrid start` checks Decisions once
  before reporting ready (a rejected key names the `auth login` command; a
  starting preview cell is waited for). During a session, a Decisions key
  refusal names the same command, and a Decisions outage is retried once,
  then reported as retryable (HTTP 503).
- **`rayline local test`** checks Chat Completions, the API hybrid routing
  uses, and reports Anthropic Messages separately.

### Changed

- **Hybrid context.** Harnesses are told the window is 1,000,000 tokens; the
  default cloud targets (Opus 5.5, Sonnet 5.5) carry their 1M window; the
  local window defaults to 98,304 tokens (`hybrid.local_context_window`,
  `--local-context-window`), capped by a llama.cpp runtime's loaded window.
  Ollama and MLX no longer need `--local-context-window`, but must be loaded
  with that window. Hermes uses its Responses provider under hybrid.

## 0.2.13 (2026-10-26)

### New

- **`rayline hybrid` (preview).** Routes each turn to a local Qwen 3.8 27B on
  your machine or to a cloud model through the Rayline router, and your coding
  harness runs unmodified. Preview: development router only, until production
  support lands. macOS 13+ or Linux
  ([#120](https://github.com/rayline-ai/raydock/pull/120),
  [#122](https://github.com/rayline-ai/raydock/pull/122)). Setup, with the
  development environment configured in `~/.config/rayline/settings.json`:
  1. `rayline --env dev auth login`
  2. `rayline local custom --url http://127.0.0.1:8080/v1 --model qwen3.8-27b`
     (your own llama.cpp, Ollama or MLX server)
  3. `rayline hybrid start --env dev --cell pf-5505cbf8-v2`, then
     `rayline hybrid run pi` (also `claude`, `codex`, `opencode`, `omp`,
     `hermes`, `openclaw`)

  Ollama and MLX also need `--local-context-window <tokens>` on `start`.
  Start a fresh harness session after every `start`; resuming a saved session
  is not supported. See [docs/hybrid-local-setup.md](docs/hybrid-local-setup.md).
- **Hybrid routing overrides.** An optional `hybrid.routing` block in
  `~/.config/rayline/settings.json` sets `policy` (`utility` or
  `prefer_local`), `min_p_pass` (strictly between 0 and 1, `prefer_local`
  only), `schedule`, `value_usd` (0.1 to 5) and `local_price` (default profile
  only). `prefer_local` needs a profile file with a qualified llama.cpp prompt
  counter. Unknown keys are refused. Details in
  [docs/hybrid-local-setup.md](docs/hybrid-local-setup.md#the-optional-override-block).
- **`rayline claude-desktop` sets up and launches Claude Desktop in one
  command (macOS).** It selects a **Rayline ARC** profile and opens the app.
  This switches the whole app off your subscription, with Chat and Cowork off,
  until you run `rayline claude-desktop off`
  ([#121](https://github.com/rayline-ai/raydock/pull/121)).

### Changed

- The installers (curl, Homebrew, PowerShell) and `rayline update` now also
  install `rayline-core`, the hybrid execution core. If you update from an
  older Rayline and the core is still missing, run `rayline update` again.
- Rolling back works: `rayline update --version 0.2.12+ee1afb00b6cd` installs
  a release whose signed checksums list no core, skips the core and removes the
  installed one. Pass the full version from the release notes; a bare
  `--version 0.2.12` says so. From 0.2.13 on, the core is required, and the
  update fails closed if it is missing
  ([#125](https://github.com/rayline-ai/raydock/pull/125)).

### Fixed

- Claude Code in proxy mode keeps the full conversation history. Rayline turns
  off Claude Code's message threads by default (your own setting is kept) and
  refuses router-bound thread continuations locally, so Claude Code resends the
  full history ([#119](https://github.com/rayline-ai/raydock/pull/119)).

### Security

- Hybrid writes `core-config.json` owner-only.
- Your saved Rayline key goes only to HTTPS endpoints on the signed-in
  environment's router host.
- Hybrid decision requests go only to the Rayline router or to loopback
  ([#125](https://github.com/rayline-ai/raydock/pull/125)).
- Tokio is updated to 1.53.3 for RUSTSEC-2026-0386
  ([#121](https://github.com/rayline-ai/raydock/pull/121)).

### Known issues

Hybrid issues are tracked in [#123](https://github.com/rayline-ai/raydock/issues/123).

- In `rayline hybrid run`, Pi gets the local model's output and input limits,
  and the largest cloud context window (200,000 tokens with the default
  profile), so turns that outgrow the local model hand off to cloud. Other
  harnesses keep their usual limits and may send requests the core rejects.
- Ctrl-C during `rayline hybrid start` can leave the runtime running. Run
  `rayline hybrid stop` once it is ready; if it never gets ready, stop the
  process whose `pid` is in `~/.rayline/hybrid/supervisor.json`, wait for it
  to exit, then run `rayline hybrid stop`.
- Smaller hybrid issues: a transient connection error can stop the gateway;
  `--state-dir` makes an existing directory owner-only; and with the core
  missing, `rayline update --check` reports an error even when up to date.
- `rayline claude-desktop --prefixed-ids` forwards `rayline/arc-sage` and
  `rayline/arc-swift`, which the hosted router refuses (400). The default bare
  ids work: the picker's `arc-sage` and `arc-swift` map to the ARC tiers
  `arc-sage-1.0` and `arc-swift-1.0`.

Release notes for 0.2.12 and earlier are on
[GitHub Releases](https://github.com/rayline-ai/raydock/releases).
