# Acceptance testing

`scripts/acceptance.sh` runs real turns through every harness, for each config profile and four cases. A profile is a config file the script passes as `rayline run <harness> --config <file>`, which runs in the file's `mode`. A change is done when every cell passes for every profile.

Cloud turns run on Rayline Platform and are billed to your account (some tens of cents per full run).

## Prerequisites

1. Rayline is installed (`rayline --version` works) and signed in (`rayline auth login`).
2. The harnesses you want to test are on `PATH`: `claude`, `codex`, `opencode`, `pi`, `omp`, `hermes`, `openclaw`. A harness that isn't installed fails; to test fewer, name them on the command line.
3. `bash`, `perl` and `python3` (all ship with macOS and most Linux distributions). `python3` runs the MCP tool server of the `heavy` case and makes session ids.
4. For the `local-router-local` profile, a machine that runs local ARC decisions (`docs/local-mode.md`).

### Windows

Run the script in Git Bash, which provides `bash` and `perl`, and `cygpath` to give the harnesses a Windows path to the MCP tool server.

The harnesses start the MCP tool server as `python3` themselves, not through `bash`, so `python3` on `PATH` must start a real Python:

- The `python3` that Windows ships is an App Execution Alias for the Microsoft Store: with arguments it prints "Python was not found" and exits non-zero. Turn it off in Settings > Apps > Advanced app settings > App execution aliases, or put a real `python3.exe` before it on `PATH`.
- The python.org installer provides `python.exe` but no `python3.exe`, and so does a virtual environment made from it. To get one, create a virtual environment with `python -m venv <dir>`, copy `<dir>\Scripts\python.exe` to `<dir>\Scripts\python3.exe`, and put `<dir>\Scripts` first on `PATH`.

`python3 -I --version` in Git Bash prints the Python version when `python3` is set up.

## Profiles

The default profiles are committed in `scripts/acceptance/profiles/`:

| Profile | `mode` | `local.routing` |
|---|---|---|
| `cloud` | `cloud` | none (the default file) |
| `local-cloud-arc-swift` | `local` | `router` `cloud`, `decisions_model` `arc-swift-1.0` |
| `local-cloud-arc-sage` | `local` | `router` `cloud`, `decisions_model` `arc-sage-1.0` |
| `local-router-local` | `local` | `router` `local`, `decisions_model` `arc-swift-1.0` |

The local profiles list no inference servers, so every model ARC picks runs on Rayline Platform. To cover your own servers, copy a profile, add your `local.inference.endpoints` and `local.inference.models` (`docs/spec.md` section 5), and pass it in `PROFILES`. A profile's name is its file name without `.json`. The script never writes a profile or `~/.config/rayline/settings.json`.

## Run

```bash
scripts/acceptance.sh                                  # every harness, profile and case
scripts/acceptance.sh codex pi                         # only these harnesses
PROFILES=scripts/acceptance/profiles/cloud.json scripts/acceptance.sh
PROFILES=$HOME/acceptance/gpu-box.json CASES="tool multi" scripts/acceptance.sh
RAYLINE=target/release/rayline scripts/acceptance.sh   # test a local build
WORK=./acceptance-out scripts/acceptance.sh            # keep every turn's output here
```

| Variable | Default | Meaning |
|---|---|---|
| `PROFILES` | the four files in `scripts/acceptance/profiles/` | Config files to run, separated by spaces. Two with the same name are a usage error. |
| `CASES` | `plain tool heavy multi` | Cases to run (below). |
| `TIMEOUT` | `600` | Seconds per turn. |
| `WORK` | a fresh temporary directory | Where each cell's working directory, output and `results.tsv` go. |
| `RAYLINE` | `rayline` | The CLI to test. |

The script:

1. Prints the Rayline version, each harness's version, and each profile with its mode and file. Keep them with the results: a harness release can change how it talks to Rayline, so a result only holds for the versions it ran with.
2. Runs the profiles in the order given. For a cloud profile it runs every cell: for each harness, each case.
3. For a local profile it starts the local process with `rayline local start --config <file>`, runs every cell, then stops it with `rayline local stop`. It also runs `rayline local stop` if it exits early, including on Ctrl-C. A failed `rayline local start` or `rayline local stop` is a `FAIL`, and so is a `rayline local status` that fails or doesn't report the process running. When `rayline local start` fails, the script stops nothing and goes on with the next profile. Before any turn runs, it checks `rayline local status` when any profile is local: if a local process is already running, it prints `a local process is already running; stop it first` and exits 1, and if `rayline local status` fails, it prints its output and `rayline local status failed` and exits 1.

## Cases

Every cell has its own directory under `WORK`; the harness runs in its `work/` subdirectory, and the turn logs stay in the cell directory. Every turn is the harness's own non-interactive mode with the arguments in the table below. The script passes no model or reasoning flags and sets no harness environment variables: the harness runs on the models and settings Rayline's config gives it.

| Case | What the model must do | Catches |
|---|---|---|
| `plain` | Answer `ACCEPT` followed by 17 + 25. | The request path as such. |
| `tool` | Read `answer.txt` (two random integers) with a tool and answer `ACCEPT` followed by their sum. | A reasoning stream that aborts when a tool call follows it. |
| `heavy` | Call `lookup_record` from an MCP server (`scripts/acceptance/mcp_tools.py`) whose tool schemas carry top-level `oneOf`, `anyOf` and `allOf` with `$ref` branches, as the Codex Computer Use plugin's do, and answer `ACCEPT` followed by the value it returns. | A router or provider that refuses such schemas. |
| `multi` | Turn 1: read `answer.txt` and remember the two integers. Turn 2, in the same session and with the file removed: answer `ACCEPT` followed by their sum. | Reasoning or thinking that is not carried into the next turn. A harness that loses the session cannot recover the integers. |

The answer never appears in the prompt: the sum and the MCP value change every run, so an error that echoes the prompt can't pass.

| Harness | Turn | Session (`multi`) | MCP server (`heavy`) |
|---|---|---|---|
| claude | `-p "<prompt>"` | `--session-id <uuid>`, then `--resume <uuid>` | skipped: Claude Code drops tools with top-level `oneOf`/`anyOf`/`allOf` before any request |
| codex | `exec --skip-git-repo-check "<prompt>"` | `session id:` from turn 1, then `exec resume <id> ...` | `-c mcp_servers.acceptance.command/args` |
| opencode | `run "<prompt>"` | `-s ses_<id>` on both turns | skipped: OpenCode wraps MCP tools in its Code Mode `execute` tool, so their schemas never leave the harness |
| pi | `-p "<prompt>"` | `--session-id <id>` on both turns | `.pi/mcp.json` in the working directory with `"exposure": "direct"` (the default, codemode, keeps the schemas inside Pi), `--approve` |
| omp | `-p "<prompt>"` | `-c` on turn 2 | skipped: persistent config only |
| hermes | `chat -q "<prompt>"` | `Session:` from turn 1, then `--resume <id>` | skipped: persistent config only |
| openclaw | `agent -m "<prompt>"` | `--session-id <id>` on every turn of every case: without it OpenClaw continues its one persistent agent session | skipped: persistent config only |

ARC picks the model of every turn; the provider models are ARC's choice and are not pinned. Each local cell line ends with `(executed <models>)`, the `executed_model` values the local process logged in `~/.rayline/local/runtime.log` during the turn.

Local mode requires a fresh harness session: `rayline run` refuses the session flags of `claude`, `codex`, `opencode` and `pi` before the harness starts, so their `multi` cells are `SKIP` in local mode, with the CLI's message as the reason. The `tool` case still covers reasoning carried into a later request in local mode: the request after the tool result holds the first request's reasoning. With `local-router-local`, `rayline run` refuses `codex`, `hermes` and `openclaw` before the harness starts, since local ARC decisions read only Anthropic Messages turns, so their cells are `SKIP` with the CLI's message.

## Results

Each cell prints one line:

- `PASS <harness> <profile> <case>`: every turn exited 0 within `TIMEOUT` and the output contains the expected answer. A `NOTE` line follows when the output still mentions an error or a 4xx status.
- `FAIL ... : <cause>`: a turn exited non-zero, timed out, or never printed the answer. The cause is the last error line of the output when the turn exited non-zero, and the last 15 lines follow.
- `FAIL ... : not installed`: the harness isn't on `PATH`.
- `SKIP ... : <reason>`: the harness cannot run the cell (see the table above), or `rayline run` refused it by design. Skips don't fail the run.

`WORK/results.tsv` has one row per cell (`harness profile case result executed cause`), and each cell's directory, `<harness>-<profile>-<case>`, holds its turn output as `turn.log`, or `turn-1.log` and `turn-2.log`, next to the harness's `work/` directory. Router keys are redacted from the output. `RAYLINE`, the `PROFILES` files and `WORK` may be relative to the directory you run the script from.

The script exits 0 when nothing failed, 1 on any `FAIL`, when a local process is already running or when `rayline local status` fails before any turn, and 2 on a usage or config error.
