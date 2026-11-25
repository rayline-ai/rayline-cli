# Local mode

In local mode a coding harness talks to a Rayline process on your machine
instead of to Rayline Platform. Local mode lets you use local models (on your
hardware) and your own servers alongside Rayline Platform.
For every request, ARC picks the model, and the local process sends the
request to one of your endpoints listed in `settings.json`, or to Rayline
Platform for any other model.

Cloud mode, the default, needs none of this. See
[Getting started](getting-started.md).

## Terms

- **Local mode** / **cloud mode**: whether a Rayline process runs on your
  machine.
- **Local ARC** / **cloud ARC**: where the routing decision is made.
  `local.routing.router` sets it: `local`, on this machine, or `cloud`, in
  Rayline Platform through the Rayline Routes API.
- **Local models**: models served on this machine. **Your own servers**:
  models you host elsewhere. **Your endpoints**: the two together, plus any
  other provider you list.

Local models need local mode, but local mode doesn't need local models: for
example, local ARC with Rayline Platform only. Local ARC and local models are
independent: you can use either one without the other.

## Setups

| Setup | `mode` | `local.routing.router` | Rayline process | ARC decides |
|---|---|---|---|---|
| Cloud mode | `cloud` | – | none | in Rayline Platform |
| Local mode, cloud ARC | `local` | `cloud` | on this machine | in Rayline Platform |
| Local mode, local ARC | `local` | `local` | on this machine | on this machine |

## Where inference can run

In local mode, ARC chooses where each turn runs.

| Setup | Rayline Platform | Local models (this machine) | Your own servers or other providers |
|---|---|---|---|
| Cloud mode | yes | no | no |
| Local mode, cloud ARC | yes | yes | yes |
| Local mode, local ARC | yes | yes | yes |

Local models and your own servers are both entries in
`local.inference.endpoints`. Any server that speaks the tool's protocol works,
on this machine or elsewhere. With none listed, every turn runs on Rayline
Platform.

## What you need

- Rayline, installed and signed in with `rayline auth login`.
- A config with a `local.routing` section: `~/.config/rayline/settings.json`,
  or a file you pass with `--config`. See [Configure](#configure).
- On macOS, macOS 13 or later.

Your endpoints are optional. With none listed, every model ARC picks goes to
Rayline Platform. Rayline doesn't start, download or stop your endpoints, and
`rayline local stop` leaves them running.

ARC makes every routing decision. See
[ARC pricing](https://docs.rayline.ai/arc/pricing) for how sessions are billed.

## Configure

Set `mode` to `local`, and add a `local` section. `local.routing` says how a
model is picked. `local.inference` lists your endpoints
(`endpoints`) and the models they serve (`models`); it is optional.

This `settings.json` runs local mode with no server of your own: ARC Sage in
Rayline Platform decides each turn, and every model it picks runs on Rayline
Platform.

```json
{
  "mode": "local",

  "local": {
    "routing": {
      "router": "cloud",
      "decisions_model": { "id": "arc-sage-1.0" },
      "classifier_model": "anthropic/claude-haiku-5.5",
      "policy": { "preset": "arc-policy.prefer-local.v1" }
    }
  }
}
```

To run some turns on your own servers, add `local.inference`:

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
        },
        "ollama": {
          "base_url": "http://127.0.0.1:11434",
          "protocols": ["openai_chat", "anthropic_messages"]
        }
      },
      "models": {
        "z-ai/glm-5.3-flash": {
          "endpoint": "gpu-box",
          "model": "GLM-5.3-Flash-EXL3",
          "context_window": 1000000,
          "max_output_tokens": 32000,
          "tokenizer": "zai-org/GLM-5.3-Flash",
          "price": { "input": 0, "output": 0, "cache_read": 0, "cache_write": 0 }
        }
      }
    },
    "routing": {
      "router": "cloud",
      "decisions_model": { "id": "arc-sage-1.0" },
      "classifier_model": "anthropic/claude-haiku-5.5",
      "policy": {
        "preset": "arc-policy.prefer-local.v1",
        "overrides": {
          "selection": { "min_p_pass": 0.5 },
          "schedule": { "model": ["cache_boundaries", "user_messages"] }
        }
      }
    }
  }
}
```

The `cloud` section is optional here. With it, `rayline run <HARNESS> --mode
cloud` runs in cloud mode from the same file. Rayline checks the whole file
whatever the mode, so a mistake in either section stops every command that
reads the file.

Rayline Platform is built in and has no entry. It serves every protocol a harness
speaks.

### `local.inference.endpoints`

These are your endpoints, keyed by an id you choose: local models, your own
servers, and any other provider that speaks the harness's protocol.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `base_url` | string (URL) | yes | The server's address. |
| `protocols` | list of `anthropic_messages`, `openai_chat`, `openai_responses` | yes, non-empty | The wire protocols it serves. |
| `api_key_env` | string | no | The name of the env var that holds the server's key. Never put the key itself in the file. |

Each harness speaks one protocol:

| Harness | Protocol |
|---|---|
| `claude`, `pi`, `omp`, `opencode` | `anthropic_messages` |
| `codex`, `hermes` | `openai_responses` |
| `openclaw` | `openai_chat` |

Requests pass straight through to an endpoint, with no translation. In a
local-mode run, every endpoint in the config must list the protocol of the
harness you run, or Rayline refuses to run. With the example above, `claude`
and `pi` can run in local mode, but `codex` can't, because `ollama` doesn't
list `openai_responses`.

### `local.inference.models`

These are the models ARC can pick that Rayline Platform doesn't serve. Each is
keyed by the model id ARC returns.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `endpoint` | string | yes | An id in `local.inference.endpoints`. |
| `model` | string | yes | The name the server knows the model by. |
| `context_window` | integer > 0 | yes, when the local process starts | The context window in tokens. |
| `max_output_tokens` | integer > 0 | yes, when the local process starts | The output ceiling in tokens. |
| `tokenizer` | string | no | A Hugging Face tokenizer repo, used to count the tokens of each request. |
| `price` | object with `input`, `output`, `cache_read`, `cache_write` (numbers >= 0) | no | Replaces ARC's price for this model only. |

When ARC picks a model listed here, the local process sends the request to
that model's endpoint under its `model` name. ARC doesn't publish the models
it knows yet, so every entry needs `context_window` and `max_output_tokens`,
and the local process refuses to start without them.

With a `tokenizer`, the requests sent to the model are sized in the model's
own tokens, whichever harness sends them, so a model with a small context
window takes the turns that fit it. For a vision model, the images in a
request are counted too. A request with content its chat template doesn't
render as text, such as a document, is sized from its bytes instead. The repo
must publish a chat template: name a model's instruct or chat variant, not its
base model, for example `google/gemma-4-E4B-it`, not `google/gemma-4-E4B`.

The only other models ARC can pick are three Rayline Platform models:
`anthropic/claude-opus-5.5`, `anthropic/claude-sonnet-5.5` and
`anthropic/claude-haiku-5.5`. Each has a 1,000,000-token context window and a
128,000-token output ceiling, and ARC prices them. When ARC picks one, the
request goes to Rayline Platform. A `models` entry keyed by one of these ids
replaces that cloud model.

Every entry works the same way. No setting marks a model as local.

### `local.routing`

| Key | Type | Required | Meaning |
|---|---|---|---|
| `router` | `cloud` or `local` | yes | `cloud`: Rayline Platform's ARC decides. `local`: ARC runs inside the local process on this machine, and no routing decision goes to Rayline Platform. |
| `decisions_model` | object with `id`, and optionally `v_value` | yes | The ARC model that makes the decisions, for example `{ "id": "arc-sage-1.0" }`. With `router` `local`, `id` must be `arc-swift-1.0` or `arc-sage-1.0`, and `v_value` isn't allowed. With `router` `cloud`, a `v_value` (a number >= 0) asks ARC for its answer at that value: `{ "id": "arc-1.0", "v_value": 2 }`. |
| `classifier_model` | string | to run Claude Code | The model that serves Claude Code's auto-mode safety classifier calls. These calls go to Rayline Platform. |
| `policy.preset` | non-empty string | yes | The id of an ARC preset: `arc-policy.package-default.v1`, `arc-policy.hybrid-default.v1` or `arc-policy.prefer-local.v1`. It is sent to ARC unchanged. |
| `policy.overrides` | object | no | Overrides in ARC's own `routing.policy.overrides` shape, sent to ARC unchanged. ARC validates them. |
| `arc_release` | object with `repo`, `revision`, `manifest_sha256`, and optionally `dir` | no | Overrides the built-in ARC release for `local` decisions with a Hugging Face repo, a full commit id, the sha256 of the release's `manifest.json`, and the repo directory it is in (default the repo root). For a private repo, set `HF_TOKEN` or sign in with `huggingface-cli login`. |

`local` decisions are a preview. They run the ARC encoder,
`rayline-arc-encoder`, which releases ship for macOS arm64, and for Linux
(x86_64 and aarch64) and Windows x86_64 with an NVIDIA GPU. Installing or
updating Rayline doesn't download it: the first `rayline local start` (or
local-mode `rayline run` that starts the local process) with
`local.routing.router` `local` downloads it from the same release as `rayline`
into `~/.rayline/arc/<version>/` and checks it against the release's
checksums. After that, `rayline update` fetches the new version's encoder
along with `rayline`. Before it downloads anything, start checks that the
platform has an encoder build and, on Linux and Windows, that the NVIDIA
driver is installed; otherwise it fails and says to use `--router cloud`.
macOS arm64 needs no driver. Rayline 0.5.3 and earlier installed the encoder
beside `rayline`; Rayline no longer uses that copy, so you can delete
`rayline-arc-encoder`, `rayline-arc-encoder-LICENSE` and, on Windows, the
`cublas*.dll` files from the folder `rayline` is in.

On Linux:

- x86_64 needs an NVIDIA GPU of compute capability 7.5, 8.x, 9.0 or 12.0
  (T4, A100, A10/L4/RTX 30 and 40, H100, RTX 50), and aarch64 one of 9.0 or
  12.1 (GH200, GB10). Compute capability 10.x (B200, GB200) isn't supported
  yet.
- The encoder needs the NVIDIA driver and no CUDA toolkit: 525.60.13 or later
  for T4 through H100, 570 or later for RTX 50, and 580 or later for GB10.
- It needs glibc 2.28 or later (as `rayline` does) and AVX2 on x86_64, and
  is 0.6 to 0.8 GB.
- There is no build for other GPUs (Vulkan) or for the CPU alone.

On Windows:

- It needs Windows 10 version 1903 or later, or Windows Server 2022, on
  x86_64, with an NVIDIA GPU of compute capability 7.5, 8.x, 9.0 or 12.0 and
  NVIDIA driver 576 or later. No CUDA toolkit or Visual C++ runtime: the
  encoder, like `rayline.exe`, links the C runtime statically.
- It is the encoder plus the cuBLAS DLLs it loads, `cublas64_12.dll` and
  `cublasLt64_12.dll`, about 1 GB together.
- Windows on Arm and other GPUs aren't supported.

When the encoder finds no GPU it can use (no NVIDIA GPU, or a driver too old
for it), start fails with an error that says so instead of running ARC on the
CPU, which takes seconds a turn.

On first use, start also downloads the
release and about 1 GB of encoder weights into the Hugging Face cache and
checks them against their pins, shown in the same progress line. Later starts hash a cached file again only
if it changed since it was last checked (a stamp in `~/.rayline/arc-verified/`);
on Windows, where a file's metadata doesn't show every change, every start
hashes the whole release again. With `local.routing.router` `local`, every
turn routes by a local ARC decision, with no fallback: a turn ARC can't
decide fails. Only Anthropic Messages harnesses (Claude Code, OpenCode, Pi,
omp) are decided locally, so `rayline run` refuses Codex, Hermes and
OpenClaw; turns with images or documents fail; and a `models` entry the
release can't route to is skipped with a warning. Section 5.4 of the
[spec](spec.md) has the details.

The [ARC routing policies](https://docs.rayline.ai/arc/routes/hybrid)
page describes how ARC applies each preset and which overrides it accepts.

### Env overrides

These keys can be set from the environment:

| Key | Env var |
|---|---|
| `mode` | `RAYLINE_MODE` |
| `local.routing.router` | `RAYLINE_LOCAL_ROUTING_ROUTER` |
| `local.routing.decisions_model.id` | `RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_ID` |
| `local.routing.decisions_model.v_value` | `RAYLINE_LOCAL_ROUTING_DECISIONS_MODEL_V_VALUE` |
| `local.routing.classifier_model` | `RAYLINE_LOCAL_ROUTING_CLASSIFIER_MODEL` |
| `local.routing.policy.preset` | `RAYLINE_LOCAL_ROUTING_POLICY_PRESET` |

- From lowest to highest precedence: built-in defaults, the config file, env
  overrides, then flags.
- An override is validated the same way as the file value.
- An override only changes a value. The config file still has to have a
  `local.routing` section, also for `rayline local start --router`.
- Commands that don't use `local` (`auth`, `top`, `update`, `local status`,
  `local stop`, and a cloud-mode run) don't read the `RAYLINE_LOCAL_*` env
  vars.
- `endpoints`, `models`, `arc_release` and `policy.overrides` can't be set
  from the environment. Put them in a file and pass it with `--config`.

## Start and run

With `"mode": "local"` in `settings.json`:

```bash
rayline local start
rayline run claude
rayline run pi -- "Fix the failing test in parser.py"
rayline local status
rayline local stop
```

With `"mode": "cloud"`, add `--mode local` to a run: `rayline run claude
--mode local`.

Pi needs version 0.87.1 or later in local mode (`npm install -g
@earendil-works/pi-coding-agent`). An older Pi doesn't send its session id,
so a local-mode `rayline run pi` refuses it.

| Command | Does |
|---|---|
| `rayline local start [--config <file>] [--router <cloud\|local>]` | Starts the local process from the `local` section of the default config file, or of `<file>`, whatever `mode` says. The config must have `local.routing`. `--router` overrides `local.routing.router`. |
| `rayline local status [--json]` | Shows whether the local process is running, its address, and the config file and environment it was started from. `--json` prints it as JSON (see [Local decisions for your own Pi](#local-decisions-for-your-own-pi)). |
| `rayline local stop` | Stops the local process. Your endpoints keep running. |
| `rayline run <HARNESS> [--mode <cloud\|local>] [--config <file>]` | Runs the harness in the mode `--mode` names, else `RAYLINE_MODE`, else the config's `mode`. In local mode it starts the local process if it isn't running. `--config <file>` uses `<file>` instead of `~/.config/rayline/settings.json`, never merged with it. |

- `--mode` and `--config` combine.
- Only one local process runs per machine.
- Running `rayline local start` while the process is already running is an
  error. The error names the running process and its config file.
- A local-mode run whose config file differs from the running process's file
  is an error. To use another file, run `rayline local stop`, then start again
  with that file.

### Known issue: starting over SSH on Windows

On Windows, `ssh <host> rayline local start` prints the ready line, then
doesn't return until the local process stops. A local-mode `rayline run`
over SSH that starts the local process does the same once the harness exits.
It happens when the SSH session has no terminal, as when you pass the command
to `ssh`. `start /b` doesn't help: the session still waits, or, when it ends
first, the start is stopped with it. A fix is planned
([#318](https://github.com/rayline-ai/rayline-cli/issues/318)).

Until then, give the session a terminal with `-tt`:

```bash
ssh -tt <host> rayline local start
```

The command returns once the local process is ready, and the process keeps
running after the session closes. `-tt` forces a terminal even when `ssh`'s
own input isn't one, as in a script or CI, and the output then carries
terminal control sequences. An interactive SSH session isn't affected:
`exit` closes it while the local process keeps running.

### Models

The model local mode declares to each harness is `local/` followed by
`local.routing.decisions_model.id`, for example `local/arc-sage-1.0`, and
the harness's model picker shows it. A model you name with the harness's own
flag, such as `--model` or `-m`, is a request, and ARC decides which model
runs. OpenClaw works only with the declared model. A
model pinned in the harness's own config, such as a subagent model in Claude
Code's `settings.json`, overrides the models Rayline sets.

## Local decisions for your own Pi

The [pi-arc](https://github.com/rayline-ai/pi-arc) extension can ask the
local process for each turn's model while you keep using your own Pi, with
its own extensions, keys and models. This is a preview with the same limits
as `local` decisions: macOS on Apple Silicon, or Linux or Windows with an
NVIDIA GPU.

```bash
rayline auth login
rayline local start --router local
```

The config needs a `local.routing` section; `--router` sets only its
`router`. For example, in `~/.config/rayline/settings.json`:

```json
"local": {
  "routing": {
    "router": "local",
    "decisions_model": { "id": "arc-sage-1.0" },
    "policy": { "preset": "arc-policy.prefer-local.v1" }
  }
}
```

`local.routing.classifier_model` is needed only to run Claude Code.

pi-arc finds the local Routes API with `rayline local status --json`:

```json
{
  "running": true,
  "pid": 40049,
  "url": "http://127.0.0.1:57693",
  "config": "/Users/you/.config/rayline/settings.json",
  "environment": "prod",
  "router": "local",
  "decisions_model": "arc-sage-1.0",
  "routes": {
    "url": "http://127.0.0.1:57696/v1/routes",
    "token_file": "/Users/you/.rayline/local/routes-token"
  }
}
```

- `routes` appears only with `local` decisions. When nothing runs, the
  output is `{"running": false}`.
- The token is never printed. Read it from `token_file` (mode 0600) and send
  `Authorization: Bearer <token>` to `routes.url`. It admits `POST /v1/routes`
  only; it can't send anything to Rayline Platform.
- The URL and token change on every start. Run `rayline local status --json`
  again after a connection error or a 401.
- The request is the Routes API request: `model` (`arc-swift-1.0` or
  `arc-sage-1.0`), `routing.execution`, `routing.policy` and the prices of
  the models you offer, plus the turn's `system`, `messages` and `tools`.
  `X-Conversation-Id` and `X-Rayline-Branch-Id` group a conversation's turns;
  `X-Rayline-Wire-Format` (`anthropic.messages.v1`) says how to read the
  request.
- You can offer any model the ARC release knows, including your own local
  models such as `z-ai/glm-5.3-flash` priced at zero. A model it doesn't know
  is refused.
- There is no fallback: when the local process isn't running or can't
  decide, the turn fails.

## Watch requests

```bash
rayline top
rayline top --json
```

`rayline top` shows live request metrics from the local process. `--json`
prints one snapshot and exits.

```
Rayline top                                    local process · up 12 min · 14:35
Routing   ARC Sage decides on this Mac · policy: prefer local
Ran on    Rayline Platform 3 · your servers 1
Tokens    35.2k in (72% cached) · 1.4k out
Decisions 4 · median 108 ms · 1 errors

 TIME  TOOL     TURN        DECIDED           MODEL            RAN ON        OUT
 14:35 codex    tool result Mac 95 ms →       qwen3.5-35b      spark     ⠋ 3.2 s
 14:35 claude   task start  Mac 122 ms →      claude-haiku-5.5 Platform      888
   ▸ why: best value for this task · no local model configured
 14:34 codex    tool result Mac 95 ms →       qwen3.5-35b      spark         118

 ↑↓ select · enter details · f filter: all · q quit
```

- The header says where decisions are made, with the policy preset, where
  requests ran, the token totals with how much of the input the providers'
  prompt caches served, and the decisions' count and median time.
- Each row is one request, newest first, with requests in flight at the top.
  `TOOL` is the harness that sent it: each local-mode launch points its
  harness at its own path on the gateway, `/h/<harness>`. `DECIDED` says where
  ARC decided and how long it took (`—` for a request no decision routed, such
  as Claude Code's auto-mode classifier), `RAN ON` is `Platform` or the
  `local.inference.endpoints` entry that served it, and `CACHED` is the share
  of its input the provider's prompt cache served.
- The selected row's `why` line gives the decision's reason, such as
  `local model good enough (pass 0.82 ≥ 0.75)`.
- `Enter` opens the selected request's details, including how many prompt
  tokens ARC's encoder reused from an earlier turn when it decided on this
  machine. `Esc` closes them.
- `f` cycles the list through all requests, then each harness that has
  requests.
- Narrow terminals drop `TOK/S`, `TTFT`, `CACHED`, `IN` and `TOOL`, in that
  order.

`--json` carries each row's `harness`, `thinking_level`, `decision_reason`,
`p_pass` and `min_p_pass`, `decision_ms` (for decisions made here or in
Rayline Platform), `arc_encoded_tokens` and `arc_restored_tokens`, and
`cache_write_tokens`, plus `totals.cache_read_tokens` and the policy preset in
`router.preset`.

The live view waits for a local process: while none runs it shows
`Local process: not running` and checks again on every refresh, and when the
local process restarts it follows the new one. `--json`, or output that isn't
a terminal, with no local process running says so and exits with an error.

## Config errors

Rayline checks the config once, when it loads, and refuses to run with an
error that names the key. It refuses:

1. An unknown key, at any level Rayline defines, in any section of the file.
2. A wrong type, an empty string, or a value out of range.
3. In a local-mode run, an endpoint that doesn't list the protocol of the
   harness being run. Cloud mode doesn't use endpoints, and
   `rayline local start` runs no harness, so neither checks this.
4. A model whose `endpoint` isn't in `local.inference.endpoints`.
5. Local mode without `local.routing`, or a `local.routing` section that is
   missing a required key, whatever the mode. An env override doesn't give a
   missing key. When the file has no `local` section, a `local` section
   with no `routing`, or a `local.routing` missing a required key, the error
   says so and links this page.
6. A `local.routing` env override when the config file has no `local.routing`
   section.
7. A `decisions_model.v_value`, or a `decisions_model.id` other than
   `arc-swift-1.0` and `arc-sage-1.0`, with `router` `local`.
8. A `--config <file>` that doesn't exist. Only the default file is optional.
9. A key repeated in one JSON object, an empty map key, a URL that isn't an
   absolute `http` or `https` URL, a protocol listed twice in `protocols`, or a
   `price` that is missing one of its four fields.

Before the local process serves anything, it also refuses:

10. An `api_key_env` that names an env var that isn't set.
11. A model entry that is missing `context_window` or `max_output_tokens`.
12. An `api_key_env` that names `RAYLINE_ROUTER_API_KEY`, which holds the
    Rayline router key.

## Files

The local process keeps its state in `~/.rayline/local/`:

| File | Holds |
|---|---|
| `core-config.json` | The config handed to the execution core. |
| `core-config.counted.json` | The same config with Rayline's token counter added. |
| `gateway-token` | The secret the CLI and the local process share. |
| `ready.json` | The local process's address, once it is up. |
| `supervisor.json` | What is running. |
| `runtime.lock` | The lock that keeps it to one local process. |
| `runtime.log` | The local process's log. |
| `ready.arc-core.json` | With `local` decisions: the core config pointed at the local routes service. |
| `routes-token` | With `local` decisions: the token other local programs, such as pi-arc, present to the local Routes API. |
| `harness/<name>/` | The files Rayline generates for each harness in local mode: `pi`, `hermes`, `omp`, `opencode` and `openclaw`. Cloud mode uses `~/.rayline/harness/<name>/` instead. |

If a local-mode run fails, check `rayline local status` and
`~/.rayline/local/runtime.log` first.
