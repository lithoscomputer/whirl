# Changelog

## Unreleased

- When Chromium refuses a screenshot for a moment ("Unable to capture screenshot"), as it can on a busy machine, take it again within the step's time. `SNAPSHOT`, `SCREENSHOT`, and `JUDGE` failed with a runtime error instead.
- Define the complete syntax of a `.whirl` file in SPEC section 17, as a parsing expression grammar that Whirl's tests run against the parser. It replaces the EBNF grammar, which left out tokens, white space, and where each kind of locator ends. Section 17.1 lists the value rules that the grammar leaves out, such as valid regexes and JSONPath queries.
- Accept a JSON array as the body of an `HTTP` or `MOCK` line, as the SPEC allows. A body line that starts with `[` was read as a section header, so every array body was a parse error.
- Report `MOCK` lines that no `VISIT` follows before the file's first `VISIT` as a parse error: a file of only `MOCK` lines, or `MOCK` lines followed by `HTTP`, `PAGE`, or a check line. The first browser entry must start with `VISIT` after any `MOCK` lines, and a check before it had no page to read.
- In a JSON body or JSON literal, read `\\{{name}}` inside a string as an escaped backslash before the reference, as the run already did. `whirl check` did not see the reference, and a JSON literal in a text comparison compared the wrong text.
- Start a comment only at a `#` at the start of a line or after white space. A `#` inside a token is text, so `VISIT /docs#install` keeps its fragment and `css:#submit` works without quotes. The rest of the token was a comment before, which silently dropped a URL's fragment, such as the `#/cart` of `ASSERT url == https://shop.example.com/#/cart`.
- Simplify the syntax so that every line reads left to right, one token at a time. This changes existing flows:
  - Every ARIA role is a locator prefix, and `role:` is gone: `role:button "Sign in"` is now `button:"Sign in"`, `role~:button Sign` is `button:~Sign`, and a role with any name is `dialog:*`. The roles are the ones Playwright accepts, except `generic`, `none`, and `presentation`.
  - Prefixes are strict: in a locator, a bare colon always marks a prefix, and an unknown one is a parse error. Quote unprefixed text that holds a colon, and quote `>>` to match it as text.
  - Browser tabs are windows: `TAB payment` is now `WINDOW payment`, and `ASSERT tab:payment closed` is `ASSERT window:payment closed`. The lint codes are `duplicate-window` and `unknown-window`.
  - A bare value cannot start with `@`: a bare `@` token is always the step timeout and must end its line. Quote a value such as `"@60s"`.
  - A `name: value` line needs a space after the colon, in options, `CAPTURE` lines, headers, and snapshot settings: `base:x` is an error.
  - The `~` of a substring match follows the colon, for roles and text prefixes alike: `text~:Added` is now `text:~Added`.
  - A quoted string joins a token only right after a prefix, as in `label:"First name"` or `button:~"Sign in"`. A `json:` or `xpath:` argument follows this rule like any other prefix value; it no longer has a rule of its own.
  - A first token with a prefix after `ACT`, `EXTRACT`, or `JUDGE` starts the scope, so `ACT css:form` alone is an error. `DRAG` needs no quotes on a `to` that is text, and a bare direction or `to` right after `SCROLL` is always the motion.
  - The removed `[Asserts]` and `[Captures]` sections are unknown sections, and `whirl fmt` no longer rewrites them. The `sections-removed` and `mixed-check-syntax` errors are gone.
- Run files with `whirl run`, as in `whirl run checkout.whirl`. This changes existing scripts:
  - `whirl checkout.whirl` and `whirl --headed checkout.whirl` are usage errors that say to use `whirl run`. `whirl` alone prints help.
  - `--out DIR` names the output directory for screenshots, traces, video, and network logs; the default is still `whirl-artifacts/`. `--artifacts` still works, prints a deprecation warning, and cannot be combined with `--out`.
  - `--load-state FILE` and `--save-state FILE` replace `--storage` and `--save-storage`, which are now usage errors. The state file format does not change. `--load-state` cannot be combined with a file's `storage` or `setup` option; `--storage` silently replaced the file's `storage` option.
- Set any option but `setup` for every file from the command line with `-O key=value`, on `whirl run` and `whirl check`, as in `whirl run -O browser=firefox -O step-timeout=15s flows/`. A value has the syntax of the file's `key: value` line and the same validation, and an invalid key or value is a usage error before any browser starts. For `allow-hosts` and `snapshot-mask`, the `-O` values form a list that replaces the file's list, and an empty value, as in `-O allow-hosts=`, clears it. `--base`, `--browser`, `--step-timeout`, `--entry-timeout`, and `--user-agent` share this validation, and supplying one of them together with `-O` for the same key is a usage error. `whirl check` applies `-O` before its lints, so `-O model=…` satisfies a file that uses `ACT`.

## 0.20.0 (2026-09-28)

- Read a plain number that a model writes as text, such as `"$1,299.00"`, as the number where an `EXTRACT` schema wants a number and does not allow a string, instead of failing with `extract-schema`. Any other text still fails. In the evals, Gemini 3.5 Flash-Lite's EXTRACT pass rate rose from 0.78 to 0.94.
- When a model answers `click` with an argument that names no mouse button, such as an empty string or the element's text, click with the left button instead of failing with `act-invalid-decision`; ignore arguments to methods that take none, such as `hover`. In the evals, this recovered most of GLM 5.3 Flash's `act-invalid-decision` failures (ACT pass rate 0.81 to 0.88) and changed nothing beyond noise for the default models.

## 0.19.0 (2026-09-28)

- Judge consecutive `JUDGE` lines with the same scope and timeout in one model call, on one snapshot and one screenshot. Each line still passes or fails on its own answer. In the evals, four claims took 1 call instead of 4, about a quarter of the input tokens, and a third of the time, with the same results.
- Let `GOAL` fill in the fields of a form in one model answer: the actions run in order, and the first that fails stops the rest. In the evals, a checkout goal took 4 model calls instead of 7 on Gemini 3.1 Flash-Lite, and a two-field sign-in 2 instead of 4.
- When a cached `GOAL` line misses because the page renamed its element, ask the model to find that element again and replay the rest of the cached path, instead of planning the rest of the goal step by step. The model then confirms the goal is done. In the evals, such a heal took 2 model calls instead of 4 and half the time.
- Before `ACT`, `GOAL`, `ai:` targets, `EXTRACT`, and `JUDGE` read the page, wait until no request has been open for 500 ms, for at most 5 seconds. A page that loads its content after the document, such as a product list from a slow API, showed the model no content. In the evals, such tasks went from 0 of 9 passes to 9 of 9, and a model step takes about 0.5 s longer.
- Move the browser shim to protocol 8; `whirl install` provisions the matching bundle.
- Let `EXTRACT` answer null when its schema's root is not an object, such as `{ "type": "string" }`. The model could not say that the page does not show the value, so it answered `"null"`, an empty string, or `0` instead of a missing value.
- Add eval tasks that load their data from a slow request, a `GOAL` heal of a renamed button, and several `JUDGE` claims in a row, and add Gemini 3.5 Flash-Lite to the default eval models.

## 0.18.0 (2026-09-28)

- Add `GOAL "goal"`, which asks the model option's language model to reach a goal with several actions, one at a time, as in `GOAL "add two blue mugs to the cart and open the cart"`. Each call sees the goal, the lines that already ran, and a new snapshot of the page, and answers with one action in the form `ACT` uses, `done`, or `impossible`. An action that fails goes back to the model, which plans again. `impossible` fails with `goal-impossible` and the model's reason, and a goal that is not done after 20 actions fails with `goal-limit`. The default time is 2 minutes, and each action gets at most the step timeout. `GOAL` has no navigation by URL, back, or reload. The AI cache records the lines that ran; later runs replay them, and a line that no longer fits heals from the current page with a `healed` warning. `whirl check` reports a `GOAL` that is not the last action of an entry with an `ASSERT` (`goal-unchecked`). Reports show each action, how the goal ended, and what the model calls cost.
- Add `goal-*` tasks to the evals, including a checkout across two views, a heal from a committed cache, and a goal that must end as `impossible`.

## 0.17.0 (2026-09-28)

- Add `JUDGE [locator] "claim"`, a check line that asks the model option's language model whether a claim about the page holds, as in `JUDGE testid:summary "the total matches the sum of the line items"`. The model sees the AI snapshot and a settled screenshot of the viewport or of the element. `yes` passes, `no` fails with `judge-false` and the model's reason, and `unsure` passes with the warning `judge-unsure`. `JUDGE` runs once and never uses the AI cache. A run whose files use `JUDGE` stops before any flow when the model's provider has no credentials. `whirl check` reports a model that does not accept images (`judge-without-images`), warns when the catalog does not know (`judge-images-unknown`), and warns about an entry with no `ASSERT` before its `JUDGE` (`judge-alone`). Reports show the verdict, the reason, and what the call cost.
- Add `judge-*` tasks to the evals, with claims that only the screenshot shows and tasks that expect `no` or `unsure`.
- Move the browser shim to protocol 7; `whirl install` provisions the matching bundle.

## 0.16.0 (2026-09-28)

- Add `EXTRACT name [locator] "instruction"`, which asks the model option's language model to read a value from the page, with an optional JSON Schema on the lines below. `extract:NAME` reads the value with its type in later checks and captures, and filters apply, as in `ASSERT extract:order json:$.total > 0`. Without a schema the value is a string; a null answer is a missing value. A `"format": "uri"` string is answered with a link's ref, and Whirl reads its absolute `href`. JSON numbers keep their exact text. The schema must use a documented subset (`extract-schema-unsupported`), and Whirl adapts it for providers that need strict schemas. An answer outside the schema fails with `extract-schema`. `whirl check` reports duplicate and unknown names and warns with `extract-unsettled` when `EXTRACT` directly follows an interaction. Reports show the value, the model, and what the call cost.
- Add `extract-*` tasks to the evals: lists, numbers, links, missing values, and a scoped extract.
## 0.15.0 (2026-09-28)

- Add `ai:"description"`, a locator segment that a language model resolves to one element, as in `CLICK ai:"the Add to cart button for the first product"`. It works in actions, checks, captures, `SNAPSHOT` targets, and `ACT` scopes, and it must be the last segment; the segments before it limit what the model sees. The model lists every element that matches: two or more fail with `strictness`, and none asks again every 2 seconds until the step times out, except in a `hidden` or `not exists` check, where none passes. `ai:` needs the `model` option and cannot be counted (`ai-count`). JSON and HTML reports show what each target resolved to and what its model calls cost.
- Add the AI cache: `<flow>.whirl-cache.json`, next to the flow, records what each `ai:` target and `ACT` line resolved to, as strict Whirl locators and lines with a role-and-name fingerprint. A run replays it without a model call. A miss or a stale entry resolves with the model, and the step passes with a `cache-miss` or `healed` warning. `--cache=update` writes the cache of each passing file and removes unused entries, and `--cache=only` fails a miss instead of asking the model. A masked value is stored as its variable reference, such as `{{env.PASSWORD}}`. `whirl check` reports an invalid cache (`cache-invalid`) and entries whose line is gone (`cache-stale-entry`).
- When an action that `ACT` chose fails, as when a banner covers its element, `ACT` takes a new snapshot and plans once more. A line heals at most once, and the chosen action gets at most half of the line's time so the heal can run.
- Add `ai:` tasks to the ACT evals, including ambiguous descriptions that must fail with `strictness`.
- Move the browser shim to protocol 6; `whirl install` provisions the matching bundle.
## 0.14.0 (2026-09-28)

- **Breaking:** reject `[Asserts]` and `[Captures]` sections with the parse error `sections-removed`. `whirl fmt flows/` still rewrites them as `ASSERT` and `CAPTURE` lines.
- Add `MOCK METHOD url STATUS`, which serves a fixed response to matching browser requests until the file ends, and `MOCK METHOD url failed`, which fails them like a dropped connection. Header lines and a JSON or fenced body follow, as for `HTTP`. `*` in the URL matches any run of characters, a later `MOCK` with the same method and URL replaces the earlier one, and the mock registered last wins. `MOCK` lines can come before the first `VISIT`, a mock can serve the page document, and a mocked request is served even when `allow-hosts` blocks its host. Reports list each mock and how many requests it served, and a passing file warns about a mock that served none with `unused-mock`.
- Add the `request:NAME` subject, which checks the request that `RESPONSE NAME` selected: `method`, `url`, `header:NAME`, `body`, `bytes`, `json:PATH`, and `xpath:EXPR`. Request checks read once, like response checks.
- Report step warnings with a stable code in the JSON report's `warnings` on each step.
- Move the browser shim to protocol 5; `whirl install` provisions the matching bundle.
## 0.13.0 (2026-09-28)

- **Breaking:** mark checks and captures with `ASSERT` and `CAPTURE` lines instead of `[Asserts]` and `[Captures]` sections, as in `ASSERT testid:cart-badge text == 1` and `CAPTURE order_id: response:order json:$.id`. The check language does not change. Check lines run in the order written, so a `CAPTURE` can come before an `ASSERT` that reads its value. Sections still work in this release: `whirl check` warns about each one with `sections-deprecated`, and a file that mixes sections and check lines is the parse error `mixed-check-syntax`. The next release rejects sections. Run `whirl fmt flows/` to rewrite them.
- Add `SNAPSHOT name locator`, which compares one element instead of the full page, as in `SNAPSHOT cart testid:cart`. The locator follows the name, and every segment needs a prefix; chains, `nth:`, and frames work. The target must match exactly one element, and Whirl finds it again for every capture, scrolls it into view, and crops the image to it. Masks still search the whole page, and a `snapshot-max-diff` percentage counts the element's pixels. When the target disappears after a mismatch, the step fails with a timeout instead of reporting the old pixels. Reports show the target. Baseline paths do not change, so run `--update-snapshots` after you add or remove a locator.
- Add `snapshot-mask`, `snapshot-max-diff`, and `snapshot-pixel-threshold`. Set them in `[Options]` for every `SNAPSHOT`, or on lines below one `SNAPSHOT` to override them for that snapshot; a local mask list replaces the file list, and `none` clears it. A mask covers matching elements with pink in every capture, including baseline updates. `snapshot-max-diff` allows a pixel count, such as `20`, or a percentage of the image, such as `0.1%`. `snapshot-pixel-threshold` sets how much a pixel's color must change to count. Image sizes must still match. Reports show the settings each snapshot used.
- Read each line of the snapshot that `ACT` takes as Playwright writes it. `ACT` took the first `[ref=…]` on a line as the element's ref, even one in a name or in page text. So for a button named `Delete [ref=e3]`, the report line described the wrong element, and a choice of the button failed with `act-invalid-decision`. Now the ref and marks such as `[active]` come only from the text after the name. A name that starts and ends with `/`, such as `/api/`, keeps its slashes: Playwright writes such a name without quotes, so `ACT` read no name, and with `--jev`, Jev left the element out of its lists. With `--jev`, Jev reads a value with a control character, such as `"a\x7fb"`, without its quotes and escapes. Jev also describes an element with no name that starts with a heading, such as a region that its own heading names through `aria-labelledby`, by that heading. Before, Jev read such a region by the heading of the section before it.
- Read the snapshot lines that Playwright puts in YAML single quotes when a name holds text such as `: ` or ` #`, as in `- 'button "Status: live" [ref=e5]'`. `ACT` read the role of such an element as `'button`, and each `'` in its name as `''`. So the report line was wrong, and with `--jev`, Jev left the element out of its lists of buttons, fields, and scroll areas. The model now reads these lines without the quotes, as it reads the other lines.
- Add `DROP locator file:path`, which drops a file on an element, for upload widgets that are drop zones with no file input. The page gets the file's own name and size and a type from its extension. The path resolves relative to the `.whirl` file, as for `UPLOAD`, and an unprefixed locator finds the zone by its text, as in `DROP "Drop files here" file:report.csv`. When the zone's `dragover` does not call `preventDefault()`, the page rejects the drop and the step fails at once. `ACT` does not drop files.
- With `--video` on Chromium, a flow that ends before the page sends its first frame, such as a short flow on a still page, no longer fails as a runtime error. Whirl captures the page and holds that frame for the whole recording. A page that has not painted yet, as right after a navigation, gets more tries for up to 1 second. When Whirl still has no frame, as for a crashed page, the recording holds a white frame, and a warning says why. When ffmpeg fails while it finishes a recording, Whirl now skips the recording with a warning instead of a runtime error. Neither warning changes the file's result.
- Name each iframe in the snapshot `ACT` sends with its `aria-label`, or else its `title`. Playwright shows every iframe without a name, so neither the model nor Jev could tell which frame "scroll inside the incident history" means.
- With `--jev`, Jev plans drags and scrolls. For a drag it picks the element to drag, then where to drop it. For a scroll the first request also asks the way and whether the page or a part of it scrolls; Jev picks the part or the element to bring into view, and a position such as `50%`, `0.75`, halfway, the top, or the bottom comes from the instruction.
- Add `SCROLL`. `SCROLL locator` brings an element into view; `SCROLL [locator] down`, `up`, `left`, or `right` scrolls one visible height or width; and `SCROLL [locator] to 50%` scrolls to a vertical position. The element scrolls when it can, else the largest scrollable box inside it, else its nearest scrollable ancestor; an iframe scrolls the page inside it, and no locator scrolls the page. It scrolls at once and ends when the position holds, for content that loads on scroll, controls that react to it, and panels that scroll on their own. `ACT` runs `scrollIntoView`, `scrollTo`, `nextChunk`, `prevChunk`, `scrollLeft`, and `scrollRight`, and a scroll of the page's `<body>` scrolls the page; with `--jev`, scrolls go to the model.
- Add `DRAG source to target`, which drags one element onto another. It presses, holds 500 ms, moves in 10 steps, and releases, so drag code that starts after a press delay or a short move works as well as native HTML5 drag and drop, including into another frame. A bare `to` separates the locators; quote `"to"` to match the text. `ACT` runs it when the model answers `dragAndDrop` with the ref of the element to drop on; with `--jev`, drags go to the model.
- Add `RIGHTCLICK` and `MIDDLECLICK`, which click an element with the right or middle mouse button, to test a page's own context menu or `auxclick` handling. `ACT` runs them when the model answers `click` with `right` or `middle`, and with `--jev`, Jev plans them without the model.
- With `--jev`, choose from lists and dropdowns built without a native select: click the option the instruction names, or open the control and click the option in step two.
- With `--jev`, describe an element in a table by its table's caption or name and its column's header, so Jev tells apart two calendar months that both show a 14.
- With `--jev`, accept a clear leader when Jev is nearly certain something matches, hold an uneasy pick while the next list tries, and let copies of one control in one item share Jev's vote.
- With `--jev`, try every named element when no control fits, ask the full list in parallel parts when Jev rejects the shortened one, and confirm an exact quoted name with one small request.
- With `--jev`, let Jev say which quoted string or placeholder is the text to type, and read unquoted text with a small model call that sees only the instruction and must copy its words.
- With `--jev`, ask Jev a fuller intent question: richer action kinds, a merged vote when a click competes with a select, a double-click, or a key press, and the key, mouse button, checkbox end state, and suggestion step in the same request.
- Add `--jev`, which plans `ACT` with TypeSafe's Jev first and falls back to the `model` option when Jev is unsure, cannot supply an argument, or fails. `TYPESAFE_API_KEY` holds Jev's key, and `WHIRL_JEV_ENDPOINT` sends its requests to another server. Whirl asks Jev through `lithos-llm`, which prices its requests. JSON reports record the planner of each action and Jev's requests, tokens, and cost, which the step's cost includes.
- When the text `ACT` types matches a string the instruction quotes, type the instruction's characters, so a model that changes the case or spacing of `"AbC 123"` still types `AbC 123`.
- After `ACT` fills a field, read the value back. When the field does not hold it, as when a `maxlength` cuts it short, the step fails with `act-fill-mismatch` instead of passing silently. Case, spaces, and punctuation do not count, so a field that formats its value still passes.
- When `ACT` clicks a native checkbox or radio input, focus it and press Space, as `CHECK` does, so a styled control that covers the input does not make the click wait out the step.
- Add `ACT locator "instruction"`, which shows the model only one element and what it contains, for long pages.
- Leave each link's URL and the cursor hints out of the snapshot `ACT` sends, about a third of a link-heavy page.
- When `ACT` clicks, double-clicks, or hovers an element that wraps a narrower one, such as a custom dropdown's trigger, point at the element that shows its text instead of the wrapper's center.
- In a file that uses `ACT`, open every shadow root that page scripts attach, so `ACT` sees and acts inside closed shadow roots.
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
- Add `ACT "instruction"`, which asks a language model to choose one element action from a Playwright AI snapshot of the page and runs it as the matching Whirl action. The new `model` option selects the model through `lithos-llm`. `WHIRL_LLM_ENDPOINT` and `WHIRL_LLM_API_KEY` send calls to one OpenAI-compatible server instead. `{{env.NAME}}` values reach the model only as placeholders, and JSON reports record the actions ACT ran and the tokens it used.
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
