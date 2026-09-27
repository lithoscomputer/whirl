# ACT evals: local set, 2026-09-27, Jev with iframe names

- Whirl: whirl 0.12.0, commit `10b05d2`
- Target runs per task (`-n`): 3
- Tasks: 11

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
| `jev:gemini/gemini-3.1-flash-lite` | 1.00 ± 0.00 | 33 | 0 | 0 | 0.5s | 0.6s | 0.09 | 1.00 | 7 | 1 | $0.0001 | $0.0027 |
| `jev:openrouter/gpt-5.6-luna` | 1.00 ± 0.00 | 33 | 0 | 0 | 0.5s | 0.6s | 0.09 | 1.00 | 15 | 2 | $0.0001 | $0.0028 |

## Per task

Passes over good runs.

| task | `jev:gemini/gemini-3.1-flash-lite` | `jev:openrouter/gpt-5.6-luna` |
| --- | --- | --- |
| closed-shadow-in-cross-site-iframe | 3/3 | 3/3 |
| closed-shadow-in-same-site-iframe | 3/3 | 3/3 |
| cross-site-iframe-in-closed-shadow | 3/3 | 3/3 |
| cross-site-iframe-in-open-shadow | 3/3 | 3/3 |
| iframe-form | 3/3 | 3/3 |
| nested-iframes | 3/3 | 3/3 |
| open-shadow-in-cross-site-iframe | 3/3 | 3/3 |
| open-shadow-in-same-site-iframe | 3/3 | 3/3 |
| same-site-iframe-in-closed-shadow | 3/3 | 3/3 |
| same-site-iframe-in-open-shadow | 3/3 | 3/3 |
| scroll-in-frame | 3/3 | 3/3 |

## Failures

Error codes of failed runs, per model.

None.
