# ACT evals: live set, 2026-09-28

- Whirl: whirl 0.18.0, commit `1e14f9a`
- Target runs per task (`-n`): 3
- Tasks: 9

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
| `openrouter/gpt-5.6-terra` | 1.00 ± 0.00 | 24 | 6 | 0 | 2.1s | 3.0s | 1.12 | - | 19.9k | 47 | $0.0449 | $1.0774 |
| `openrouter/gpt-5.6-luna` | 1.00 ± 0.00 | 24 | 6 | 0 | 1.7s | 3.3s | 1.12 | - | 20.2k | 52 | $0.0036 | $0.0852 |
| `gemini/gemini-3.1-flash-lite` | 1.00 ± 0.00 | 24 | 5 | 0 | 1.8s | 3.4s | 1.12 | - | 25.3k | 68 | $0.0047 | $0.1125 |
| `openrouter/google/gemini-3.5-flash-lite` | 1.00 ± 0.00 | 24 | 5 | 0 | 1.6s | 2.6s | 1.12 | - | 26.2k | 69 | $0.0076 | $0.1817 |

## Per task

Passes over good runs.

| task | `openrouter/gpt-5.6-terra` | `openrouter/gpt-5.6-luna` | `gemini/gemini-3.1-flash-lite` | `openrouter/google/gemini-3.5-flash-lite` |
| --- | --- | --- | --- | --- |
| amazon | 3/3 | 3/3 | 3/3 | 3/3 |
| apartments | - | - | - | - |
| bidnet | 3/3 | 3/3 | 3/3 | 3/3 |
| google-flights | 3/3 | 3/3 | 3/3 | 3/3 |
| google | 3/3 | 3/3 | 3/3 | 3/3 |
| home-depot | 3/3 | 3/3 | 3/3 | 3/3 |
| playwright-docs | 3/3 | 3/3 | 3/3 | 3/3 |
| vantech-journal | 3/3 | 3/3 | 3/3 | 3/3 |
| wikipedia | 3/3 | 3/3 | 3/3 | 3/3 |

## Failures

Error codes of failed runs, per model.

None.

## Drift

Tasks whose precheck failed, as of 2026-09-28.

- amazon (`openrouter/gpt-5.6-luna`)
- amazon (`openrouter/gpt-5.6-terra`)
- apartments (`gemini/gemini-3.1-flash-lite`)
- apartments (`openrouter/google/gemini-3.5-flash-lite`)
- apartments (`openrouter/gpt-5.6-luna`)
- apartments (`openrouter/gpt-5.6-terra`)
