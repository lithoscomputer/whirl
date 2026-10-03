# ACT evals: local set, 2026-09-28

- Whirl: whirl 0.18.0, commit `a8ee7e7`
- Target runs per task (`-n`): 3
- Tasks: 84

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
| `openrouter/gpt-5.6-terra` | 0.96 ± 0.01 | 252 | 0 | 0 | 2.0s | 4.6s | 1.48 | - | 3.1k | 86 | $0.0024 | $0.6014 |
| `openrouter/gpt-5.6-luna` | 0.96 ± 0.01 | 252 | 0 | 0 | 2.1s | 4.3s | 1.50 | - | 3.1k | 97 | $0.0003 | $0.0640 |
| `gemini/gemini-3.1-flash-lite` | 0.96 ± 0.01 | 252 | 0 | 0 | 1.6s | 2.8s | 1.30 | - | 3.2k | 72 | $0.0009 | $0.2191 |

## Per task

Passes over good runs.

| task | `openrouter/gpt-5.6-terra` | `openrouter/gpt-5.6-luna` | `gemini/gemini-3.1-flash-lite` |
| --- | --- | --- | --- |
| ai-target-absent-banner | 3/3 | 3/3 | 3/3 |
| ai-target-any-add-button | 3/3 | 3/3 | 3/3 |
| ai-target-blue-mug | 3/3 | 3/3 | 3/3 |
| ai-target-invoice-row | 3/3 | 3/3 | 3/3 |
| ai-target-price-check | 3/3 | 3/3 | 3/3 |
| ai-target-red-tee-button | 3/3 | 3/3 | 3/3 |
| ai-target-slow-products | 3/3 | 3/3 | 3/3 |
| cheapest-red-shirt | 3/3 | 3/3 | 3/3 |
| closed-shadow-dom | 3/3 | 3/3 | 3/3 |
| closed-shadow-in-cross-site-iframe | 3/3 | 3/3 | 3/3 |
| closed-shadow-in-same-site-iframe | 3/3 | 3/3 | 3/3 |
| cross-site-iframe-in-closed-shadow | 3/3 | 3/3 | 3/3 |
| cross-site-iframe-in-open-shadow | 3/3 | 3/3 | 3/3 |
| custom-dropdown | 2/3 | 3/3 | 3/3 |
| delete-account-among-80 | 3/3 | 3/3 | 3/3 |
| delete-invoice-row | 3/3 | 3/3 | 3/3 |
| end-membership | 3/3 | 3/3 | 3/3 |
| extract-cheapest-price | 3/3 | 3/3 | 0/3 |
| extract-missing-coupon | 0/3 | 0/3 | 0/3 |
| extract-red-tee-names | 3/3 | 3/3 | 3/3 |
| extract-scoped-comment | 3/3 | 3/3 | 3/3 |
| extract-section-link | 3/3 | 3/3 | 3/3 |
| extract-slow-products | 3/3 | 3/3 | 3/3 |
| faq-by-meaning | 3/3 | 3/3 | 3/3 |
| goal-checkout | 3/3 | 2/3 | 3/3 |
| goal-gift-card | 1/3 | 1/3 | 3/3 |
| goal-heal-checkout | 3/3 | 3/3 | 3/3 |
| goal-login-with-secret | 0/3 | 0/3 | 3/3 |
| goal-slow-search | 3/3 | 3/3 | 3/3 |
| hidden-input-dropdown | 3/3 | 3/3 | 2/3 |
| iframe-form | 3/3 | 3/3 | 3/3 |
| input-behind-dropdown | 3/3 | 3/3 | 3/3 |
| judge-chart-rising | 3/3 | 3/3 | 3/3 |
| judge-error-banner | 3/3 | 3/3 | 3/3 |
| judge-order-total | 3/3 | 3/3 | 3/3 |
| judge-shipping-address | 3/3 | 3/3 | 0/3 |
| judge-status-green | 3/3 | 3/3 | 3/3 |
| judge-wrong-total | 3/3 | 3/3 | 3/3 |
| kanban-move | 3/3 | 3/3 | 3/3 |
| large-directory-scoped | 3/3 | 3/3 | 3/3 |
| large-directory | 3/3 | 3/3 | 3/3 |
| login-with-secret | 3/3 | 3/3 | 3/3 |
| long-country-list | 3/3 | 3/3 | 3/3 |
| middle-click-tab | 3/3 | 3/3 | 3/3 |
| namespaced-markup | 3/3 | 3/3 | 3/3 |
| native-select | 3/3 | 3/3 | 3/3 |
| nested-iframes | 3/3 | 3/3 | 3/3 |
| new-tab | 3/3 | 3/3 | 3/3 |
| newsletter-signup | 3/3 | 3/3 | 3/3 |
| no-js-click | 3/3 | 3/3 | 3/3 |
| on-sale-product | 3/3 | 3/3 | 3/3 |
| open-shadow-in-cross-site-iframe | 3/3 | 3/3 | 3/3 |
| open-shadow-in-same-site-iframe | 3/3 | 3/3 | 3/3 |
| plan-by-seats | 3/3 | 3/3 | 3/3 |
| pointer-drag-delay | 3/3 | 3/3 | 3/3 |
| pointer-drag-distance | 3/3 | 3/3 | 3/3 |
| press-key | 3/3 | 3/3 | 3/3 |
| range-slider | 3/3 | 3/3 | 3/3 |
| reply-to-author | 3/3 | 3/3 | 3/3 |
| right-click-menu | 3/3 | 3/3 | 3/3 |
| same-site-iframe-in-closed-shadow | 3/3 | 3/3 | 3/3 |
| same-site-iframe-in-open-shadow | 3/3 | 3/3 | 3/3 |
| saved-amazon-add-to-cart | 3/3 | 3/3 | 3/3 |
| saved-checkboxes | 3/3 | 3/3 | 3/3 |
| saved-google-flights | 3/3 | 3/3 | 3/3 |
| saved-google-search | 3/3 | 3/3 | 3/3 |
| saved-ionwave | 3/3 | 3/3 | 3/3 |
| saved-os-dropdown | 3/3 | 3/3 | 3/3 |
| saved-radio-button | 3/3 | 3/3 | 3/3 |
| scroll-back-up | 3/3 | 3/3 | 3/3 |
| scroll-halfway | 3/3 | 3/3 | 3/3 |
| scroll-in-frame | 3/3 | 3/3 | 3/3 |
| scroll-into-view | 3/3 | 3/3 | 3/3 |
| scroll-panel-chunk | 3/3 | 3/3 | 3/3 |
| scroll-sideways | 3/3 | 3/3 | 3/3 |
| settings-link | 3/3 | 3/3 | 3/3 |
| shadow-dom | 3/3 | 3/3 | 3/3 |
| similar-buttons | 3/3 | 3/3 | 3/3 |
| slow-products | 3/3 | 3/3 | 3/3 |
| synonym-toolbar | 3/3 | 3/3 | 3/3 |
| tab-by-meaning | 3/3 | 3/3 | 3/3 |
| two-month-calendar | 3/3 | 3/3 | 3/3 |
| unlabeled-grid-input | 3/3 | 3/3 | 3/3 |
| work-vs-personal-email | 3/3 | 3/3 | 3/3 |

## Failures

Error codes of failed runs, per model.

- `openrouter/gpt-5.6-terra`: `act-no-match` 1, `assert` 4, `goal-impossible` 1, `goal-limit` 2, `timeout` 1
- `openrouter/gpt-5.6-luna`: `act-invalid-decision` 3, `assert` 3, `goal-limit` 3
- `gemini/gemini-3.1-flash-lite`: `act-invalid-decision` 1, `assert` 6, `judge-false` 3
