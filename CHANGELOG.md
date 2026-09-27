# Changelog

## Unreleased

- With `--jev`, try every named element when no control fits, ask the full list in parallel parts when Jev rejects the shortened one, and confirm an exact quoted name with one small request, as Stagehand does.
- With `--jev`, let Jev say which quoted string or placeholder is the text to type, and read unquoted text with a small model call that sees only the instruction and must copy its words, as Stagehand does.
- With `--jev`, ask Jev Stagehand's fuller intent question: richer action kinds, a merged vote when a click competes with a select, a double-click, or a key press, and the key, mouse button, checkbox end state, and suggestion step in the same request.
- Add `--jev`, which plans `ACT` with TypeSafe's Jev first and falls back to the `model` option when Jev is unsure, cannot supply an argument, or fails. `TYPESAFE_API_KEY` holds Jev's key, and `WHIRL_JEV_ENDPOINT` sends its requests to another server. Whirl asks Jev through `lithos-llm`, which prices its requests. JSON reports record the planner of each action and Jev's requests, tokens, and cost, which the step's cost includes.
- When the text `ACT` types matches a string the instruction quotes, type the instruction's characters, so a model that changes the case or spacing of `"AbC 123"` still types `AbC 123`.
- After `ACT` fills a field, read the value back. When the field does not hold it, as when a `maxlength` cuts it short, the step fails with `act-fill-mismatch` instead of passing silently. Case, spaces, and punctuation do not count, so a field that formats its value still passes.
- When `ACT` clicks a native checkbox or radio input, focus it and press Space, as `CHECK` does, so a styled control that covers the input does not make the click wait out the step.
- Add `ACT locator "instruction"`, which shows the model only one element and what it contains, for long pages.
- Leave each link's URL and the cursor hints out of the snapshot `ACT` sends, about a third of a link-heavy page.
- When `ACT` clicks, double-clicks, or hovers an element that wraps a narrower one, such as a custom dropdown's trigger, point at the element that shows its text instead of the wrapper's center.
- In a file that uses `ACT`, open every shadow root that page scripts attach, so `ACT` sees and acts inside closed shadow roots, as Stagehand does.
- When a page replaces the element `ACT` chose while the model answers, as a re-render after load does, `ACT` takes a new snapshot and asks once more instead of waiting out the step timeout. A second replacement fails the step at once with `stale-ref`.
- Replace the check operators with Hurl's vocabulary. A check is `subject { filter } [not] predicate`, with predicates such as `startsWith`, `isInteger`, and `exists`, and filters such as `count`, `regex`, `toInt`, `split`, `urlQueryParam`, and `base64Decode`. Captures take the same filters, and `regex` is now one of them.
- Replace JSON Pointer with JSONPath: `json:/items/0/id` becomes `json:$.items[0].id`, and a query such as `json:$.items[*].sku` gives a list.
- Compare JSON values and filter results by type: `json:$.id == 42` needs the number 42, and `json:$.id == "42"` the string. Page text still compares as text, a value that starts with `[` or `{` is a JSON literal, and a bare `hex,…;` or `base64,…;` is bytes. `startsWith`, `endsWith`, and `contains` on a string compare with the expected value's text, so `startsWith 12` works on `"123"`. `whirl check` reports a literal whose type cannot match, such as `status == "200"`.
- Count `nth:` from 0, and from the end with a negative index: `nth:-1` is the last match.
- `contains` on a JSON list checks for an item instead of a substring of the list's JSON text.
- Add the `body`, `bytes`, and `location` response fields. A `RESPONSE` body that Chromium or WebKit hands back already decoded is re-encoded with its charset, so `body` and `bytes` match what the server sent; WebKit can still lose bytes it cannot decode. Add `eval "script"` as a check subject that retries until it passes.
- Evaluate every filter and predicate in Rust. JSON numbers keep their exact text, so 64-bit IDs keep their precision in checks and captures.
- Report check failures as `type-mismatch`, `filter-error`, `missing-value`, or `read` as well as `assert`. `whirl check` reports a check whose types cannot work as `filter-type`.
- Add the `xpath:EXPR` response field and filter. XPath 1.0 runs through a pinned libxml2 that release binaries link statically. A response whose `Content-Type` is XML parses as XML, with its root namespaces available and the default namespace as `_`; anything else parses as HTML.
- Run regexes in Unicode mode everywhere, so a pattern such as `/a\-b/` is a parse error.
- Page captures retry like page checks. An absent attribute on an element passes `!=` and every `not` predicate.
- Move the browser shim to protocol 2; `whirl install` provisions the matching bundle.
- Keep variable types. Captures keep the type of their value, `--var`, `--variables-file`, and `{{env.NAME}}` values are typed as Hurl types `--variable`, and a bare `{{name}}` that is a whole expected value compares with its type. In an HTTP JSON body or a JSON literal, a bare `{{name}}` inserts the variable as JSON.
- Write JSON report version 2, where each capture is `{type, value}` and numbers keep their exact text. `whirl report` and `--rerun-failed` read versions 1 and 2, and the HTML report shows each capture's type.
- Add `ACT "instruction"`, which asks a language model to choose one element action from a Playwright AI snapshot of the page and runs it as the matching Whirl action, following Stagehand's `act()`. The new `model` option selects the model through `lithos-llm`. `WHIRL_LLM_ENDPOINT` and `WHIRL_LLM_API_KEY` send calls to one OpenAI-compatible server instead. `{{env.NAME}}` values reach the model only as placeholders, and JSON reports record the actions ACT ran and the tokens it used.
- `whirl check` reports `ACT` without a `model` option and a model the catalog cannot route.
- Raise the minimum supported Rust version to 1.88, which `lithos-llm` requires.

## 0.12.0 (2026-09-08)

- Record the installing Whirl version in the shim bundle. An upgraded binary refuses a stale bundle with a runtime error naming `whirl install`, and `whirl doctor` reports the same, instead of running an older shim silently.
- Replace named inline `HTTP` actions with structured HTTP entries. Requests can run before `VISIT`, use multiline JSON or fenced text bodies, and assert or capture their response without a name. Whirl warns when an HTTP entry has no status assertion.

## 0.11.0 (2026-09-08)

- Record Chromium video at 60 frames per second by default through a screencast recorder, with `--video-fps` (1 to 60) to choose the rate. Firefox and WebKit keep Playwright's 25 fps recorder and warn when a rate is requested. Reports record the recorded rate as `runtime.videoFps`, and `whirl doctor` checks for ffmpeg.

## 0.10.0 (2026-09-04)

- Combine saved reports by recorded attempt timestamps. Add expected-scenario coverage, Not run states, separate setup history, and per-source provenance.
- Add plain-text author details and support saving report metadata in JSON without generating HTML.

## 0.9.0 (2026-09-04)

- Add `whirl report report.json --html evidence.html` to regenerate HTML from saved results, with editable author metadata and support for relocated artifacts.
- Record UTC run and flow timestamps, SHA-256 hashes of parsed flow sources, setup and requested roles, and recording settings. Preserve original execution details when rendering saved reports.

## 0.8.0 (2026-09-04)

- Add portable HTML reports with embedded recordings and screenshots, execution checkpoints, failure details, and optional author-written report metadata.

## 0.7.0 (2026-09-04)

- Add `HTTP` requests with explicit headers and bodies, isolated from browser cookies, using existing response assertions and captures.

## 0.6.0 (2026-09-04)

- Add `chrome`, `firefox`, and `safari` user-agent aliases to flow options and the CLI, and record the resolved string in JSON reports.

## 0.5.0 (2026-09-04)

- Add `RESPONSE` to observe requests made by the browser, with status, header, JSON Pointer assertions, and response captures.

- Add named popups with `POPUP`, explicit tab selection with `TAB`, `CLOSE`, and `tab:name closed` assertions.

- Add `frame:` locator segments for cross-origin and nested iframes in actions, assertions, and captures.

- Allow selected browser installation and add `whirl doctor` with repair commands.

- Add JSON check diagnostics, stable error codes, runtime report metadata,
  and `--rerun-failed` for complete failed flows and their setup.

- Add a runnable sample shop and a CI task with reports and failure artifacts.

- Keep actionability details in failures and add `whirl show-trace`.

- Reject input paths that select no flow files.
- Preserve presence assertions before checks that accept an absent element.

- Bound shim lifecycle operations and request writes so an unresponsive shim
  cannot stall a run indefinitely.
- Move filesystem preparation off async workers and improve worker shutdown.
- Accept a popup closing during a click only when its target received the
  click; a closure before delivery still fails.
- Add opt-in diagnostic logging through `WHIRL_LOG`, with sensitive values
  excluded from log fields.

## 0.4.0 (2026-09-04)

- Add the `reduced-motion` option, which sets what the page's
  `prefers-reduced-motion` media query reports, so pages that honor it skip
  animations and background video.
- `SCREENSHOT` and `SNAPSHOT` names may contain hyphens, so
  `SCREENSHOT after-verification-code` writes `after-verification-code.png`.
- Lint warns about a `count >= 1` assert directly followed by a check on the
  same locator; the second check already waits for the element.

## 0.3.0 (2026-09-04)

- Add the `setup` option: a flow that runs first, once per invocation, whose
  saved state every file naming it starts from, and whose captures those
  files read as `{{setup.name}}`. A failed setup flow fails its dependents
  with a `[setup]` case without running them. `whirl check` validates setup
  references against the setup flow's captures.

## 0.2.0 (2026-09-03)

- Add the `user-agent` option and `--user-agent` flag, which set the browser's
  user agent string for the flow.
- `STORE` gains the `session` and `cookie` scopes: `sessionStorage` entries and
  cookies for the current page's host.
- `CHECK` and `UNCHECK` toggle native checkbox and radio inputs with the
  keyboard, so switches that hide their input behind a styled track (Chakra,
  Radix, Headless UI) no longer time out, and they click `role="switch"`
  controls. Both are idempotent and verify the resulting state.
- Add `TYPE locator "text"`, which sends one key event per character for
  inputs that ignore a plain `FILL`, such as segmented one-time-code fields.
- Add `STORE local "key" "value"`, which writes one `localStorage` entry on
  the current origin so a flow can skip onboarding screens and dismissed
  banners without `EVAL`.
- `VISIT` now completes at the new document's `DOMContentLoaded` instead of
  `load`, so a slow image, font, or video no longer fails a navigation that
  the flow's own asserts would have waited out.

## 0.1.0 (2026-08-28)

Initial public release.

- The complete Whirl V1 language per [SPEC.md](SPEC.md): actions, PAGE
  checks, retried asserts, captures, variables with env masking, options,
  and per-line timeout overrides.
- `whirl` runs flows in parallel against Chromium, Firefox, or WebKit via
  a bundled Playwright shim; `whirl check` parses and lints; `whirl fmt`
  rewrites files to the canonical form; `whirl install` provisions the
  pinned Node runtime, shim, and browsers.
- Console output plus JUnit XML and JSON reports; screenshots, visual
  snapshots, traces, video, and HAR artifacts.
- Platforms: macOS (Apple silicon), Linux x86_64, Linux arm64.
