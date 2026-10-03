# ACT evals

These evals compare language models on `ACT` (SPEC section 7.4), `ai:`
targets (SPEC section 6.3), and `EXTRACT` (SPEC section 7.6) for accuracy,
cost, and speed. The `ai-target-*` tasks use `ai:` instead of `ACT`, and the
`extract-*` tasks use `EXTRACT`. Each task is an ordinary `.whirl` flow: `ACT` is
the step under test, and its `ASSERT` lines grade it. Whirl's JSON report records
the rest.

## Run

Set the keys for the providers you compare, such as `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, and `GEMINI_API_KEY`. Then:

```console
$ mise run eval:act                             # local set, models.txt, -n 10
$ mise run eval:act -- -m openai/gpt-5.6-luna -n 3
$ mise run eval:act -- --set live               # live set, -n 1
$ mise run eval:act -- --set all -t custom-dropdown
$ mise run eval:act -- --preview                # the planned runs; spends nothing
$ mise run eval:act -- --summarize              # summarize existing runs only
```

A session calls real models and costs money. Check it first with
`--preview`. A local session is about 590 runs per model.

`-n` is a target: the script runs only the runs each model and task still
need, one pass over the tasks at a time. Repeating a command resumes an
interrupted session, and it does nothing once every target is met.

`models.txt` lists the default models. `-m` replaces the list for one
session.

A model name that starts with `jev:` runs Whirl with `--jev` (SPEC section
7.4): Jev plans first, and the rest of the name is the fallback model. Set
`TYPESAFE_API_KEY` as well. `jev:` runs keep their own results, so one
session can compare both planners:

```console
$ mise run eval:act -- -m gemini/gemini-3.1-flash-lite -m jev:gemini/gemini-3.1-flash-lite -n 3
```

The leaderboard's "by Jev" column is the share of actions Jev chose without
the model. Calls and tokens count the model only; costs include Jev's
requests, which `lithos-llm` prices from its catalog.

## Sets

- **local** (default): flows in `local/flows/` against the pages in
  `local/site/`, which the script serves on `127.0.0.1`. A page reaches a
  second site through `localhost`. These runs are deterministic apart from
  the model. The `saved-*` tasks use saved copies of real sites, in
  `local/site/saved/` (see its README). Other tasks make the model reason:
  compare prices, tell identical buttons apart by their row, or find an
  element that the instruction names by meaning only.
- **live** (opt-in): flows in `live/flows/` against real websites. The sites
  can change or block the browser at any time.

## Scoring

The script classifies each flow in each run:

| Result | Class | In the pass rate |
| --- | --- | --- |
| The flow passed | pass | Yes |
| A live flow's `# precheck` entry failed | drift | No |
| A later entry failed | fail | Yes |
| The flow had a runtime error (exit 3) | error | No |

Drift and errors do not count toward `-n`, so the next session runs
replacements. A pass that adds no good run stops the session, so a broken
site or key does not retry without end.

A task whose file name ends in `.no-match.whirl` expects `ACT` to find no
element: it passes only when the ACT step fails with `act-no-match`. A task
whose file name ends in `.ambiguous.whirl` expects an `ai:` description to
fit several elements: it passes only when the step fails with `strictness`.
Times, calls, tokens, and costs sum the task's `ACT`, `ai:`, and `EXTRACT`
steps.

## Results

Raw runs go to `runs/<set>/<model>/<timestamp>/` and are not committed. Each
session writes `results/<date>-<set>.md`: a leaderboard of pass rate, ACT
time, tokens, and cost per model; a models × tasks grid; failure codes; and,
for the live set, drift. Commit the summaries: a live summary is a record of
that date.

## Add a task

Add a flow to `local/flows/` and, when it needs one, a page to
`local/site/`. Start the flow with the options below, give each `ACT` line
`@60s`, and assert the outcome, not the way the model reached it. A task can
have several `ACT` lines, such as typing and then pressing Enter; its
measurements are their sums.

```whirl
[Options]
base: {{env.EVAL_BASE}}
model: {{model}}

# Choose from a dropdown that is not a select element.
VISIT /custom-dropdown/
ACT "choose Canada from the country dropdown" @60s
ASSERT testid:selected-country text == Canada
```

A live flow uses absolute URLs and starts with an entry whose comment
begins with `# precheck`. That entry checks, without `ACT`, that the page
still has what the task needs.

`mise run test:evals` checks every flow and tests the script without calling
a model.

## Test without a model

`WHIRL_LLM_ENDPOINT` (SPEC section 13) sends model calls to an
OpenAI-compatible server, such as `twin-openai` from `../twins`. Its
unscripted answers name elements that do not exist, so every run fails with
`act-invalid-decision`, which exercises the whole script at no cost.
Delete `runs/` and `results/` afterwards.
