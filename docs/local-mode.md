# Local mode

In local mode a coding harness talks to a Rayline process on your machine
instead of to Rayline Platform. For every request, ARC picks the model, and the
local process sends the request to one of your own inference servers listed in
`settings.json`, or to Rayline Platform for any other model.

Local mode controls where inference runs; `routing.router` controls
where routing decisions are made (default `hosted`: the request is sent to
the Rayline Routes API).

Cloud mode, the default, needs none of this. See
[Getting started](getting-started.md).

## What you need

- Rayline, installed and signed in with `rayline auth login`.
- A config with a `routing` section: `~/.config/rayline/settings.json`, or a
  file you pass with `--config`.
- On macOS, macOS 13 or later.

Your own inference servers are optional. With none listed, every model ARC
picks goes to Rayline Platform. Rayline doesn't start, download or stop your
servers, and `rayline local stop` leaves them running.

ARC makes every routing decision. See
[ARC pricing](https://docs.rayline.ai/arc/pricing) for how sessions are billed.

## Configure

The config has three sections. A local-mode config needs `routing`. Add
`endpoints` and `models` for each model your own servers serve.

```json
{
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
  },
  "routing": {
    "decisions_model": "arc-swift-1.0",
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
```

Rayline Platform is built in and has no entry. It serves every protocol a harness
speaks.

### `endpoints`

These are your own inference servers, keyed by an id you choose.

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

### `models`

These are the models ARC can pick that Rayline Platform doesn't serve. Each is
keyed by the model id ARC returns.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `endpoint` | string | yes | An id in `endpoints`. |
| `model` | string | yes | The name the server knows the model by. |
| `context_window` | integer > 0 | yes, when the local process starts | The context window in tokens. |
| `max_output_tokens` | integer > 0 | yes, when the local process starts | The output ceiling in tokens. |
| `tokenizer` | string | no | A Hugging Face tokenizer repo, used to count tokens. |
| `price` | object with `input`, `output`, `cache_read`, `cache_write` (numbers >= 0) | no | Replaces ARC's price for this model only. |

When ARC picks a model listed here, the local process sends the request to
that model's endpoint under its `model` name. ARC doesn't publish the models
it knows yet, so every entry needs `context_window` and `max_output_tokens`,
and the local process refuses to start without them.

The only other models ARC can pick are three Rayline Platform models:
`anthropic/claude-opus-5.5`, `anthropic/claude-sonnet-5.5` and
`anthropic/claude-haiku-5.5`. Each has a 1,000,000-token context window and a
128,000-token output ceiling, and ARC prices them. When ARC picks one, the
request goes to Rayline Platform. A `models` entry keyed by one of these ids
replaces that cloud model.

Every entry works the same way. No setting marks a model as local.

### `routing`

| Key | Type | Required | Meaning |
|---|---|---|---|
| `decisions_model` | string | yes | The ARC product that makes the decisions, for example `arc-swift-1.0`. |
| `classifier_model` | string | to run Claude Code | The model that serves Claude Code's auto-mode safety classifier calls. These calls go to Rayline Platform. |
| `policy.preset` | non-empty string | yes | The id of an ARC preset: `arc-policy.package-default.v1`, `arc-policy.hybrid-default.v1` or `arc-policy.prefer-local.v1`. It is sent to ARC unchanged. |
| `policy.overrides` | object | no | Overrides in ARC's own `routing.policy.overrides` shape, sent to ARC unchanged. ARC validates them. |
| `router` | `local` or `hosted` | no, default `hosted` | `hosted`: Rayline Platform's ARC decides. Set `local` to decide on this machine: ARC runs inside the local process, and no routing decision goes to Rayline Platform. |
| `arc_release` | object with `repo`, `revision`, `manifest_sha256`, and optionally `dir` | no | Overrides the built-in ARC release for `local` decisions with a Hugging Face repo, a full commit id, the sha256 of the release's `manifest.json`, and the repo directory it is in (default the repo root). For a private repo, set `HF_TOKEN` or sign in with `huggingface-cli login`. |

`local` decisions are a preview. They run the ARC encoder,
`rayline-arc-encoder`, which releases ship for macOS arm64, and for Linux
(x86_64 and aarch64) and Windows x86_64 with an NVIDIA GPU. Installing or
updating Rayline doesn't download it: the first `rayline local start` (or
`rayline run ... --local` that starts the local process) with
`routing.router: "local"` downloads it from the same release as `rayline`
into `~/.rayline/arc/<version>/` and checks it against the release's
checksums. After that, `rayline update` fetches the new version's encoder
along with `rayline`. Before it downloads anything, start checks that the
platform has an encoder build and, on Linux and Windows, that the NVIDIA
driver is installed; otherwise it fails and says to use `--router hosted`.
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
  NVIDIA driver 576 or later. No CUDA toolkit or Visual C++ runtime.
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
hashes the whole release again. With `routing.router: "local"`, every
turn routes by a local ARC decision, with no fallback: a turn ARC can't
decide fails. Only Anthropic Messages harnesses (Claude Code, OpenCode, Pi,
omp) are decided locally, so `rayline run` refuses Codex, Hermes and
OpenClaw; turns with images or documents fail; and a `models` entry the
release can't route to is skipped with a warning. Section 5.4 of the
[spec](spec.md) has the details.

The [ARC routing policies](https://docs.rayline.ai/arc/routes/hybrid)
page describes how ARC applies each preset and which overrides it accepts.

### Env overrides

Four `routing` keys can be set from the environment:

| Key | Env var |
|---|---|
| `routing.decisions_model` | `RAYLINE_ROUTING_DECISIONS_MODEL` |
| `routing.classifier_model` | `RAYLINE_ROUTING_CLASSIFIER_MODEL` |
| `routing.policy.preset` | `RAYLINE_ROUTING_POLICY_PRESET` |
| `routing.router` | `RAYLINE_ROUTING_ROUTER` |

- From lowest to highest precedence: built-in defaults, the config file, env
  overrides, then flags.
- An override is validated the same way as the file value.
- An override only changes a value. The config file still has to have a
  `routing` section, because the file decides the mode. The exception is
  `rayline local start --router <local|hosted>` with no config file or no
  `routing` section: the overrides apply to its built-in `routing` defaults, and so do
  they for a `rayline run <harness> --local` without `routing` while that
  local process runs.
- Commands that use no `routing` (`auth`, `top`, `update`, `local status`,
  `local stop`) don't read these env vars.
- `endpoints`, `models`, `arc_release` and `policy.overrides` can't be set
  from the environment. Put them in a file and pass it with `--config`.

## Start and run

```bash
rayline local start
rayline run claude --local
rayline run pi --local -- "Fix the failing test in parser.py"
rayline local status
rayline local stop
```

Pi needs version 0.87.1 or later in local mode (`npm install -g
@earendil-works/pi-coding-agent`). An older Pi doesn't send its session id,
so `rayline run pi --local` refuses it.

| Command | Does |
|---|---|
| `rayline local start [--config <file>] [--router <local\|hosted>]` | Starts the local process from the default config file, or from `<file>`. The config must have a `routing` section, except with `--router`. `--router` overrides `routing.router`. |
| `rayline local status [--json]` | Shows whether the local process is running, its address, and the config file and environment it was started from. `--json` prints it as JSON (see [Local decisions for your own Pi](#local-decisions-for-your-own-pi)). |
| `rayline local stop` | Stops the local process. Your inference servers keep running. |
| `rayline run <HARNESS> --local` | Runs the harness in local mode with `~/.config/rayline/settings.json`. Starts the local process if it isn't running. |
| `rayline run <HARNESS> --config <file>` | Uses `<file>` instead of the default config, never merged with it. The run is in local mode if `<file>` has a `routing` section, and in cloud mode if it doesn't. |

- `--local` and `--config` together are a usage error.
- Only one local process runs per machine.
- Running `rayline local start` while the process is already running is an
  error. The error names the running process and its config file.
- A local-mode run whose config file differs from the running process's file
  is an error. To use another file, run `rayline local stop`, then start again
  with that file.

### Models

The model local mode declares to each harness is `rayline-auto`. A model you
name with the harness's own flag, such as `--model` or `-m`, is a request, and
ARC decides which model runs. OpenClaw works only with the declared model. A
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

`--router` needs no `settings.json`. Without a `routing` section, `local` and
`hosted` both use `arc-swift-1.0` and the `arc-policy.prefer-local.v1` preset.
`routing.classifier_model` is needed only to run Claude Code.

pi-arc finds the local Routes API with `rayline local status --json`:

```json
{
  "running": true,
  "pid": 40049,
  "url": "http://127.0.0.1:57693",
  "config": "/Users/you/.config/rayline/settings.json",
  "environment": "prod",
  "router": "local",
  "decisions_model": "arc-swift-1.0",
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
prints one snapshot and exits. Under its title, `top` names the router that
decides, such as `Router: local ARC (arc-swift-1.0) on this machine` (the
`router` key in `--json`), and each request shows the router that decided it,
how long a local decision took and the event it was decided for.

The live view waits for a local process: while none runs it shows
`Local process: not running` and checks again on every refresh, and when the
local process restarts it follows the new one. `--json`, or output that isn't
a terminal, with no local process running says so and exits with an error.

## Config errors

Rayline checks the config once, when it loads, and refuses to run with an
error that names the key. It refuses:

1. An unknown key, at any level Rayline defines.
2. A wrong type, an empty string, or a value out of range.
3. In a local-mode run, an endpoint that doesn't list the protocol of the
   harness being run. Cloud mode doesn't use endpoints, and
   `rayline local start` runs no harness, so neither checks this.
4. A model whose `endpoint` isn't in `endpoints`.
5. Local mode without `routing`, or a `routing` section that is missing a
   required key. When `settings.json` doesn't exist, the error prints one to
   paste, with the hosted router.
6. A routing env override when the config file has no `routing` section.
7. A `--config <file>` that doesn't exist. Only the default file is optional.
8. A key repeated in one JSON object, an empty map key, a URL that isn't an
   absolute `http` or `https` URL, a protocol listed twice in `protocols`, or a
   `price` that is missing one of its four fields.

Before the local process serves anything, it also refuses:

9. An `api_key_env` that names an env var that isn't set.
10. A model entry that is missing `context_window` or `max_output_tokens`.
11. An `api_key_env` that names `RAYLINE_ROUTER_API_KEY`, which holds the
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
