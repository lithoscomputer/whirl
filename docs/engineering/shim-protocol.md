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
`{"protocol": 1, "playwrightVersion": "1.62.1"}`.

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
  "video": {"tempDir": "abs path", "finalPath": "abs path"} | null,
  "harPath": "abs path" | null,
  "trace": false
}
```

Result: `{}`.

- `allowHosts: null` means all hosts are allowed. When it is a list, Rust has
  already appended the `base` host; the shim routes all requests and aborts
  any whose hostname matches no glob, records the blocked hostname, blocks
  WebSockets to non-matching hosts the same way, and disables service workers.
  Globs match the hostname only. `*.` prefixes do not match the apex.
  `data:` and `blob:` URLs are always allowed.
- `dialogs` installs an auto-dismiss or auto-accept handler for alert,
  confirm, and prompt.
- `trace: true` starts Playwright tracing (screenshots and snapshots on).
- `video` records video into `tempDir`; at `endFlow` the shim moves the
  recording to `finalPath`.

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

Each step command runs one SPEC line. Common params on every step command:

- `timeoutMs`: the budget for this step. The shim passes it to the underlying
  Playwright call or `expect` and must not retry past it. Rust computes it
  (step timeout, `@duration` override, or remaining entry budget, whichever
  is smallest).
- `title`: the rendered, secret-masked step text (for example
  `FILL label:"Password" ***`). When tracing is on, the shim wraps the step
  in `tracing.group(title)` so trace step titles never contain secrets.

Commands and their extra params (result `{}` unless noted):

| cmd | params |
| --- | --- |
| `visit` | `url` (absolute; Rust resolved `base`); resolves at the new document's `DOMContentLoaded`, not `load` |
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
| `store` | `scope` (`"local"`), `key`, `value` — writes one `localStorage` entry on the current origin |
| `page` | `expect` (section 4.2) |
| `assert` | `spec` (section 4.3) |
| `capture` | `source`, `filter` (section 4.4); result `{"value": "..."}` |

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
- `evalAction` / `eval` capture source: run the script as the body of an
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
  {"type": "nth", "index": 1}
]
```

`exact: false` is the `~` substring variant (Playwright default matching).
`nth.index` is 1-based; the shim subtracts 1. Rust guarantees `nth` is never
first and `index >= 1`. The mapping to Playwright calls is SPEC section 6.1.

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

### 4.3 Assert spec

```json
{
  "subject": {"type": "locator", "locator": [...]} | {"type": "url"} | {"type": "title"},
  "check":
    {"type": "state", "state": "visible" | "hidden" | "enabled" | "disabled" | "checked" | "unchecked" | "focused"}
  | {"type": "text", "op": <strop>}
  | {"type": "value", "op": <strop>}
  | {"type": "attr", "name": "aria-expanded", "op": <strop>}
  | {"type": "count", "op": "==" | "!=" | "<" | "<=" | ">" | ">=", "value": 3}
}
```

String operator `<strop>`:

```json
{"op": "==" | "!=" | "contains", "value": "text"}
{"op": "matches", "source": "Order #\\w+", "flags": "i"}
```

A `url` or `title` subject always carries a check of type `"text"` with a
string operator; the shim picks `toHaveURL`/`toHaveTitle` from the subject.
In every regex object (`matches` operators, PAGE `regex`, capture `filter`),
`source` carries the pattern with the `\/` delimiter escape unescaped to a
plain `/`; all other escape sequences are verbatim. `flags` is zero or more
of `i`, `s`, `m` in that order.

The shim compiles each check to a Playwright web-first assertion where one
exists and to a shim-owned poll loop with the same timeout otherwise (SPEC
section 15). SPEC section 9 semantics apply: `hidden` passes on zero matches;
more than one match fails immediately with candidates (including for
`hidden`); `attr` with `!=` also passes when the attribute is absent; `count`
accepts any number of matches. Failures reply with error kind `"assert"` and
`expected`/`actual` strings.

### 4.4 Capture source

```json
{"type": "element", "locator": [...], "extract":
    {"type": "text"} | {"type": "value"} | {"type": "count"} | {"type": "attr", "name": "href"}}
{"type": "url"}
{"type": "title"}
{"type": "eval", "script": "document.title.trim()"}
```

`filter` is `{"source": "Order #(\\w+)", "flags": ""} | null`. The shim
applies it to the extracted string and returns capture group 1, or the whole
match when the pattern has no group; no match is error kind `"capture"`.

- `text`, `value`, and `attr` wait for exactly one element up to `timeoutMs`;
  after the element resolves, an absent attribute is an immediate error kind
  `"capture"`.
- `count` never waits; it returns the current match count as a decimal
  string.
- `eval` runs under the `evalAction` rules, then applies the SPEC section 10
  result contract inside the page: a string is returned as-is; `null`,
  booleans, finite numbers, and arrays/plain objects containing only those
  are returned as compact JSON; anything else is error kind `"eval-result"`.
  Classification and serialization happen in the page so the result is
  independent of Playwright's transport.

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
| `assert` | Check failed at timeout (`expected`/`actual` set) |
| `snapshot-mismatch` | Stable visual difference (actual/diff written) |
| `snapshot-missing-baseline` | No baseline image and `update` false |
| `eval` | Script syntax error, exception, or rejection |
| `eval-result` | Capture `eval` result outside the section 10 contract |
| `capture` | Extraction failed (absent attribute, regex mismatch) |
| `action` | Actionability failure other than the above |
| `cancelled` | Step aborted by `cancelFlow` |
| `internal` | Shim bug or unexpected Playwright error |

Rust maps kinds to reporting: `timeout`, `strictness`, `assert`,
`snapshot-mismatch`, `eval`, `eval-result`, `capture`, and `action` are test
failures (exit 1); `snapshot-missing-baseline` and `internal` are runtime
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
