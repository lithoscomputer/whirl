# Ideas for the AI steps, measured: 2026-09-28

Each idea was built, run on the local set against the build before it, and
kept only when it helped. Models: `openrouter/gpt-5.6-terra`,
`openrouter/gpt-5.6-luna`, and `gemini/gemini-3.1-flash-lite`. "Calls" and
"tokens" are per task. Costs through OpenRouter vary from run to run, so
calls and tokens are the better measure of spend.

The full-set summaries of this session are `2026-09-28-local-baseline.md`
(before any change), `2026-09-28-local-settle.md` (idea 1),
`2026-09-28-local-effort.md` (idea 2, discarded), and
`2026-09-28-local-final.md` (the kept ideas together).

## All together

The kept ideas and the two fixes below, against the baseline, on the full
local set with 3 runs per task:

| model | pass | calls | in tok | out tok | p50 step |
| --- | --- | --- | --- | --- | --- |
| `openrouter/gpt-5.6-terra` | 0.940 -> 0.996 | 1.53 -> 1.30 | 3158 -> 2844 | 99 -> 62 | 1.50 s -> 2.19 s |
| `openrouter/gpt-5.6-luna` | 0.937 -> 0.992 | 1.61 -> 1.29 | 3242 -> 2831 | 109 -> 76 | 1.74 s -> 2.37 s |
| `gemini/gemini-3.1-flash-lite` | 0.929 -> 0.977 | 1.31 -> 1.23 | 3218 -> 3095 | 71 -> 72 | 1.06 s -> 1.71 s |

Part of the pass rate comes from the two fixes, not from the ideas: the
EXTRACT null fix (`extract-missing-coupon` 0/9 -> 9/9) and the login page
(`goal-login-with-secret` 4/9 -> 9/9). The final set also has three new
tasks (`goal-heal-renamed` and the two `judge-several-claims` tasks). The
p50 step time grew by the settle wait.

## Kept

1. **Wait for the page to settle before a model reads it.** Full set, 3
   runs per task: pass rate 0.929-0.940 -> 0.960-0.964. `slow-products`,
   `extract-slow-products`, and `goal-slow-search` went from 0/9, 0/9, and
   5/9 to 9/9; no other task changed beyond noise. A model step's p50 time
   grew by about 0.5 s.
5. **Find a renamed element again when a cached GOAL path misses.**
   `goal-heal-renamed`, 10 runs: 10/10 both ways; calls 4 -> 2, input tokens
   about -63%, p50 time about -50%. `goal-heal-checkout`, whose cached path
   is incomplete on purpose, costs one more call and still passes 10/10.
6. **Let GOAL fill in a form in one answer.** GOAL tasks, 6 runs: Gemini
   calls 4.5 -> 3.2 and p50 time 6.7 s -> 4.7 s at 1.00; luna pass
   0.75 -> 0.83 with 10% fewer calls; terra flat within noise.
8. **Judge consecutive JUDGE claims in one call.** Four claims, 10 runs:
   calls 4 -> 1, input tokens 6.4k -> 1.7k, p50 time -60 to -70%, 10/10 both
   ways; a batch with a false claim still fails on it 10/10. Batched Gemini
   answered `no` where alone it answered `unsure` for a claim whose evidence
   was out of view.

## Discarded

2. **Least reasoning effort for ACT, `ai:`, and EXTRACT.** These models
   already reason little on these calls. Gemini's lowest level is `low`,
   which tripled its output tokens (62 -> 213) and cost about 27% more;
   luna's ACT pass rate fell 1.000 -> 0.972; terra did not change.
3. **Smaller snapshots.** Playwright's AI snapshot already collapses
   single-child wrappers (13 lines, 0.4% of the saved Amazon page). Safe
   pruning of repeated text saves 1-3% on large pages. No dropdown failure
   came from mixing up native and custom dropdowns, so a `select` label
   had nothing to fix.
4a. **ACT step two reads only the lines step one added.** Dropdown tasks,
   10 runs: few tokens saved on these pages, and Gemini's
   `hidden-input-dropdown` passes fell 29/30 -> 24/30.
4b. **Later GOAL calls read only the lines that changed.** GOAL tasks, 6
   runs: pass rate fell on every model (Gemini 1.00 -> 0.80, luna
   0.60 -> 0.40); `goal-checkout` fell 17/18 -> 3/18; the models took more
   steps, so tokens rose.
7. **The model's earlier reasons in GOAL's step history.** GOAL tasks, 6
   runs: the same pass rates, calls flat except luna (-12%), tokens +3%.
   The "try another way" hint was already in the prompt.

## Found on the way

- `EXTRACT` with a non-object schema root, such as `{ "type": "string" }`,
  could not answer null, so a missing value came back as `"null"`, `""`, or
  `0`. `extract-missing-coupon` failed 0/9 on every model; fixed.
- The eval login page's button did nothing, so `goal-login-with-secret`
  asked a careful model to sign in without a way to succeed. The page now
  says who signed in. The GPT models' GOAL pass rate rose from about 0.75
  to 0.95-0.97 with that fix.
- `judge-several-claims` first asked about a chart below the fold. It now
  asks only about what the viewport shows.
