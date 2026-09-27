# ACT evals: local set, 2026-09-27, scroll, middle-click, and pointer-drag tasks

- Whirl: whirl 0.12.0, commit `08c4fdc`
- Target runs per task (`-n`): 3
- Tasks: 5

Pass rate counts only good runs, with its standard error. Drift (a live
precheck failed) and errors (exit 3: network, credentials, shim) are
left out. Times are the task's ACT steps: snapshots, model calls, and actions.
Costs use catalog prices and show n/a when any run was unpriced.
A `jev:` model runs with `--jev`: Jev plans first, and the named model
plans when Jev is unsure. "by Jev" is the share of actions Jev chose. Calls
and tokens count the model only; costs include Jev's requests.

## Leaderboard

| model | pass | good runs | drift | errors | p50 ACT | p90 ACT | calls | by Jev | in tok | out tok | $/task | $ total |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `openrouter/gpt-5.6-terra` | 1.00 ± 0.00 | 15 | 0 | 0 | 2.0s | 3.3s | 1.00 | - | 1.6k | 63 | $0.0023 | $0.0350 |
| `openrouter/gpt-5.6-luna` | 1.00 ± 0.00 | 15 | 0 | 0 | 2.1s | 2.9s | 1.00 | - | 1.6k | 86 | $0.0003 | $0.0039 |
| `gemini/gemini-3.1-flash-lite` | 1.00 ± 0.00 | 15 | 0 | 0 | 1.0s | 1.7s | 1.00 | - | 1.6k | 61 | $0.0005 | $0.0073 |
| `jev:gemini/gemini-3.1-flash-lite` | 1.00 ± 0.00 | 15 | 0 | 0 | 0.5s | 1.9s | 0.20 | 0.80 | 289 | 13 | $0.0002 | $0.0035 |
| `jev:openrouter/gpt-5.6-luna` | 1.00 ± 0.00 | 15 | 0 | 0 | 0.5s | 3.5s | 0.20 | 0.80 | 304 | 27 | $0.0002 | $0.0027 |

## Per task

Passes over good runs.

| task | `openrouter/gpt-5.6-terra` | `openrouter/gpt-5.6-luna` | `gemini/gemini-3.1-flash-lite` | `jev:gemini/gemini-3.1-flash-lite` | `jev:openrouter/gpt-5.6-luna` |
| --- | --- | --- | --- | --- | --- |
| middle-click-tab | 3/3 | 3/3 | 3/3 | 3/3 | 3/3 |
| pointer-drag-delay | 3/3 | 3/3 | 3/3 | 3/3 | 3/3 |
| pointer-drag-distance | 3/3 | 3/3 | 3/3 | 3/3 | 3/3 |
| scroll-into-view | 3/3 | 3/3 | 3/3 | 3/3 | 3/3 |
| scroll-sideways | 3/3 | 3/3 | 3/3 | 3/3 | 3/3 |

## Failures

Error codes of failed runs, per model.

None.
