# ACT evals: local set, 2026-09-27, Jev with iframe names

- Whirl: whirl 0.12.0, commit `d9afb8a`
- Target runs per task (`-n`): 3
- Tasks: 54

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
| `jev:gemini/gemini-3.1-flash-lite` | 1.00 ± 0.00 | 162 | 0 | 0 | 0.5s | 1.2s | 0.26 | 0.84 | 1.9k | 11 | $0.0004 | $0.0615 |
| `jev:openrouter/gpt-5.6-luna` | 1.00 ± 0.00 | 162 | 0 | 0 | 0.5s | 1.8s | 0.25 | 0.85 | 1.6k | 14 | $0.0003 | $0.0497 |

## Per task

Passes over good runs.

| task | `jev:gemini/gemini-3.1-flash-lite` | `jev:openrouter/gpt-5.6-luna` |
| --- | --- | --- |
| cheapest-red-shirt | 3/3 | 3/3 |
| closed-shadow-dom | 3/3 | 3/3 |
| closed-shadow-in-cross-site-iframe | 3/3 | 3/3 |
| closed-shadow-in-same-site-iframe | 3/3 | 3/3 |
| cross-site-iframe-in-closed-shadow | 3/3 | 3/3 |
| cross-site-iframe-in-open-shadow | 3/3 | 3/3 |
| custom-dropdown | 3/3 | 3/3 |
| delete-account-among-80 | 3/3 | 3/3 |
| delete-invoice-row | 3/3 | 3/3 |
| end-membership | 3/3 | 3/3 |
| faq-by-meaning | 3/3 | 3/3 |
| hidden-input-dropdown | 3/3 | 3/3 |
| iframe-form | 3/3 | 3/3 |
| input-behind-dropdown | 3/3 | 3/3 |
| kanban-move | 3/3 | 3/3 |
| large-directory-scoped | 3/3 | 3/3 |
| large-directory | 3/3 | 3/3 |
| login-with-secret | 3/3 | 3/3 |
| long-country-list | 3/3 | 3/3 |
| namespaced-markup | 3/3 | 3/3 |
| native-select | 3/3 | 3/3 |
| nested-iframes | 3/3 | 3/3 |
| new-tab | 3/3 | 3/3 |
| newsletter-signup | 3/3 | 3/3 |
| no-js-click | 3/3 | 3/3 |
| on-sale-product | 3/3 | 3/3 |
| open-shadow-in-cross-site-iframe | 3/3 | 3/3 |
| open-shadow-in-same-site-iframe | 3/3 | 3/3 |
| plan-by-seats | 3/3 | 3/3 |
| press-key | 3/3 | 3/3 |
| range-slider | 3/3 | 3/3 |
| reply-to-author | 3/3 | 3/3 |
| right-click-menu | 3/3 | 3/3 |
| same-site-iframe-in-closed-shadow | 3/3 | 3/3 |
| same-site-iframe-in-open-shadow | 3/3 | 3/3 |
| saved-amazon-add-to-cart | 3/3 | 3/3 |
| saved-checkboxes | 3/3 | 3/3 |
| saved-google-flights | 3/3 | 3/3 |
| saved-google-search | 3/3 | 3/3 |
| saved-ionwave | 3/3 | 3/3 |
| saved-os-dropdown | 3/3 | 3/3 |
| saved-radio-button | 3/3 | 3/3 |
| scroll-back-up | 3/3 | 3/3 |
| scroll-halfway | 3/3 | 3/3 |
| scroll-in-frame | 3/3 | 3/3 |
| scroll-panel-chunk | 3/3 | 3/3 |
| settings-link | 3/3 | 3/3 |
| shadow-dom | 3/3 | 3/3 |
| similar-buttons | 3/3 | 3/3 |
| synonym-toolbar | 3/3 | 3/3 |
| tab-by-meaning | 3/3 | 3/3 |
| two-month-calendar | 3/3 | 3/3 |
| unlabeled-grid-input | 3/3 | 3/3 |
| work-vs-personal-email | 3/3 | 3/3 |

## Failures

Error codes of failed runs, per model.

None.
