# Whirl browser shim protocol

This document is the wire contract between the Rust binary and the Node
browser shim. [SPEC.md](../../SPEC.md) defines product behavior and takes
precedence; this document only fixes the split of responsibilities and the
message shapes so the two sides can be built independently.

## 1. Process model

- The Rust binary spawns one shim process per worker slot:
  `<node> <shim-entry-js>`. The shim speaks the protocol on stdin/stdout and
  writes diagnostics to stderr. Rust captures stderr for runtime-error
  reporting.
- A shim process runs at most one flow (one browser context) at a time. It
  serves many flows in sequence.
- The shim keeps one launched browser per process. When the next flow needs a
  different engine or headed mode, it closes the old browser and launches the
  new one.
- If the shim becomes unresponsive (section 6), Rust kills it with SIGKILL and
  spawns a replacement for that worker slot. Other workers are unaffected.

## 2. Framing

Newline-delimited JSON, UTF-8, one object per line.

- Request (Rust → shim): `{"id": <u64>, "cmd": "<name>", "params": {...}}`
- Success (shim → Rust): `{"id": <u64>, "ok": true, "result": {...}}`
- Failure (shim → Rust): `{"id": <u64>, "ok": false, "error": {...}}`

Every request gets exactly one response. Rust sends at most one step command
at a time, but `cancelFlow` (section 6) may be sent while a step is in
flight, so the shim must read requests continuously and must not block its
read loop on an in-flight step. Responses may therefore arrive out of request
order; `id` matches responses to requests.

Error object:

```json
{
  "kind": "<see section 7>",
  "message": "human-readable detail",
  "expected": "optional string",
  "actual": "optional string",
  "candidates": ["optional list of matched-element descriptions"]
}
```

## 3. Lifecycle commands

### `hello`

Sent once after spawn. Params: `{}`. Result:
`{"protocol": 2, "playwrightVersion": "1.62.1", "ffmpegPath": "abs path" | null}`.
`ffmpegPath` is Playwright's bundled ffmpeg, which every video recording
needs; `null` means it is not installed. `whirl doctor` reports it.
Protocol 2 replaces value checks and captures with the `read` and
`readResponse` commands (sections 4.4 and 4.5).

### `startFlow`

Creates the browser context and page for one flow. Params:

```json
{
  "browser": "chromium" | "firefox" | "webkit",
  "headed": false,
  "viewport": {"width": 1280, "height": 720},
  "storageStatePath": "abs path" | null,
  "dialogs": "dismiss" | "accept",
  "allowHosts": ["example.com", "*.example.com"] | null,
  "navTimeoutMs": 30000,
  "userAgent": "chrome" | "firefox" | "safari" | "literal string" | null,
  "reducedMotion": "reduce" | "no-preference" | null,
  "video": {"tempDir": "abs path", "finalPath": "abs path", "fps": 60 | null} | null,
  "harPath": "abs path" | null,
  "trace": false,
  "openShadowRoots": false
}
```

Result: `{"browserVersion": "...", "nodeVersion": "...", "playwrightVersion": "...", "userAgent": "...", "videoFps": 60 | null}`. These are the active browser, Node process, and Playwright library versions, plus the context's actual `navigator.userAgent`, plus the frame rate of the flow's recording (`null` without `video`). Older protocol 1 shims may omit these additive fields; reports then use null values. Rust applies secret masking to the user agent before reporting it.

- `allowHosts: null` means all hosts are allowed. When it is a list, Rust has
  already appended the `base` host; the shim routes all requests and aborts
  any whose hostname matches no glob, records the blocked hostname, blocks
  WebSockets to non-matching hosts the same way, and disables service workers.
  Globs match the hostname only. `*.` prefixes do not match the apex.
  `data:` and `blob:` URLs are always allowed.
- `dialogs` installs an auto-dismiss or auto-accept handler for alert,
  confirm, and prompt.
- `userAgent` sets the context's user agent string; `null` keeps the
  engine default. Rust sends the value after interpolation and CLI overrides.
  The shim resolves `chrome`, `firefox`, and `safari` to the user agent strings
  from Playwright's `Desktop Chrome`, `Desktop Firefox`, and `Desktop Safari`
  presets. It uses no other preset fields. All other strings pass through.
- `reducedMotion` emulates the `prefers-reduced-motion` media feature for
  the context; `null` keeps the engine default.
- `trace: true` starts Playwright tracing (screenshots and snapshots on).
- `openShadowRoots: true` adds an init script to the context that makes every
  `attachShadow` call create an open root, so the AI snapshot and locators see
  inside roots a page asks to close. Rust sets it when the file uses `ACT`
  (SPEC 7.4).
- `video` records video into `tempDir`; at `endFlow` the shim moves the
  recording to `finalPath`. With `fps: null` the shim uses Playwright's
  `recordVideo` at its fixed 25 frames per second. With a number (Chromium
  only; Rust sends `null` for other engines) the shim records the main
  page's CDP screencast through Playwright's bundled ffmpeg at that rate,
  holding the last frame while the page is still. A number for another
  engine, or a missing ffmpeg, fails `startFlow` with kind `"internal"`.
  An ffmpeg failure at `endFlow` fails `endFlow` the same way. `cancelFlow`
  discards an in-progress screencast recording.

### `endFlow`

Ends the flow and closes the context. Params:

```json
{"saveStoragePath": "abs path" | null, "tracePath": "abs path" | null}
```

- `saveStoragePath` writes the context storage state before close.
- `tracePath` exports the trace there; `null` discards a running trace.

Result: `{"blockedHosts": ["host", ...], "videoPath": "abs path" | null}`.
`blockedHosts` is the sorted, de-duplicated set of hostnames blocked by
`allowHosts` during the flow.

### `cancelFlow`

Out-of-band abort (section 6). Params: `{}`. The shim force-closes the page
and context. The in-flight step command, if any, responds with error kind
`"cancelled"` (either order is fine). Result: `{}`. After `cancelFlow` the
shim is ready for the next `startFlow`.

### `shutdown`

Params: `{}`. Result: `{}`, then the shim closes the browser and exits 0.

## 4. Step commands

Most step commands run one SPEC line. `read` and `readResponse` read one value for a check or capture that Rust evaluates, and `traceGroup` and `traceGroupEnd` group those reads in the trace. Common params on every step command:

- `timeoutMs`: the budget for this step. The shim passes it to the underlying
  Playwright call or `expect` and must not retry past it. Rust computes it
  (step timeout, `@duration` override, or remaining entry budget, whichever
  is smallest).
- `entryStart`: optional boolean; true resets popup and request observation windows before the step runs. Rust sets it on the first action of each entry. False or absent preserves the window.
- `title`: the rendered, secret-masked step text (for example
  `FILL label:"Password" ***`), or `null`. When tracing is on, the shim wraps
  a step with a title in `tracing.group(title)` so trace step titles never
  contain secrets. A step with `title: null` gets no group of its own. Rust
  sends the reads of one check with `title: null`, inside one `traceGroup`.

Commands and their extra params (result `{}` unless noted):

| cmd | params |
| --- | --- |
| `visit` | `url` (absolute; Rust resolved `base`); resolves at the new document's `DOMContentLoaded`, not `load` |
| `response` | `name`, `method`, `url` (absolute) — select the first matching request from the selected tab in the current entry and await its response headers |
| `http` | `name`, `method`, `url` (absolute), `headers` (array of `[name, value]` pairs), `body` (string or null) — send a request without browser cookies and name its completed response |
| `popup` | `name` — name an unnamed popup from the selected tab in the current entry, without selecting it |
| `tab` | `name` — select a named open tab |
| `close` | `name` — close a named tab without changing selection |
| `click` | `locator` |
| `dblclick` | `locator` |
| `fill` | `locator`, `value` |
| `type` | `locator`, `text` (one key event per character via `pressSequentially`) |
| `press` | `locator` (or `null`), `key` |
| `checkbox` | `locator`, `checked` (bool; CHECK/UNCHECK) |
| `selectOption` | `locator`, `label` |
| `hover` | `locator` |
| `upload` | `locator`, `path` (absolute; Rust resolved it) |
| `screenshot` | `path` (absolute .png; full page) |
| `snapshot` | `baselinePath`, `actualPath`, `diffPath`, `update` (bool) |
| `evalAction` | `script` |
| `store` | `scope` (`"local"` \| `"session"` \| `"cookie"`), `key`, `value` — writes one `localStorage` or `sessionStorage` entry on the current origin, or one cookie for the current page's URL (host, path `/`, no attributes); `cookie` on a non-http(s) page is an `action` error |
| `ariaSnapshot` | `locator` (or `null`); result `{"snapshot": "..."}`, the selected tab's `page.ariaSnapshot({ mode: "ai" })`, or that one element's `locator.ariaSnapshot({ mode: "ai" })` with the usual waiting and strictness, for `ACT` (SPEC 7.4) |
| `page` | `expect` (section 4.2) |
| `assert` | `spec` (section 4.3) — state checks and tab closure only |
| `read` | `subject` (section 4.4); result `{"type": "value", "value": ...}` or `{"type": "missing", "reason": "no-element" \| "absent-attribute"}` |
| `readResponse` | `name`, `body` (bool) (section 4.5); result `{"status": 201, "url": "...", "headers": [[name, value], ...], "bodyBase64": "..." \| null, "bodyError": "..." \| null, "bodyMayBeDecoded": false}` |
| `traceGroup` | none; opens one trace group named by `title` for the reads of one check |
| `traceGroupEnd` | none; closes the group that `traceGroup` opened |

Semantics the shim owns (per SPEC sections 7, 9, 15):

- Element steps use Playwright auto-waiting and actionability. A locator that
  resolves to more than one element is a strictness failure reported with
  error kind `"strictness"` and the candidate list.
- `screenshot` reports errors normally; Rust downgrades them to warnings.
- `snapshot` is a shim-owned poll loop: capture frames until two consecutive
  frames are byte-identical, compare with Playwright's image comparator
  (identical dimensions; per-pixel color-distance threshold 0.2, no other
  tolerance), recapture on mismatch until `timeoutMs` expires. On final
  mismatch write `actualPath` and `diffPath` and reply with error kind
  `"snapshot-mismatch"`. A missing baseline is error kind
  `"snapshot-missing-baseline"` (Rust reports it as a runtime error). With
  `update: true`, write the settled frame to `baselinePath` and reply
  `{"updated": true}`.
- `evalAction` and an `eval` read: run the script as the body of an
  async function in the page main world via `page.evaluate`. If the script
  parses as a single expression, run `return (script);`; otherwise run it as
  written. Await a returned promise. Syntax errors, thrown exceptions,
  rejections, and timeouts are error kind `"eval"`. The action form discards
  the result.

### 4.1 Locator JSON

A locator is an array of segments, in chain order:

```json
[
  {"type": "role", "role": "button", "name": "Sign in" , "exact": true},
  {"type": "role", "role": "button", "name": null, "exact": true},
  {"type": "label", "text": "Email", "exact": true},
  {"type": "placeholder", "text": "Search", "exact": false},
  {"type": "text", "text": "Add to cart", "exact": true},
  {"type": "alt", "text": "Logo", "exact": true},
  {"type": "title", "text": "Info", "exact": true},
  {"type": "testid", "id": "cart-badge"},
  {"type": "css", "selector": ".foo > .bar"},
  {"type": "frame", "selector": "#payment-element iframe"},
  {"type": "nth", "index": 0},
  {"type": "ref", "ref": "e12"}
]
```

`exact: false` is the `~` substring variant (Playwright default matching).
`frame` selects an iframe in the current scope. Following `nth` segments narrow
that iframe before `contentFrame()` enters it. The next non-`nth` segment runs
inside the frame. Rust requires an element segment after the final frame.

`nth.index` is 0-based, and a negative index counts from the end. The shim
passes it to `.nth(index)` unchanged. Rust guarantees `nth` is never first.
The mapping to Playwright calls is SPEC section 6.1.

`ref` names an element from an `ariaSnapshot` result, such as `e12`, or `f1e3`
inside an iframe. The shim resolves it with `page.locator("aria-ref=e12")`.
Only Rust creates `ref` segments, as the only segment of an `ACT` action's
locator, and only for refs in the latest snapshot. `.whirl` files have no
syntax for them. For `click`, `dblclick`, and `hover` on a `ref` locator, the
shim points at the deepest descendant that shows the element's text, when one
exists, instead of the element's center (SPEC 7.4).

Popup names are local to a flow; `main` names the original page. The shim records
popup events before actions and attaches dialog handling to every page. It keeps
named closed pages for closure assertions. All page operations use the selected
tab, including failure screenshots. The main page remains the `--video` source.

A tab closure assertion uses the `assert` command with
`{"subject":{"type":"tab","name":"payment"},"check":{"type":"closed"}}`.
It can run after the selected tab closes; no page evaluation is required.

Rust reads a named response with `readResponse` (section 4.5) and evaluates
every response check and capture itself (ADR `evaluate-checks-in-rust`).

`http` uses the runtime's Fetch API with redirects disabled. It checks the host
allowlist before sending, reads the completed body within the step timeout, and
limits decoded response bodies to 1 MiB. A context close aborts outstanding HTTP
requests. The request has no browser cookie store; response cookies do not
change the browser. Rust resolves URL, header, and body variables and validates
literal header names before sending the command.

The shim listens to context request events before navigation so popup initial
requests are included. Selection is scoped to the current entry and selected
tab, including its frames, and uses the first method/URL match regardless of
outcome. Named responses survive entry resets. Response checks never reselect
a retry. `readResponse` waits for body completion and enforces the step
deadline and SPEC body limit; Rust caches its result for the flow. Observation
listeners and references are released when the context closes.

### 4.2 PAGE expectation

```json
{"kind": "path", "value": "/dashboard"}
{"kind": "pathQuery", "value": "/search?q=widget"}
{"kind": "url", "value": "https://shop.example.com/x"}
{"kind": "regex", "source": "checkout/\\d+", "flags": ""}
```

Retried like an assert up to `timeoutMs`. `path` compares the URL path only;
`pathQuery` compares path plus query (`?` included); both exclude the
fragment. `url` compares the full URL string. `regex` tests the full URL.
Failures use error kind `"assert"` with `expected`/`actual` set.

In a `regex` object, `source` carries the pattern with the `\/` delimiter
escape unescaped to a plain `/`; all other escape sequences are verbatim.
`flags` is zero or more of `i`, `s`, `m` in that order. The shim adds the `u`
flag when it compiles the pattern (SPEC section 3.1).

### 4.3 Assert spec

```json
{
  "subject": {"type": "locator", "locator": [...]},
  "check": {"type": "state", "state": "visible" | "hidden" | "enabled" | "disabled" | "checked" | "unchecked" | "focused"}
}
{"subject": {"type": "tab", "name": "payment"}, "check": {"type": "closed"}}
```

The shim compiles each state check to a Playwright web-first assertion
(SPEC section 15). `hidden` passes on zero matches; more than one match fails
immediately with candidates, including for `hidden`. Failures reply with
error kind `"assert"` and `expected`/`actual` strings.

### 4.4 Read subject

```json
{"type": "element", "locator": [...], "extract":
    {"type": "text"} | {"type": "value"} | {"type": "attr", "name": "href"}}
{"type": "count", "locator": [...]}
{"type": "url"}
{"type": "title"}
{"type": "eval", "script": "window.dataLayer"}
```

A `read` makes one attempt and never waits; Rust owns the retry loop (SPEC
section 9.7). It replies with `{"type": "value", "value": ...}` or
`{"type": "missing", "reason": "no-element" | "absent-attribute"}`:

- `element` resolves the locator once. More than one match fails at once
  with error kind `"strictness"` and candidates. No match, or an absent
  attribute on the element, is `missing`. `text` returns the normalized text
  content (SPEC section 9.2) as Playwright's text engine sees it: text nodes
  and shadow roots, without `script`, `noscript`, `style`, or document head
  content. `value` returns the input value, and `attr` the attribute value,
  each as a string. `value` on an element that is not an
  input is error kind `"read"`. An element that detaches during the read
  reads as `missing`.
- `count` returns the current number of matches as a JSON number. It is never
  `missing`.
- For `element` and `count`, a `frame` segment that matches more than one
  iframe fails at once with error kind `"strictness"` and the iframes as
  candidates.
- `url` returns `page.url()`, and `title` the normalized document title.
- `eval` runs under the `evalAction` rules, then applies the SPEC section 10
  result contract inside the page: a string, `null`, a boolean, a finite
  number, or an array or plain object that contains only those. The shim
  returns that JSON value. Anything else is error kind `"eval-result"`.
  Classification happens in the page, so the result is independent of
  Playwright's transport.

### 4.5 Read response

`readResponse` takes `name`, a name from `response` or the implicit name of an
`http` step, and `body`. It replies with `status` (a number), `url` (the
request URL, for `location`), and `headers` (an array of `[name, value]`
pairs in received order). With `body: true` it also waits for the complete
body within `timeoutMs` and returns it as `bodyBase64`, after content
decoding. A body that cannot be read, such as a redirect's or one over the
SPEC body limit, comes back as `bodyError` text with `bodyBase64: null`, so
status and header checks still work. Rust reads the body only for a check
that needs it. An unknown name is error kind `"internal"`.

`bodyMayBeDecoded` is true for a `response` in Chromium or WebKit. Those
browsers can hand a text body back already decoded, and Playwright then
encodes it as UTF-8. Rust undoes that decoding when the `Content-Type` names a
charset other than UTF-8 that can encode the text (SPEC section 9.2). It is
false for an `http` step and in Firefox, which give the exact bytes.

## 5. Timeouts

The shim bounds every step with `timeoutMs` through Playwright options,
`expect` timeouts, or its own poll-loop deadline. Rust additionally arms an
external watchdog per step (`timeoutMs` plus a grace period). When the
watchdog fires — for example an `EVAL` that blocked the renderer so the
shim's Playwright call never settles — Rust sends `cancelFlow`. If the shim
does not answer `cancelFlow` within the grace period, Rust kills the process
(section 1). Node stays responsive when the page hangs, so `cancelFlow`
normally succeeds.

## 6. Cancellation

`cancelFlow` is the only out-of-band command. The shim must:

1. Immediately force-close the page and context (its own internal watchdog
   bounds the close).
2. Fail the in-flight step, if any, with error kind `"cancelled"`.
3. Reply to `cancelFlow` with `{}` and be ready for `startFlow`.

## 7. Error kinds

| kind | meaning |
| --- | --- |
| `timeout` | The step's Playwright call or poll loop hit `timeoutMs` |
| `strictness` | Locator matched more than one element (`candidates` set) |
| `assert` | State check, tab closure, or `PAGE` failed at timeout (`expected`/`actual` set) |
| `snapshot-mismatch` | Stable visual difference (actual/diff written) |
| `snapshot-missing-baseline` | No baseline image and `update` false |
| `eval` | Script syntax error, exception, or rejection |
| `eval-result` | `eval` read result outside the SPEC section 10 contract |
| `read` | A read failed: `value` on an element that is not an input, or page churn mid-read |
| `action` | Actionability failure other than the above |
| `stale-ref` | A `ref` locator (section 4.1) no longer matches an element, because the page replaced it after the snapshot. The shim reports it at once instead of waiting. |
| `cancelled` | Step aborted by `cancelFlow` |
| `internal` | Shim bug or unexpected Playwright error |

Rust maps kinds to reporting: `timeout`, `strictness`, `assert`,
`snapshot-mismatch`, `eval`, `eval-result`, `read`, `action`, and `stale-ref`
are test failures (exit 1); inside a page check, `read`, `eval`, and `eval-result`
mean "not passing yet" and Rust reads again; `snapshot-missing-baseline` and `internal` are runtime
errors (exit 3). A malformed request is answered with kind `"internal"`.

## 8. Development environment

Rust resolves the shim in this order:

1. `WHIRL_SHIM_JS` (path to the built shim entry) and `WHIRL_NODE` (node
   executable, name or path) — set by the repository's mise environment for
   development and tests.
2. The installed bundle provisioned by `whirl install` in the platform data
   directory.

Missing both is a runtime error (exit 3) with a remedy naming
`whirl install`.
