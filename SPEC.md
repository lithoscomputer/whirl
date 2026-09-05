# Whirl V1 Specification

Status: v1
Date: 2026-08-18

Whirl is a command-line tool that runs web UI tests written in plain text files. It is to browser flows what [Hurl](https://hurl.dev/) is to HTTP: a tight, closed, file-based format that is readable, diffable, and easy to generate. The `whirl` binary is written in Rust and drives real browsers through Playwright.

## 1. Design principles

1. **Closed vocabulary.** The language has a fixed set of actions, checks, and extractors. There are no conditionals, loops, functions, or user-defined keywords. A flow that needs branching is two files.
2. **No waits in the language.** Actions auto-wait for their target. Assertions retry until they pass or time out. The format has no `SLEEP` and no `WAIT`.
3. **Semantic locators first.** The locator grammar puts `role:` and `label:` in front and makes raw CSS the visually distinct escape hatch.
4. **One flow per file, top to bottom.** A file is a linear sequence of entries. Execution order is textual order. A failure stops the file.
5. **Plain text.** Files are UTF-8, line-oriented, comment-friendly, and review well in a diff.

## 2. Example

```whirl
# checkout.whirl — buy a widget as a signed-in user.
[Options]
base: https://shop.example.com
viewport: 1280x800

# Log in.
VISIT /login

FILL "Email" alice@example.com
FILL "Password" {{env.TEST_PASSWORD}}
CLICK role:button "Sign in"
PAGE /dashboard
[Asserts]
role:heading "Welcome back" visible
testid:user-menu text == Alice

# Find a product.
FILL placeholder:"Search products" widget
PRESS Enter
[Asserts]
url contains "q=widget"
testid:result-card count >= 1
[Captures]
first_product: testid:result-card >> nth:1 >> role:link attr:href

# Add it to the cart.
VISIT {{first_product}}
CLICK "Add to cart"
[Asserts]
testid:cart-badge text == 1
role:alert text contains "Added to cart"
```

Run it:

```console
$ whirl flows/checkout.whirl
$ whirl --report-junit report.xml flows/
```

## 3. Files

- Extension: `.whirl`. Encoding: UTF-8. Line endings: LF or CRLF.
- The format is line-oriented. Each action, check, capture, and option is one line.
- `#` starts a comment. A comment runs to the end of the line. A `#` inside a quoted string or a regex literal is literal text.
- Blank lines are ignored everywhere.
- Keywords (`VISIT`, `PAGE`, `[Asserts]`, checks, prefixes) are case-sensitive.

### 3.1 Values

A **value** is written in one of two forms:

- **Quoted**: `"..."` with backslash escapes `\"`, `\\`, `\n`, `\t`, and `\u{XXXX}`.
- **Bare**: a single token with no whitespace, no `"`, and no `#`. Bare and quoted forms are interchangeable, with one reservation: a line's final bare token of the form `@duration` always parses as the step timeout (section 12), so that value must stay quoted (`"@60s"`). `whirl fmt` never removes quotes whose removal would change the parse.

Values support variable interpolation with `{{name}}` (section 11). Write `\{{` for a literal `{{`.

Other literal forms:

- **Regex**: `/pattern/` with optional flags `i`, `s`, `m` (for example `/Order #\w+/i`). Escape a literal slash as `\/`. Patterns use JavaScript (ECMAScript) regex syntax, because checks execute inside Playwright's engine.
- **Number**: a non-negative decimal integer.
- **Duration**: an integer with unit `ms` or `s` (for example `500ms`, `10s`).
- **Viewport**: `WIDTHxHEIGHT` in CSS pixels (for example `1280x800`).

## 4. File structure

```
file    := [Options-section] entry+
entry   := action+ [PAGE-line] [Asserts-section] [Captures-section]
```

- The optional `[Options]` section appears once, before the first entry.
- An **entry** is one or more action lines, then an optional `PAGE` line, then an optional `[Asserts]` section, then an optional `[Captures]` section, in that order.
- An action line after a `PAGE` line, an `[Asserts]` section, or a `[Captures]` section starts a new entry. There is deliberately no other delimiter: consecutive action lines always belong to one entry, because an entry is Whirl's unit of verification, timeout scope, and reporting, and actions group with the checks that follow them. Blank lines and comments never split an entry.
- The first action in a file must be `VISIT`, because no page exists yet.

## 5. Options

The `[Options]` section holds `key: value` lines. V1 keys:

| Key | Value | Default | Meaning |
| --- | --- | --- | --- |
| `base` | URL | none | Base URL for relative `VISIT` and `PAGE` |
| `browser` | `chromium` \| `firefox` \| `webkit` | `chromium` | Browser engine |
| `viewport` | `WxH` | `1280x720` | Viewport size |
| `step-timeout` | duration | `10s` | Default per-step timeout for actions, asserts, and captures |
| `entry-timeout` | duration | none | Cap on an entry's total time across all of its lines |
| `nav-timeout` | duration | `30s` | Navigation timeout for `VISIT` |
| `allow-hosts` | glob list | all hosts | Hosts the browser may reach; requests to others are aborted |
| `dialogs` | `dismiss` \| `accept` | `dismiss` | Automatic response to alert, confirm, and prompt dialogs |
| `reduced-motion` | `reduce` \| `no-preference` | engine default | What the page's `prefers-reduced-motion` media query reports |
| `storage` | file path | none | Saved storage state loaded into each file's browser context |
| `user-agent` | alias or string | engine default | User agent string the browser sends and reports |
| `setup` | file path | none | A flow that runs first; this file starts from its final state |

`allow-hosts` takes one or more host globs (`allow-hosts: example.com *.example.com`). Globs match the request's hostname only — scheme and port are ignored — and `*.example.com` does not match the apex `example.com`; list both to cover both. The `base` host is always allowed. Whirl aborts requests to any other host, including fetch/XHR, WebSockets, and subresources, and the reports list every blocked host. Service workers are disabled when `allow-hosts` is set, because they can bypass request routing. IP-literal hosts match textually; `data:` and `blob:` URLs have no host and are always allowed. Without the option, all hosts are allowed.

`storage` names a Playwright storageState JSON file, resolved relative to the `.whirl` file. Each browser context starts from that saved state (cookies and local storage) instead of empty, so flows can skip UI login. Produce the file with `--save-storage`, which writes the final context state of a successful run — typically of a dedicated login flow.

`setup` names another `.whirl` file, resolved relative to this one, whose final state this file starts from: Whirl runs the setup flow first, in its own context, saves that context's cookies and storage, and starts this file's context from the saved state, the way `storage` would. The two options cannot be combined. The setup flow is an ordinary flow with its own options and its own `VISIT`, so it runs and debugs on its own, and it may not name a `setup` of its own. Every file that names the same setup flow in one invocation shares one run of it: ten flows that need a signed-in session sign in once. The saved state lives only for the invocation. The setup flow's captures are readable in the dependent file as `{{setup.name}}` (section 11). When the setup flow fails, its dependents do not start and each reports the failure as its `[setup]` case (section 12). The path must be literal: it is resolved before any variable exists.

`reduced-motion: reduce` makes the page's `prefers-reduced-motion` media query match, as it does for a user who asked their OS for less motion. Pages that honor it skip transitions, looping animations, and background video, which makes `SNAPSHOT` baselines stable and removes work the flow never asserted. `no-preference` forces the opposite; without the option the engine default applies.

`user-agent` replaces the browser's user agent string for the flow's context, in request headers and in `navigator.userAgent`. It exists for testing an app's own user-agent handling and for apps that gate on the string, such as bot protection that rejects headless Chromium's `HeadlessChrome` token.

The exact, case-sensitive values `chrome`, `firefox`, and `safari` are aliases for
the user agent strings in the bundled Playwright's `Desktop Chrome` (Windows),
`Desktop Firefox` (Windows), and `Desktop Safari` (macOS) presets. For example,
`user-agent: chrome` uses the Chrome string. Aliases change only the user agent;
`browser`, viewport, and other context settings remain independent. Their strings
are pinned to the bundled Playwright version and may change with a Whirl upgrade.
All other values are literal strings, including unknown names such as `chorme`.
Quote a value that contains spaces. Quoted and unquoted forms have the same
meaning: `chrome` and `"chrome"` both select the alias. Alias resolution happens
after option interpolation and CLI overrides; `--user-agent chrome` works too.

Unknown keys are a parse error. When section 13 defines a corresponding command-line flag, that flag overrides the file option.

## 6. Locators

A locator selects one element (or, for `count`, a set of elements). It is a chain of one or more segments joined by `>>`. Each later segment searches inside the result of the chain so far. `nth:` may not be the first segment, and its index starts at 1 — `nth:0` is a parse error.

```
locator := segment (">>" segment)*
segment := prefix ":" value [value]   # second value: role name only
         | "nth:" number
         | value                      # default engine; actions only (6.1)
```

### 6.1 Segment prefixes

| Segment | Playwright equivalent |
| --- | --- |
| `role:TYPE "Name"` | `getByRole('TYPE', { name: 'Name', exact: true })` |
| `role:TYPE` | `getByRole('TYPE')` |
| `label:"Text"` | `getByLabel('Text', { exact: true })` |
| `placeholder:"Text"` | `getByPlaceholder('Text', { exact: true })` |
| `text:"Text"` | `getByText('Text', { exact: true })` |
| `alt:"Text"` | `getByAltText('Text', { exact: true })` |
| `title:"Text"` | `getByTitle('Text', { exact: true })` |
| `testid:id` | `getByTestId('id')` |
| `frame:"selector"` | Select an iframe by CSS and enter its document for subsequent segments. |
| `css:"selector"` | `locator('selector')` — escape hatch |
| `nth:N` | `.nth(N - 1)` — 1-based position |

Text matching is exact (after whitespace normalization). For partial or pattern matching, assert on the element instead (`text contains`, `text matches`).

Every text-matching prefix has a substring variant marked with `~` — `role~:`, `label~:`, `placeholder~:`, `text~:`, `alt~:`, `title~:` — which matches by case-insensitive substring, Playwright's default matching. So `text~:"Added"` matches "Added to cart". `testid:` and `css:` have no `~` form, and the unprefixed default engine stays exact.

An unprefixed value in locator position selects a default engine: `label:` for form actions (`FILL`, `SELECT`, `CHECK`, `UNCHECK`, `UPLOAD`, and `PRESS` with a target), and `text:` for pointer actions (`CLICK`, `DBLCLICK`, `HOVER`) — buttons and links have no label; their accessible name is their text. So `FILL "Email" alice@example.com` fills the input labeled Email, and `CLICK "Add to cart"` clicks the element with that exact text. Prefixes stay available everywhere for precision. Default engines exist only in actions: in `[Asserts]` and `[Captures]` every segment must carry a prefix (or be `nth:`), and an unprefixed value there is a parse error.

`frame:` works in actions, asserts, and captures. It may follow an element scope or another frame. An immediately following `nth:N` selects the iframe before entering it. A frame must be followed by an element segment; use `css:` to check the iframe element itself. Nested and cross-origin frames use the same syntax. Frames are resolved lazily, so normal actionability and assertion timeouts also cover frames that load or are replaced later. Multiple matching frames fail strictly unless narrowed explicitly.

```whirl
FILL frame:"#payment-element iframe" >> label:"Card number" "4242424242424242"
[Asserts]
frame:"#payment-element iframe" >> label:"Card number" value contains "4242"
```

### 6.2 Strictness

When an action or a single-element check runs, the locator must resolve to exactly one element. Zero matches fails after the timeout — except the `hidden` check, which passes when nothing matches (section 9.1). More than one match fails immediately with the candidate list, for `hidden` as well. Narrow the locator or add `nth:`. Only `count` accepts any number of matches.

## 7. Actions

An action is a verb, an optional locator, and an optional value. Element-targeting actions use Playwright's actionability checks for that operation up to the applicable timeout. A trailing `@duration` overrides the step timeout for that line (section 12).

| Syntax | Meaning |
| --- | --- |
| `VISIT url` | Navigate, and continue once the new document has parsed. A `url` starting with `/` resolves against `base`. |
| `RESPONSE name METHOD url` | Name the first matching HTTP request started in this entry and wait for its response headers. |
| `HTTP name METHOD url [header:NAME value]... [body:value]` | Send an independent HTTP request and name its response for checks and captures. |
| `POPUP name` | Name an unnamed popup opened by the selected tab in this entry; selection stays unchanged. |
| `TAB name` | Select an open named tab for subsequent commands. The original tab is `main`. |
| `CLOSE name` | Close a named tab; selection stays unchanged. Already closed tabs succeed. |
| `CLICK locator` | Click the element. |
| `DBLCLICK locator` | Double-click the element. |
| `FILL locator "text"` | Replace the input's content with `text`. |
| `TYPE locator "text"` | Focus the element, then send one key event per character of `text`. |
| `PRESS "Key"` | Send a key or chord (Playwright key names, for example `"Enter"`, `"Control+A"`) to the focused element. |
| `PRESS locator "Key"` | Focus the element, then send the key. |
| `CHECK locator` | Set a checkbox, radio, or switch to checked. |
| `UNCHECK locator` | Set a checkbox or switch to unchecked. |
| `SELECT locator "Label"` | Choose the `<select>` option with visible text `Label`. |
| `HOVER locator` | Move the pointer over the element. |
| `UPLOAD locator file:path` | Set the file input to `path`, resolved relative to the `.whirl` file. |
| `SCREENSHOT name` | Save a full-page screenshot as artifact `name.png`. The name is an identifier that may also contain hyphens. Never fails the entry (see below). |
| `SNAPSHOT name` | Compare a full-page screenshot against the stored baseline; fails the entry on visual difference. |
| `EVAL "script"` | Run a JavaScript script in the page. The escape hatch; rules below. |
| `STORE local "key" "value"` | Write one `localStorage` entry on the current page's origin. |
| `STORE session "key" "value"` | Write one `sessionStorage` entry on the current page's origin. |
| `STORE cookie "name" "value"` | Set one cookie for the current page's host, with path `/`. |

`PRESS` with a single argument treats it as the key: `PRESS Enter` presses Enter on the focused element, even though `Enter` could also parse as a locator. Only when two arguments are present is the first a locator.

`CHECK` and `UNCHECK` are idempotent: a control already in the requested state is left alone. On a native checkbox or radio input, Whirl focuses the input and presses Space, which works whether the input is visible or hidden behind a styled track, as Chakra, Radix, and Headless UI switches hide theirs; a click on such an input would wait out the timeout. On any other control, such as a `role="switch"` button, Whirl clicks it. Both paths verify the resulting state and fail the entry with the expected and actual states when the page did not toggle. A radio button cannot be unchecked; `CHECK` another one in its group. A control hidden with `display: none` cannot take focus, and the error says so; locate the visible control instead.

`FILL` is the default way to enter text: it sets the value and fires one `input` event, which is what a plain field expects. `TYPE` is for the pages that listen for keys instead — segmented one-time-code inputs, masked fields, autocomplete boxes, and rich-text editors ignore a plain fill. It focuses the element and sends a `keydown`, `keypress`, `input`, and `keyup` per character, as Playwright's `pressSequentially` does. `TYPE` does not clear the element first. Use `FILL` unless the page needs key events; a flow that reaches for `TYPE` to slow down typing wants an assertion on the state the page should reach, not a slower `TYPE`.

`STORE` sets browser storage that a page reads to decide what to show — an onboarding flag, a dismissed banner, a feature switch — so a flow can skip a one-time screen without clicking through it or reaching for `EVAL`. `local` names `localStorage` and `session` names `sessionStorage`; both are per origin, so `STORE` runs against the origin of the current page, after a `VISIT` has landed there. `cookie` sets a session cookie for the current page's host with path `/` and no other attributes: no `Secure`, `HttpOnly`, `SameSite`, or expiry. That covers routing flags and feature switches, which is what `STORE` is for; a flow that needs an `HttpOnly` cookie is testing the server and should start from a saved `storage` state instead. A cookie is sent with the next request, so a page whose server reads it needs a second `VISIT`. `cookie` on a page without an http or https origin, such as a `data:` URL, fails the entry. `STORE` writes once and does not retry. A page that reads the key only while loading needs a second `VISIT` to see the value; a page that re-reads it on render picks the value up on its own. Values are strings, as in the browser; write `"true"` or `done`, not a number or a boolean. Section 11's masking applies to `STORE` values like any other line, so a masked variable stays masked in reports.

`SCREENSHOT` never fails the entry, even when it goes wrong: if the capture or the file write fails — a crashed page, an I/O error, or its step timeout expiring — Whirl skips the artifact and records a warning naming the screenshot and the cause, in the console output and in reports, so a missing artifact is always explained. One cap outranks this: an expiring `entry-timeout` fails the entry as usual, whatever line is in flight.

`SNAPSHOT` is retried like an assert through a shim-owned polling loop and uses Playwright's image comparator; V1 exposes no tuning knobs. Whirl captures frames until two consecutive frames are identical, then compares against the baseline at `<flow>.whirl-snapshots/<name>-<browser>-<platform>.png` next to the flow file. Images match only when their dimensions are identical and no pixel differs; a pixel differs when its color distance exceeds the comparator's default per-pixel threshold (0.2 on a 0–1 scale), which absorbs invisible anti-aliasing noise and nothing more. On a mismatch Whirl recaptures and recompares until the step timeout, so a difference that settles late can still pass; a stable mismatch fails the entry when the timeout expires and writes the actual and diff images to the artifacts directory. The platform tag (`linux`, `darwin`, `win32`) keeps baselines rendered on one OS from failing on another; the viewport is not part of the key, because the flow's `viewport` option already pins it. A missing baseline fails the run; `--update-snapshots` writes or refreshes baselines instead of comparing.

`EVAL` is the JavaScript escape hatch — the `css:` of actions — and the one place Whirl runs code it does not read: a script of one or more statements, such as `EVAL "foo(); bar();"`. Whirl runs the script as the body of an async function in the page's main world through Playwright's `page.evaluate`: a script that parses as a single expression runs as `return (expression);`, so its value is the result; any other script runs as written and yields its `return` value, or `undefined` without one. `await` is available in both forms, and a returned Promise is awaited. A syntax error, a thrown exception, a rejected Promise, or the step timeout fails the entry; the action form discards the result. `EVAL` has no target, does not auto-wait, and does not retry: it runs once, after the preceding line completes. `{{name}}` interpolation happens textually before evaluation, so interpolated values become source text — and a value sent into the page escapes the output masking of section 11, so keep secrets out of `EVAL`. A script the page cannot cancel — one that blocks the renderer or never settles — is still bounded: section 12 defines how Whirl enforces timeouts from outside the page.

### 7.1 Popups and tabs

`POPUP payment` waits for a popup from the selected tab and binds it to a name.
Whirl records popup events before the entry's first action, so a popup that opens
before `CLICK` returns is available. Only unnamed popups observed in the current
entry qualify. Multiple qualifying popups fail strictly. A popup that has already
closed can still be named and checked with `tab:payment closed`.

Names match `[A-Za-z_][A-Za-z0-9_-]*`. `main` is reserved for the original tab.
Names last for the flow, including after closure. Duplicate names and references
to names not yet declared are lint errors. Named tabs share their flow's browser
context; separate flow files remain isolated. Setup transfers storage, not tabs.

`TAB` selects a tab without waiting for navigation. `PAGE`, element assertions,
captures, screenshots, and `EVAL` then operate on that tab. `POPUP` and `CLOSE`
never change selection. After a selected tab closes, use `TAB` to select an open
one; ordinary commands on a closed tab fail. `tab:name closed` can inspect a
named tab regardless of which tab is selected. Context options, host filtering,
and dialog handling apply to popups too. Failure screenshots use the selected
tab; a closed selected tab cannot be screenshotted. Traces cover all tabs;
`--video` continues to save the original `main` tab's recording.

```whirl
CLICK role:button "Pay with provider"
POPUP payment
TAB payment
[Asserts]
role:heading "Confirm payment" visible

CLICK role:button Confirm
[Asserts]
tab:payment closed @30s

TAB main
[Asserts]
text:"Payment complete" visible
```

### 7.2 Observing network responses

`RESPONSE order POST /api/orders` names the response to the first request whose
method and URL match. The request must start during the current entry and belong
to the selected tab or one of its frames. Relative URLs resolve against `base`
the same way as `VISIT`. HTTP and HTTPS URLs are matched exactly after URL
normalization, including their query; fragments are ignored. Methods are literal
uppercase names, such as `GET`, `POST`, or `PATCH`.

Whirl observes requests at browser-context level before actions execute. A fast
response, including a popup's initial navigation, remains available after the
triggering action returns. Earlier entries' requests and other tabs' requests
cannot satisfy the command. Put the action and its `RESPONSE` in the same entry.

Selection uses method and URL only. It never skips a failed request or an error
status to choose a later retry. A transport failure fails the command. An HTTP
error status is a response that can be asserted. Each redirect hop is a separate
request; select the final URL to check the final response.

Names match `[A-Za-z_][A-Za-z0-9_-]*` and last for the flow. Duplicate names and
references before declaration are lint errors. Named responses can be asserted
or captured in later entries, including after their tab closes. Selecting the
same method and URL under another name in one entry selects the same first
request. Names and observed requests are not transferred by `setup`.

The command waits for response headers, not a completed body. Its timeout covers
both finding the request and receiving those headers. JSON operations wait for
the body within their own step timeout. Network observation does not change
service-worker settings; worker-originated requests without a page frame are
outside the selected-tab scope. Observation retains at most 10,000 requests per
entry; exceeding this limit fails `RESPONSE` explicitly. JSON bodies are limited
to 1 MiB, with declared content length checked before reading when available.

```whirl
CLICK role:button "Place order"
RESPONSE order POST /api/orders
[Asserts]
response:order status == 201
response:order header:content-type contains application/json
response:order json:/status == paid
text:"Order confirmed" visible
[Captures]
order_id: response:order json:/id
```

### 7.3 Independent HTTP requests

`HTTP` sends a request from Whirl's runtime. It neither sends nor changes browser
cookies, so an API-key assertion cannot accidentally pass using the page's login
session. Only explicitly supplied headers carry credentials. Header values, URLs,
and the optional text body support interpolation; header names are literal and
case-insensitive duplicates are rejected. Set `header:Content-Type application/json`
when sending a JSON body.

`GET` and `HEAD` cannot have a body. `CONNECT`, `TRACE`, and `TRACK` are not
supported. These requests fail the action without contacting the server.

Relative paths resolve against `base`. Only HTTP and HTTPS URLs without embedded
credentials are accepted. `allow-hosts` applies. Redirects and failed requests are
never retried or followed automatically; assert a 3xx, 4xx, or 5xx like any other
response. The step timeout covers receiving the entire response, with a 1 MiB
body limit. HTTP requests are cancelled if their flow closes. Browser CORS rules
do not apply, and these runtime requests do not appear in the browser's HAR.
The body limit covers decoded response bytes. A response without a body, such as
`HEAD`, can advertise a larger `Content-Length` without failing the limit.

Names share the `RESPONSE` namespace. The same `response:name` assertions and
captures work for either command. The file still begins with `VISIT`.

```whirl
HTTP account GET /api/account header:Authorization "Bearer {{env.API_KEY}}"
[Asserts]
response:account status == 200
response:account json:/name == Ada
```

## 8. PAGE

```
PAGE value
PAGE matches /regex/
```

`PAGE` asserts where the browser landed. It is the analogue of Hurl's `HTTP 200` line and is retried like an assert.

- If the value starts with `/` and contains no `?`, it must equal the current URL's path (query and fragment excluded). If it contains a `?`, it must equal the path plus query (fragment excluded).
- Otherwise the value must equal the full current URL.
- `PAGE matches /re/` tests the full current URL against the regex.

`PAGE` never navigates. For other comparisons, use `url` asserts.

## 9. Asserts

An `[Asserts]` section holds one check per line. Checks run in order. Page and element checks retry until they pass or the step timeout expires. Response checks wait for immutable data and fail on a mismatch. The first failing check fails the entry.

```
assert  := subject check
subject := locator | "url" | "title"
```

### 9.1 Element state checks

`visible`, `hidden`, `enabled`, `disabled`, `checked`, `unchecked`, `focused`.

`hidden` passes when the element is not visible, including when it does not exist. All others require the element to exist.

`tab:name closed` retries until the named tab closes. An unknown name never counts as closed.

### 9.2 Element value checks

| Check | Subject value |
| --- | --- |
| `text op value-or-regex` | Normalized text content |
| `value op value-or-regex` | Current value of an input, textarea, or select |
| `attr:NAME op value-or-regex` | Value of attribute `NAME`; `!=` also passes when the attribute is absent |
| `count numop number` | Number of matching elements |

`NAME` is an attribute name: a letter or underscore, then letters, digits, underscores, or hyphens — so `attr:aria-expanded` and `attr:data-state` are valid. The formal grammar (section 17) calls this production `attr-name`.

### 9.3 Page checks

| Check | Subject value |
| --- | --- |
| `url op value-or-regex` | Full current URL |
| `title op value-or-regex` | Document title |

### 9.4 Operators

- String operators (`op`): `==`, `!=`, `contains`, `matches /re/`.
- Count operators (`numop`): `==`, `!=`, `<`, `<=`, `>`, `>=`.

### 9.5 Response checks

`response:name status numop number` compares the HTTP status using the same
numeric operators as `count`. `response:name header:NAME str-check` checks a
case-insensitive header name. `response:name json:POINTER str-check` checks a
JSON value using JSON Pointer: `/items/0/id` selects an array item's field, `~1`
escapes `/`, `~0` escapes `~`, and `json:""` selects the whole body. Header names
follow `attr-name`. Header names and pointers support value interpolation.

Strings are compared as-is, without whitespace normalization. Other JSON values
are serialized as compact JSON (`true`, `null`, `42`, arrays, or objects). Missing
headers, missing pointer targets, invalid pointers, and malformed JSON fail even
with `!=`. All checks for a name examine the same response. Once its data is
available, a mismatch fails immediately: an immutable response is not retried.
A successful status alone does not prove streaming output or a background job
completed; assert the user's result separately.

## 10. Captures

A `[Captures]` section extracts values into variables for later entries.

```
capture   := name ":" source ["regex" /re/]
source    := locator extractor | "url" | "title" | "eval" value | "response:" name response-field
extractor := "text" | "value" | "count" | "attr:" NAME
```

- `name` matches `[A-Za-z_][A-Za-z0-9_]*`.
- The optional `regex` filter applies the pattern to the extracted string and stores capture group 1 (the whole match if there is no group). No match fails the entry.
- Extraction waits like an assert: `text`, `value`, and `attr:` wait for the locator to resolve to exactly one element, up to the step timeout. Once the element resolves, an absent attribute fails the entry — it does not wait further and does not become an empty value. `count` never waits: it records the current number of matches immediately, and zero is a valid result; assert a `count` first when the flow must wait for elements to appear.
- A `response:name` source extracts `status`, `header:NAME`, or `json:POINTER` using the response-check rules above. The existing `regex` filter and interpolation work on the extracted string.
- An `eval` source runs a script under the rules of section 7 and stores the result. Whirl owns the result contract, independent of Playwright's transport: a string is stored as-is; `null`, booleans, finite numbers, arrays, and plain objects that recursively contain only those values are stored as compact JSON; anything else — `undefined`, non-finite numbers, `BigInt`, functions, symbols, cyclic structures, and browser objects — fails the entry.
- A capture that reuses a name overwrites it.

```whirl
[Captures]
order_id: testid:confirmation text regex /Order #(\w+)/
cart_url: url
```

## 11. Variables

`{{name}}` interpolates a variable inside any value.

Sources, later entries overriding earlier ones:

1. `--variables-file` entries (`name=value` lines),
2. `--var name=value` flags,
3. captures, as the file runs.

Option values (section 5) resolve once, when the file starts, before the browser context is created. Only `--variables-file` entries, `--var` flags, and `{{env.NAME}}` are available there — captures do not exist yet, and a later capture never rewrites an option. A reference to an undefined variable in an option value fails the file before any entry runs and is reported as a failed run (exit 1).

`{{setup.NAME}}` reads a capture named `NAME` taken by the file's `setup` flow (section 5). It is available everywhere `{{name}}` is, including option values, and it is read-only: the file's own captures live in the plain namespace and never shadow it. A reference to a name the setup flow does not capture is a lint error, so `whirl check` catches it without a browser. A secret the setup flow masked stays masked in the dependent's output.

`{{env.NAME}}` reads the environment variable `NAME` at run time. This is the intended path for secrets; secret values never belong in `.whirl` files. A reference to an undefined variable or unset environment variable fails the step.

Whirl masks every value sourced from `env.*` in the textual output it generates: console failure details, rendered step text, the JSON and JUnit reports, and trace step titles. Browser-recorded artifacts — screenshots, video, HAR files, and saved storage state — can still contain secrets the flow typed or received. Treat the artifacts directory and storage-state files as sensitive, and prefer dedicated test credentials.

## 12. Execution model

- **Isolation.** Each file runs in a fresh browser context with an initial page named `main` and any popups it opens. Without the `storage` option the context starts empty; with it, the context starts from the saved storage state. Files never share live state either way.
- **Order.** Entries run top to bottom. Within an entry: actions, then `PAGE`, then asserts, then captures.
- **Failure.** The first failing step fails the entry, and a failed entry stops its file; remaining entries in that file are skipped and reported as skipped. Other files still run. On failure Whirl saves a full-page screenshot and, with `--trace`, a Playwright trace to the artifacts directory.
- **Navigation.** `VISIT` completes when the new document reaches `DOMContentLoaded`: the HTML is parsed and its synchronous scripts have run. It does not wait for the `load` event, because images, fonts, iframes, and media hold `load` open for reasons a flow never asserted, and every later line waits for what it needs anyway: actions wait for their element to be actionable, asserts and `PAGE` retry. A page that only becomes usable after `load` needs an assert on that state before an `EVAL` or `SCREENSHOT`, which run once without waiting.
- **Timeouts.** Each action, PAGE, assert, and capture line gets the step timeout (`step-timeout` option, default 10s); `VISIT` gets the navigation timeout (`nav-timeout` option, default 30s). A trailing `@duration` on any such line overrides its own budget: `CLICK "Generate report" @60s`. The optional `entry-timeout` option caps an entry's total time across all of its lines; when it expires, the in-flight step fails with an entry-timeout error. An entry without one is still bounded by its per-step timeouts. The suffix must be bare: a line’s final bare token of the form `@duration` is always its timeout, and a quoted `"@60s"` is an ordinary value. Timeouts are enforced from outside the page, so they hold even when the page cannot respond — an `EVAL` script blocking the renderer or returning a Promise that never settles. When a timed-out step cannot be cancelled cleanly, Whirl closes that flow's browser context; if closing also stalls, it terminates and restarts only that worker's shim process. Either way the flow fails and reports normally, and other files are unaffected.
- **Setup.** Files with a `setup` option run after their setup flows. Whirl first runs every distinct setup flow named by the inputs, once each and in parallel like any files, then runs the remaining files, each starting from its setup flow's saved state with the setup flow's captures as `{{setup.name}}`. A setup flow that is also an input runs once, as the setup. A failed setup flow reports normally, and each of its dependents reports a `[setup]` failure naming the setup flow and its first failing step, without opening a browser. Setup flows are one level deep.
- **Parallelism.** Files run in parallel across worker slots (`--jobs`, default: logical CPU count). A single file is never parallelized.
- **Dialogs.** `alert`, `confirm`, and `prompt` dialogs are auto-dismissed by default. The `dialogs: accept` option auto-accepts them instead.

## 13. Command line

```
whirl [OPTIONS] <PATH>...        Run files; directories recurse to *.whirl
whirl check [--json] <PATH>...            Parse and lint only; nothing runs
whirl install [BROWSER]...       Provision the shim bundle and selected browsers
whirl doctor [--browser NAME]    Check the runtime and browser; print repair commands
whirl show-trace <PATH>          Open a trace with the private runtime
whirl fmt [--check] <PATH>...    Rewrite files to canonical form
whirl report <REPORT> --html <PATH>  Generate HTML from saved results
```

`whirl install chromium` provisions only Chromium; any combination of `chromium`, `firefox`, and `webkit` may be named. Without names, all three engines are provisioned. `whirl doctor` checks the selected Node runtime, shim protocol, Playwright version, and a real headless browser launch (Chromium by default). It installs nothing, finishes within 30 seconds, exits 0 when ready or 3 when diagnosis fails, and prints repair commands. On Linux, a failed launch also prints the private-runtime command for installing system libraries. Unsupported browser names are usage errors.

`whirl fmt` rewrites files to the canonical form: single spaces between tokens, quotes only where a value requires them, and one blank line between entries. `--check` writes nothing and exits with code 1 when any file would change.

| Flag | Meaning |
| --- | --- |
| `--rerun-failed REPORT` | Run only failed or errored files from a JSON report; replaces PATH arguments |
| `--base URL` | Override the `base` option |
| `--browser NAME` | Override the `browser` option |
| `--step-timeout DURATION` | Override the `step-timeout` option |
| `--headed` | Run with a visible browser window |
| `--jobs N` | Worker slots for parallel files |
| `--var k=v` | Define a variable (repeatable) |
| `--variables-file PATH` | Load variables from a file |
| `--artifacts DIR` | Artifact output directory (default `whirl-artifacts/`) |
| `--trace` | Record a Playwright trace per file; the trace is saved only when the file fails |
| `--report-junit PATH` | Write a JUnit XML report |
| `--report-json PATH` | Write a JSON report |
| `--report-html PATH` | Write a standalone HTML report with embedded recordings and screenshots |
| `--report-metadata PATH` | Read author-written HTML report context from JSON; requires `--report-html` |
| `--fail-fast` | Stop scheduling new files after the first failure |
| `--update-snapshots` | Write or refresh SNAPSHOT baselines instead of comparing |
| `--video` | Record a .webm video of each file's run into the artifacts directory |
| `--har` | Record a .har network log per file into the artifacts directory |
| `--storage PATH` | Override the storage option |
| `--save-storage PATH` | Write the final storage state after a successful run (single file only) |
| `--entry-timeout DURATION` | Override the entry-timeout option |
| `--user-agent UA` | Override the user-agent option with `chrome`, `firefox`, `safari`, or a literal string |

Exit codes:

| Code | Meaning |
| --- | --- |
| 0 | All files passed |
| 1 | The command's negative result: a failed entry (run), formatting drift (`fmt --check`) |
| 2 | Parse or lint error |
| 3 | Runtime error (browser or shim failure) |
| 4 | Usage error |

`--rerun-failed` reads a version 1 report with an absolute `workingDirectory`. Relative file paths resolve against that directory, even when the report is moved. Each selected file runs from the beginning, including its setup. Existing CLI overrides and secrets must be supplied again; a report is not executable configuration. An unsupported or malformed report is a usage error. A report with no failed or errored files exits 0 with a message and launches no browser.

If the input paths select no `.whirl` files, run, `check`, and `fmt` report a usage error (exit 4).

Whirl parses and lints every input file before it launches any browser: a parse or lint error anywhere stops the invocation with exit 2 and nothing runs. When one invocation hits several categories, the highest applicable code wins — a usage error (4) is detected before parsing and preempts everything, and within a run a runtime error (3) outranks failed entries (1), which outrank 0.

## 14. Output and reports

- Action failures retain Playwright's actionability log, including the locator being awaited and any element blocking interaction. Output masking applies to the log. Each trace artifact includes a shell-quoted `whirl show-trace -- <PATH>` command. The viewer uses Whirl's private runtime; a missing trace is a usage error, and a viewer launch failure is a runtime error.
- Default console output: one line per file with pass/fail and duration, then a failure detail block per failed entry: file, line, the failing step, expected versus actual, and the artifact paths.
- The JUnit report maps one file to one test suite and one entry to one test case. An entry is named by the comment line directly above it — the nearest comment line with no other content line between it and the entry's first action (`# Log in.`) — falling back to its first action line and line number. A failure before the first entry — option resolution, storage loading, or browser launch — reports as a synthetic test case named `[setup]` in that file's suite: an `<error>` for runtime errors, a `<failure>` otherwise. The JSON report carries the same synthetic entry, and masking applies to it like any other output.
- The version 1 JSON report includes `workingDirectory`, `whirlVersion`, `platform`, and `architecture`. Files whose browser context starts include `runtime`: browser engine, viewport, the actual user agent string (with section 11's masking), and actual browser, Node, and Playwright versions. Unavailable user agent and version fields are null. Each step error has a stable `code`, separate from its human-readable message. Additive fields do not change the report version; readers must ignore unknown fields. See [machine-readable output](docs/engineering/machine-output.md) for schemas and codes.
- The JSON report is the machine-readable superset: per-step timing, captures (with values sourced from `env.*` masked), and artifact paths.
- Artifacts: each flow writes to `<artifacts>/<flow path without the .whirl extension>/`. The mirrored path is the flow file's canonical path (made absolute, symlinks resolved) relative to the current working directory, so parallel flows never collide, and overlapping inputs or symlinked duplicates of one file resolve to one flow, run once, and write to one directory. A flow outside the working directory writes to `<file stem>-<hash>/` instead, where `<hash>` is the first 16 hex digits of the SHA-256 of the canonical path, so absolute paths and `..` segments never escape the artifacts directory. Because all inputs are known before the run starts, Whirl verifies that no two flows map to the same directory; a collision is a runtime error. Names inside are fixed: screenshots by their given name, `snapshot-<name>-actual.png` and `snapshot-<name>-diff.png`, `failure.png`, `trace.zip`, `video.webm`, and `network.har`. Duplicate `SCREENSHOT` names or duplicate `SNAPSHOT` names within one flow are a lint error; the two keywords have separate name spaces, because their artifact files never collide.

### 14.1 HTML reports

`--report-html evidence.html` writes a standalone HTML file after the run, including failed runs. It renders the same masked results as the other reports: file and entry outcomes, steps and timings, failures with expected and actual values, captures, warnings, blocked hosts, and browser environment details. The report embeds available PNG screenshots and, with `--video`, WebM recordings from the current run. It opens offline and can be moved without its artifacts directory. Trace and HAR paths are shown as text; these files are not embedded.

Test outcomes and media availability are separate. A missing or unreadable recording never changes a passed test to failed, and a recording never makes a failed test pass. The report explains absent or invalid media. Whirl reads only listed media artifacts inside their flow's artifact directory. A media read failure during embedding or an output write failure is a runtime error (exit 3). HTML output is written to a temporary file and replaces its destination only after generation succeeds. Its parent directory must exist. The HTML destination must not conflict with an input, another requested report, or a recorded artifact.

The HTML contains no executable scripts or remote resources. Text is escaped, including flow paths, comments, errors, captures, and author metadata. Embedded browser images and recordings have the same secret exposure as their original artifacts (section 11); textual masking does not redact their pixels.

`--report-metadata context.json` supplies optional plain-text presentation fields:

```json
{
  "title": "Critical browser evidence",
  "description": "Local services, development accounts, and simulated model responses.",
  "files": {
    "flows/login.whirl": {
      "title": "Account access",
      "description": "Sign in and verify that the account page is available."
    }
  }
}
```

All fields are optional. Without a title, the report uses `Browser test report` and each flow uses its path. `files` keys resolve relative to the metadata file; Whirl matches canonical paths, including symlinks. Paths must name existing files; duplicate canonical paths, invalid JSON, wrong field types, and unknown fields are usage errors before execution. Metadata for unselected flows is ignored. Metadata is not interpolated or executed and cannot change results. When JSON output is also requested, its optional `metadata` field contains the author context with selected file keys matching the report's file paths.

### 14.2 HTML from saved results

`whirl report report.json --html evidence.html` renders a version 1 JSON report without parsing flows, resolving variables, installing a runtime, or starting browsers. It preserves the recorded results, original Whirl version, platform, browser environment, and run timestamps. It never uses the JSON file's modification time as execution time.

`--metadata context.json` replaces the saved author context. It uses the section 14.1 schema, but `files` keys match recorded `files[].path` strings exactly. Source files do not need to exist. Unmatched keys are ignored. Invalid author metadata remains a usage error.

Relative artifact paths resolve against the report's absolute `workingDirectory`, even if the JSON is moved. `--working-directory DIR` selects another directory for relative artifact paths, such as a copied project tree. Absolute artifact paths remain absolute. Keep each run's JSON and artifacts together in a distinct output location if later runs would overwrite the media. The command embeds the available files at their recorded paths; source hashes do not authenticate artifacts.

A successful render exits 0, including when the saved tests failed or media is missing. Invalid or unsupported input is a usage error (exit 4); an output failure is a runtime error (exit 3). The protection and atomic-write rules in section 14.1 also apply. The command never changes the input JSON. Older version 1 reports with `workingDirectory` are supported; absent timestamps, hashes, roles, and recording settings are shown as not recorded.

### 14.3 Run records

New JSON reports include `startedAt` and `finishedAt` as UTC RFC 3339 timestamps for the run and each reported file. Run timestamps enclose preparation, scheduled flows, and cleanup. File timestamps enclose each scheduled attempt, including failures before browser startup. Files not scheduled because of `--fail-fast` remain absent. `durationMs` continues to use the monotonic clock; wall-clock adjustments can affect timestamps.

Each file includes `sourceSha256`: the lowercase SHA-256 of the exact UTF-8 bytes parsed, including comments and line endings, before interpolation. Whirl retains this hash even if the source changes during execution. The hash covers only that flow file, not external fixtures, variable values, browser artifacts, or application code.

Each file includes `roles` with independent `requested` and `setup` booleans. `requested` means the file was selected by the invocation's input paths or `--rerun-failed`. `setup` means another selected flow names it through the `setup` option. Both can be true; the flow still runs once as setup. A synthetic `[setup]` entry in a requested scenario does not make that scenario a setup flow. HTML labels these roles and shows both counts; a flow with both roles appears in both counts. Status totals count each reported file once.

The run's `videoRequested` boolean distinguishes an absent requested recording from a run made without `--video`. These fields are additive within report version 1. Consumers must parse timestamps to compare execution times and must ignore unknown fields.

## 15. Architecture

Rust source, configuration, and project setup follow the [Brynary Rust Style Guide](https://github.com/brynary/rust-style-guide). TypeScript source and language tooling follow the [Brynary TypeScript Style Guide](https://github.com/brynary/typescript-style-guide) for language-level and authoring conventions. The shim targets the pinned private Node runtime specified here, so the TypeScript guide's Bun-specific runtime, API, package-management, and test-runner policies do not apply. This specification and accepted Whirl ADRs take precedence over both guides.

- `whirl` is a single Rust binary containing the parser, the runner, the reporters, and the shim manager.
- Whirl drives browsers through a thin Node shim that Whirl owns: a small, stable JSON API over stdio pipes, shaped like Whirl's closed vocabulary and implemented on the Playwright library. The Rust binary launches the shim as a child process. Whirl does not reimplement browser automation and does not speak CDP or Playwright's internal driver protocol, so it inherits Playwright's auto-waiting, retrying assertions, locator engine, tracing, and three browser engines — and Playwright upgrades stay internal to the shim.
- `whirl install` downloads the pinned shim bundle (a private Node runtime, the shim, and the `@playwright/test` package) and the browser builds. Users do not need Node installed. Each Whirl release pins exactly one Playwright version.
- The shim bundles `@playwright/test` and drives its standalone `expect` for retried checks: they compile to Playwright's web-first assertions (`toHaveText`, `toHaveCount`, `toHaveURL`, ...) where a usable one exists, so retry timing and regex semantics match Playwright's. Checks with no usable web-first assertion — count comparators other than `==`, negated attribute checks, and `SNAPSHOT`, whose `toHaveScreenshot` runs only inside Playwright's test runner — run as shim-owned poll loops with the same step timeout; snapshot comparison uses Playwright's image comparator with the defaults of section 7.

## 16. Errors

- **JSON diagnostics.** `whirl check --json` writes one version 1 JSON document to stdout, containing `exitCode` and `diagnostics`, with no diagnostic text on stderr. Each diagnostic includes a stable code, severity, path, line, column, length, message, and expected alternatives. Positions are 1-based Unicode character positions; locations unavailable for input or I/O errors are null. CLI argument syntax errors still use the ordinary usage message.
- **Parse errors** (exit 2) are reported with file, line, column, a caret under the offending token, and the expected alternatives. `whirl check` surfaces them without launching a browser. Lint warnings do not change the exit code. Whirl warns about a capture that is never used, and about a `count >= 1` assert directly followed by a check on the same locator, only when the following check requires at least one element. A `hidden` check or a count comparison that accepts zero does not make the presence check redundant.
- **Test failures** (exit 1) report the failing step the same way, plus expected versus actual and the artifacts.
- **Runtime errors** (exit 3) cover shim crashes, missing browsers, and similar environmental failures.

## 17. Grammar

```ebnf
file       = [ options ] , entry , { entry } ;
options    = "[Options]" , { option-line } ;
option-line= key , ":" , value , { value } ;

entry      = action , { action } , [ page ] , [ asserts ] , [ captures ] ;

action     = action-body , [ step-timeout ] ;
action-body = "VISIT" , value
           | "HTTP" , artifact-name , http-method , value , { http-option }
           | "RESPONSE" , artifact-name , http-method , value
           | ( "POPUP" | "TAB" | "CLOSE" ) , artifact-name
           | "CLICK" , locator
           | "DBLCLICK" , locator
           | "FILL" , locator , value
           | "TYPE" , locator , value
           | "PRESS" , [ locator ] , value
           | "CHECK" , locator
           | "UNCHECK" , locator
           | "SELECT" , locator , value
           | "HOVER" , locator
           | "UPLOAD" , locator , "file:" , value
           | "SCREENSHOT" , artifact-name
           | "SNAPSHOT" , artifact-name
           | "EVAL" , value
           | "STORE" , ( "local" | "session" | "cookie" ) , value , value ;

page       = "PAGE" , ( value | "matches" , regex ) , [ step-timeout ] ;

asserts    = "[Asserts]" , { assert } ;
assert     = assert-body , [ step-timeout ] ;
assert-body = "response:" , artifact-name , "status" , numop , number
           | "response:" , artifact-name , ( "header:" , value | "json:" , value ) , str-check
           | "tab:" , artifact-name , "closed"
            | locator , state-check
           | locator , value-check
           | locator , "count" , numop , number
           | ( "url" | "title" ) , str-check ;
state-check= "visible" | "hidden" | "enabled" | "disabled"
           | "checked" | "unchecked" | "focused" ;
value-check= ( "text" | "value" | "attr:" attr-name ) , str-check ;
str-check  = ( "==" | "!=" | "contains" ) , value
           | "matches" , regex ;
numop      = "==" | "!=" | "<" | "<=" | ">" | ">=" ;

captures   = "[Captures]" , { capture } ;
capture    = name , ":" , source , [ "regex" , regex ] , [ step-timeout ] ;
source     = locator , extractor | "url" | "title" | "eval" , value
           | "response:" , artifact-name , response-field ;
response-field = "status" | "header:" , value | "json:" , value ;
http-method = uppercase-letter , { uppercase-letter } ;
http-option = "header:" , attr-name , value | "body:" , value ; (* at most one body *)
extractor  = "text" | "value" | "count" | "attr:" , attr-name ;

locator    = segment , { ">>" , segment } ;
segment    = ( "role:" | "role~:" ) , name , [ value ]
           | ( "label" | "placeholder" | "text" | "alt"
             | "title" ) , [ "~" ] , ":" , value
           | ( "testid:" | "css:" | "frame:" ) , value
           | "nth:" , number   (* N >= 1; never the first segment *)
           | value ;             (* default engine; actions only — see 6.1 *)

step-timeout = "@" , duration ;
value      = quoted-string | bare-token ;
name       = letter-or-underscore , { letter-digit-underscore } ;
artifact-name = letter-or-underscore , { letter-digit-underscore | "-" } ;
attr-name  = letter-or-underscore , { letter-digit-underscore | "-" } ;
regex      = "/" , pattern , "/" , [ flags ] ;
```

Comments and blank lines may appear between any two lines and are not part of the grammar.

## 18. Non-goals and deferred features

Permanent non-goals — these keep the format Hurl-grade:

- Conditionals, loops, functions, includes, or user-defined keywords.
- Arbitrary JavaScript woven into the language. `EVAL` (section 7) is the single, explicit escape hatch: Whirl passes its script to the browser without reading it, and offers no way to branch on the result.
- A programming language of Whirl's own. The format has no Whirl-native expressions, conditionals, or control flow; when a flow outgrows Whirl, the answer is Playwright itself.

Deferred beyond V1 (candidate V2 features, not promised):

- Network stubbing and request-body or request-count assertions.
- Per-entry `[Options]` overrides and mobile device emulation.
- An LLM-as-judge assertion (a `JUDGE` keyword with an explicit model option and advisory rather than hard-failing verdicts).
