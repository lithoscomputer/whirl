# Whirl Specification

Status: v2, implemented and not yet released.
Date: 2026-09-26

Whirl is a command-line tool that runs web UI tests written in plain text files. It is to browser flows what [Hurl](https://hurl.dev/) is to HTTP: a tight, closed, file-based format that is readable, diffable, and easy to generate. The `whirl` binary is written in Rust and drives real browsers through Playwright.

## 1. Design principles

1. **Closed vocabulary.** The language has a fixed set of actions, subjects, filters, and predicates. There are no conditionals, loops, functions, or user-defined keywords. A flow that needs branching is two files. `ACT` (section 7.4), `GOAL` (section 7.7), `ai:` targets (section 6.3), `EXTRACT` (section 7.6), and `JUDGE` (section 9.8) are fixed keywords too, but what a language model chooses for them can change from run to run, so a flow asserts the result it expects. The AI cache (section 12.1) records each choice, so later runs replay it.
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
ASSERT role:heading "Welcome back" visible
ASSERT testid:user-menu text == Alice

# Find a product.
FILL placeholder:"Search products" widget
PRESS Enter
ASSERT url contains "q=widget"
ASSERT testid:result-card count >= 1
CAPTURE first_product: testid:result-card >> nth:0 >> role:link attr:href

# Add it to the cart.
VISIT {{first_product}}
CLICK "Add to cart"
ASSERT testid:cart-badge text == 1
ASSERT role:alert text contains "Added to cart"
```

Run it:

```console
$ whirl flows/checkout.whirl
$ whirl --report-junit report.xml flows/
```

## 3. Files

- Extension: `.whirl`. Encoding: UTF-8. Line endings: LF or CRLF.
- The format is line-oriented. Each check, capture, and option is one line. Most
  actions are one line. An independent `HTTP` request can also own the header
  and body lines defined in section 7.3; the complete request is one action.
  A `SNAPSHOT` owns the comparison option lines below it (section 7).
- `#` starts a comment. A comment runs to the end of the line. A `#` inside a
  quoted string, a regex literal, a JSON body, or a fenced HTTP body is literal
  text.
- Blank lines are ignored outside HTTP bodies.
- Keywords (`VISIT`, `PAGE`, `ASSERT`, checks, prefixes) are case-sensitive.

### 3.1 Values

A **value** is written in one of two forms:

- **Quoted**: `"..."` with backslash escapes `\"`, `\\`, `\n`, `\t`, and `\u{XXXX}`.
- **Bare**: a single token with no whitespace, no `"`, and no `#`. Bare and quoted forms are interchangeable, with five reservations:
  - A line's final bare token of the form `@duration` always parses as the step timeout (section 12), so that value must stay quoted (`"@60s"`).
  - In a typed comparison (section 9.6), a bare typed literal and its quoted form differ: `42` is a number and `"42"` is a string.
  - In `ASSERT` and `CAPTURE` lines, a bare role name cannot be a subject, state, or predicate keyword such as `text` or `visible`, because that word ends the locator. Quote it: `role:button "visible" visible`.
  - In `DRAG` and `SCROLL`, a bare `to` is a keyword, and in `SCROLL` so is a bare `down`, `up`, `left`, or `right` at the end of the line. Quote them to match the text: `DRAG "to" to testid:done`, `SCROLL "down"`.

  - In `snapshot-mask`, only bare `none` clears the mask list.

A token can join bare and quoted parts, as in `label:"First name"`; the parts form one value.

`whirl fmt` never removes quotes whose removal would change the parse or the type.

Values support variable interpolation with `{{name}}` (section 11). Write `\{{` for a literal `{{`; `\{` also writes a literal `{`.

Other literal forms:

- **Regex**: `/pattern/` with optional flags `i`, `s`, `m` (for example `/Order #\w+/i`). Escape a literal slash as `\/`. Patterns use ECMAScript regex syntax in Unicode mode: the `u` flag is always on, for checks, filters, and `PAGE matches`. A pattern that is invalid in Unicode mode, such as one that escapes `-` outside a character class, is a parse error.
- **Number**: the JSON number grammar (RFC 8259 section 6): an optional minus sign, an integer part without leading zeros, an optional fraction, and an optional exponent, such as `-12`, `3.14`, or `1e6`. `007`, `+1`, and `.5` are not numbers; as bare values they are strings.
- **Index**: an integer, such as `0`, `2`, or `-1`, for `nth:` and the `nth` filter. Indexes start at 0, and a negative index counts from the end.
- **Typed literal**: in a typed comparison (section 9.6), a bare number, `true`, `false`, or `null`.
- **Bytes literal**: `hex,DIGITS;` or `base64,TEXT;`, such as `hex,89504e47;`. In a typed comparison, a bare bytes literal is bytes (section 9.6), and one that does not decode, such as `hex,zz;`, is a parse error. Quote it to compare the text.
- **JSON literal**: in the expected-value position of a check, a value that starts with `[` or `{` is a JSON array or object. It must end on the same line, and it may contain spaces. `{{name}}` works inside it as in HTTP JSON bodies (sections 7.3 and 11).
- **Duration**: an integer with unit `ms` or `s` (for example `500ms`, `10s`).
- **Viewport**: `WIDTHxHEIGHT` in CSS pixels (for example `1280x800`).
- **Percent**: a number from 0 to 100 with a `%` suffix, such as `50%` or `33.5%`, for `SCROLL` and `snapshot-max-diff`.

## 4. File structure

```
file          := [Options-section] entry+
entry         := browser-entry | http-entry
browser-entry := action+ [PAGE-line] check-line*
http-entry    := HTTP-request check-line*
check-line    := ASSERT-line | JUDGE-line | CAPTURE-line
```

- The optional `[Options]` section appears once, before the first entry.
- A **browser entry** is one or more browser action lines, then an optional
  `PAGE` line, then zero or more check lines, in that order. A check line is
  an `ASSERT` line (section 9), a `JUDGE` line (section 9.8), or a `CAPTURE`
  line (section 10).
- An **HTTP entry** is one independent `HTTP` request, then zero or more
  check lines for that response. It has no `PAGE` line and cannot contain
  browser actions or a second request.
- In a browser entry, an action line after a `PAGE` line or a check line
  starts a new entry. Consecutive browser
  action lines belong to one entry. An `HTTP` action always starts its own
  entry, and the next action starts another entry. An entry is Whirl's unit of
  verification, timeout scope, and reporting. Blank lines and comments never
  split an entry.
- Check lines run in the order written. A `CAPTURE` can come before an
  `ASSERT` that reads its value.
- `HTTP` entries can appear before the first `VISIT`, and a file can contain
  only HTTP entries. The first browser entry must start with `VISIT`, after
  any `MOCK` lines (section 7.5), because no page exists yet. After it, later
  browser entries can start with any browser action.

### 4.1 Removed sections

Earlier versions marked checks with an `[Asserts]` section and captures with
a `[Captures]` section: a header line, then one check or capture per line
without a keyword. A run and `whirl check` reject a file with such a section
with the parse error `sections-removed`. `whirl fmt` still rewrites each
section as `ASSERT` and `CAPTURE` lines, with the same meaning: `[Asserts]`
comes before `[Captures]`, and each entry has at most one of each. A file that
uses both sections and check lines is the parse error `mixed-check-syntax`,
and `whirl fmt` cannot rewrite it.

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
| `model` | `provider/model` | none | The language model that `ACT`, `GOAL`, `ai:` targets, `EXTRACT`, and `JUDGE` ask (sections 6.3, 7.4, 7.6, 7.7, 9.8) |
| `snapshot-mask` | explicit locator or `none` | no masks | Mask matching elements in every `SNAPSHOT`; repeated lines form a list |
| `snapshot-max-diff` | pixel count or percentage | `0` | Maximum different pixels allowed in a `SNAPSHOT` |
| `snapshot-pixel-threshold` | number from 0 to 1 | `0.2` | Color distance above which a pixel counts as different |

`allow-hosts` takes one or more host globs (`allow-hosts: example.com *.example.com`). Globs match the request's hostname only — scheme and port are ignored — and `*.example.com` does not match the apex `example.com`; list both to cover both. The `base` host is always allowed. Whirl aborts requests to any other host, including fetch/XHR, WebSockets, and subresources, and the reports list every blocked host. Service workers are disabled when `allow-hosts` is set, because they can bypass request routing. IP-literal hosts match textually; `data:` and `blob:` URLs have no host and are always allowed. Without the option, all hosts are allowed.

`storage` names a Playwright storageState JSON file, resolved relative to the `.whirl` file. Each browser context starts from that saved state (cookies and local storage) instead of empty, so flows can skip UI login. Produce the file with `--save-storage`, which writes the final context state of a successful run — typically of a dedicated login flow.

`setup` names another `.whirl` file, resolved relative to this one, whose final state this file starts from: Whirl runs the setup flow first, in its own context, saves that context's cookies and storage, and starts this file's context from the saved state, the way `storage` would. The two options cannot be combined. The setup flow is an ordinary flow with its own options, so it runs and debugs on its own, and it may not name a `setup` of its own. It can use HTTP entries before its first `VISIT`, or contain only HTTP entries. Every file that names the same setup flow in one invocation shares one run of it: ten flows that need a signed-in session sign in once. The saved state lives only for the invocation. The setup flow's captures are readable in the dependent file as `{{setup.name}}` (section 11). When the setup flow fails, its dependents do not start and each reports the failure as its `[setup]` case (section 12). The path must be literal: it is resolved before any variable exists.

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

`model` names a language model in the form `provider/model`, such as
`anthropic/claude-sonnet-5` or `openai/gpt-5.6-luna`, from the model catalog
built into Whirl. Credentials come from the provider's usual environment
variable, such as `ANTHROPIC_API_KEY` or `OPENAI_API_KEY`. A file that uses
`ACT` without `model` is a lint error. Without `WHIRL_LLM_ENDPOINT` (section
13), `whirl check` also reports a literal `model` that the catalog cannot route.

Unknown keys are a parse error. When section 13 defines a corresponding command-line flag, that flag overrides the file option.

## 6. Locators

A locator selects one element (or, for `count`, a set of elements). It is a chain of one or more segments joined by `>>`. Each later segment searches inside the result of the chain so far. `nth:` may not be the first segment. Its index starts at 0, and a negative index counts from the end: `nth:0` is the first match and `nth:-1` is the last.

```
locator := segment (">>" segment)*
segment := prefix ":" value [value]   # second value: role name only
         | "nth:" index
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
| `nth:N` | `.nth(N)` — 0-based position; a negative `N` counts from the end |
| `ai:"description"` | The one element that a language model finds for the description (section 6.3) |

Text matching is exact (after whitespace normalization). For partial or pattern matching, assert on the element instead (`text contains`, `text matches`, `text startsWith`).

Every text-matching prefix has a substring variant marked with `~` — `role~:`, `label~:`, `placeholder~:`, `text~:`, `alt~:`, `title~:` — which matches by case-insensitive substring, Playwright's default matching. So `text~:"Added"` matches "Added to cart". `testid:` and `css:` have no `~` form, and the unprefixed default engine stays exact.

An unprefixed value in locator position selects a default engine: `label:` for form actions (`FILL`, `SELECT`, `CHECK`, `UNCHECK`, `UPLOAD`, and `PRESS` with a target), and `text:` for pointer actions (`CLICK`, `RIGHTCLICK`, `MIDDLECLICK`, `DBLCLICK`, `HOVER`, `DRAG`, `SCROLL`) and `DROP` — buttons and links have no label; their accessible name is their text, and a drop zone says what it takes, such as "Drop files here". So `FILL "Email" alice@example.com` fills the input labeled Email, and `CLICK "Add to cart"` clicks the element with that exact text. Prefixes stay available everywhere for precision. Default engines exist only in actions: in `ASSERT` and `CAPTURE` lines every segment must carry a prefix (or be `nth:`), and an unprefixed value there is a parse error. The scope of `ACT` (section 7.4), the target of `SNAPSHOT`, and `snapshot-mask` (section 7) follow the same rule.

`frame:` works in actions, asserts, and captures. It may follow an element scope or another frame. An immediately following `nth:N` selects the iframe before entering it. A frame must be followed by an element segment; use `css:` to check the iframe element itself. Nested and cross-origin frames use the same syntax. Frames are resolved lazily, so normal actionability and assertion timeouts also cover frames that load or are replaced later. Multiple matching frames fail strictly unless narrowed explicitly.

```whirl
FILL frame:"#payment-element iframe" >> label:"Card number" "4242424242424242"
ASSERT frame:"#payment-element iframe" >> label:"Card number" value contains "4242"
```

### 6.2 Strictness

When an action or a single-element check runs, the locator must resolve to exactly one element. Zero matches fails after the timeout — except the `hidden` check, which passes when nothing matches (section 9.1). More than one match fails immediately with the candidate list, for `hidden` as well. Narrow the locator or add `nth:`. Only `count` accepts any number of matches. Both locators of `DRAG` follow this rule, and so does the target of `SNAPSHOT`. A `snapshot-mask` locator is not a target: it may match any number of elements (section 7).

### 6.3 AI targets

`ai:"description"` is a locator segment that a language model resolves. It
names one element in words, where a precise locator is hard to write:

```whirl
[Options]
model: anthropic/claude-sonnet-5

CLICK ai:"the Add to cart button for the first product"
FILL role:dialog >> ai:"the second email field" ada@example.com
ASSERT ai:"the order total" text == "$42.00"
```

An `ai:` segment can appear wherever a locator can: in actions, in `ASSERT`
and `CAPTURE` lines, as the target of `SNAPSHOT`, and as the scope of `ACT`,
`EXTRACT`, and `JUDGE`. It cannot appear in `snapshot-mask`, which can match
any number of elements. `ai:` must be the last segment: a segment after it is
a parse error. The segments before it limit what the model sees, as the scope
of `ACT` does: `role:dialog >> ai:"the second email field"` shows the model
only the dialog. They follow the rules of section 6.1, and every one carries a
prefix. The description supports interpolation.

A file that uses `ai:` needs the `model` option (section 5); without it,
`whirl check` reports the error `act-without-model`. `ai:` must resolve to
exactly one element, so it cannot be used with `count`: that is the lint
error `ai-count`. `--jev` (section 13) does not apply to `ai:`.

To resolve a target, Whirl takes the AI snapshot of the selected tab, or of
the element that the earlier segments select, without link URLs, as `ACT`
does (section 7.4). It asks the model for every element that matches the
description. The model answers with a list of refs, each with a short
description, and never guesses. Then Whirl applies the rules of section 6.2:

- One match: the line runs on that element, with the rules of its action or
  check. A check that retries reads the same element again; it resolves the
  target again only when the element is gone.
- Two or more matches: the line fails at once with `strictness` and lists the
  candidates.
- No match: Whirl asks again with a new snapshot, at most once every 2
  seconds, until the step timeout expires. In an `ASSERT` with the `hidden`
  state or with `not exists`, no match passes at once.

The AI cache (section 12.1) records the element that each target resolved
to, so a later run finds it without a model call. The JSON report adds an
`ai` object to the step: the model, each target with the locator it resolved
to and its cache status, and the token usage and cost of the model calls.

Model calls count against the step timeout, as for `ACT` (section 12). The
description goes to a third party, with masked values as placeholders
(section 7.4). A model error fails the line under the rules of `ACT`, with the
code `act-model`.

## 7. Actions

An action is a verb, an optional locator, and an optional value. Element-targeting actions use Playwright's actionability checks for that operation up to the applicable timeout. A trailing `@duration` overrides the step timeout for that line (section 12).

| Syntax | Meaning |
| --- | --- |
| `VISIT url` | Navigate, and continue once the new document has parsed. A `url` starting with `/` resolves against `base`. |
| `RESPONSE name METHOD url` | Name the first matching HTTP request started in this entry and wait for its response headers. |
| `HTTP METHOD url` | Send an independent HTTP request. Header and body lines can follow as defined in section 7.3. |
| `MOCK METHOD url STATUS` | Serve a fixed response to matching browser requests until the file ends. Header and body lines can follow (section 7.5). |
| `MOCK METHOD url failed` | Fail matching browser requests the way a dropped connection does (section 7.5). |
| `POPUP name` | Name an unnamed popup opened by the selected tab in this entry; selection stays unchanged. |
| `TAB name` | Select an open named tab for subsequent commands. The original tab is `main`. |
| `CLOSE name` | Close a named tab; selection stays unchanged. Already closed tabs succeed. |
| `CLICK locator` | Click the element. |
| `RIGHTCLICK locator` | Click the element with the right mouse button. |
| `MIDDLECLICK locator` | Click the element with the middle mouse button. |
| `DBLCLICK locator` | Double-click the element. |
| `FILL locator "text"` | Replace the input's content with `text`. |
| `TYPE locator "text"` | Focus the element, then send one key event per character of `text`. |
| `PRESS "Key"` | Send a key or chord (Playwright key names, for example `"Enter"`, `"Control+A"`) to the focused element. |
| `PRESS locator "Key"` | Focus the element, then send the key. |
| `CHECK locator` | Set a checkbox, radio, or switch to checked. |
| `UNCHECK locator` | Set a checkbox or switch to unchecked. |
| `SELECT locator "Label"` | Choose the `<select>` option with visible text `Label`. |
| `HOVER locator` | Move the pointer over the element. |
| `DRAG locator to locator` | Drag the first element and drop it on the second. |
| `SCROLL locator` | Bring the element into view. |
| `SCROLL [locator] down` | Scroll down one visible height. `up`, `left`, and `right` scroll the same way. Without a locator, the page scrolls. |
| `SCROLL [locator] to N%` | Scroll to a vertical position, from `0%` at the top to `100%` at the bottom. Without a locator, the page scrolls. |
| `UPLOAD locator file:path` | Set the file input to `path`, resolved relative to the `.whirl` file. |
| `DROP locator file:path` | Drop the file at `path` on the element, as a user drops a file from the desktop. `path` resolves relative to the `.whirl` file. |
| `SCREENSHOT name` | Save a full-page screenshot as artifact `name.png`. The name is an identifier that may also contain hyphens. Never fails the entry (see below). |
| `SNAPSHOT name` | Compare a full-page screenshot against the stored baseline; fails the entry on visual difference. |
| `SNAPSHOT name locator` | The same, for a screenshot of the element alone. |
| `EVAL "script"` | Run a JavaScript script in the page. The escape hatch; rules below. |
| `ACT "instruction"` | Ask a language model to choose one element action, then run it (section 7.4). |
| `ACT locator "instruction"` | The same, looking only inside the element (section 7.4). |
| `EXTRACT name [locator] "instruction"` | Ask a language model to read a value from the page, with an optional JSON Schema on the lines below (section 7.6). |
| `STORE local "key" "value"` | Write one `localStorage` entry on the current page's origin. |
| `STORE session "key" "value"` | Write one `sessionStorage` entry on the current page's origin. |
| `STORE cookie "name" "value"` | Set one cookie for the current page's host, with path `/`. |

`RIGHTCLICK` and `MIDDLECLICK` test what the page does with those buttons. A right click fires the page's `contextmenu` event, and a middle click fires `auxclick`; neither fires `click`. A page that shows its own menu on a right click, such as a file list, can be tested this way. The browser's own context menu is not part of the page, and no step can check it. What a middle click on a link does depends on the engine: Firefox opens a popup that `POPUP` can name, Chromium opens a tab without an opener that `POPUP` cannot name, and WebKit follows the link in the same tab. To test a link that opens a new tab, click it with `CLICK`.

`DRAG` moves the pointer to the first element, presses the left button, and holds it for 500 ms. It then waits until the second element is visible and stable, moves to its center in 10 steps, waits two animation frames, and releases. The hold serves drag code that starts only after a press delay, commonly 100 to 300 ms, and cancels a drag when the pointer moves sooner; drag code with a longer delay does not start. The steps serve drag code that starts after the pointer moves a few pixels. Native HTML5 drag and drop works in every engine, including from one frame into another. `DRAG` does not check the result: assert where the element landed. In WebKit, a page that took a native HTML5 drag gets no `pointerdown` for later presses until it loads again, so put a `VISIT` between such a drag and drag code that uses pointer events.

Every action already scrolls its element into view, so a flow needs `SCROLL` only for what scrolling itself does: content that loads as it comes into view, controls that react to the scroll position, and boxes that scroll on their own, such as a panel in a dialog. `SCROLL locator` brings the element into view, and passes when it already is. `down`, `up`, `left`, `right`, and `to N%` scroll a scroll box: the element when it can scroll in that direction, else the largest box inside it that can, else its nearest ancestor that can, else its document. An `iframe` element scrolls the page inside it, across origins too. So `SCROLL text:"Filter 3" down` and `SCROLL role:dialog Filters down` both scroll the dialog's list, and `html` or `body` stands for the page. A frame's elements scroll within that frame. A chunk is the box's visible height or width. A position is a share of the vertical scroll range, so `to 50%` centers the middle of the content. `SCROLL` scrolls at once, even when the page asks for smooth scrolling, and the step ends when the position holds for two animation frames. A box already at the requested position stays where it is, and the step passes. `SCROLL` fires the page's `scroll` events and intersection observers, but no `wheel` events. `SCROLL` cannot reach an element that the page has not rendered yet, such as a row far down a virtualized list; write as many `SCROLL locator down` lines as the list needs before the line that uses the row.

`UPLOAD` sets the files of an `<input type=file>`. Many upload widgets are drop zones with no file input, so `UPLOAD` cannot reach them; use `DROP`. `DROP` fires `dragenter`, `dragover`, and `drop` at the element's center with one file, as when a user drops the file from the desktop. The page sees the file's own name and size, and a type from its extension, such as `text/csv` for `report.csv`; an extension with no known type gives `application/octet-stream`. A page takes a drop only when a `dragover` handler calls `preventDefault()`. When none does, the element rejects the drop: Whirl fires `dragleave` instead of `drop` and fails the step at once. A missing file also fails the step. The element must be visible, so a zone that appears only while a drag is over the page cannot be the target. The events are synthetic, so a page that ignores events whose `isTrusted` is false rejects the drop. `DROP` does not check what the page did with the file: assert it.

`PRESS` with a single argument treats it as the key: `PRESS Enter` presses Enter on the focused element, even though `Enter` could also parse as a locator. Only when two arguments are present is the first a locator.

`CHECK` and `UNCHECK` are idempotent: a control already in the requested state is left alone. On a native checkbox or radio input, Whirl focuses the input and presses Space, which works whether the input is visible or hidden behind a styled track, as Chakra, Radix, and Headless UI switches hide theirs; a click on such an input would wait out the timeout. On any other control, such as a `role="switch"` button, Whirl clicks it. Both paths verify the resulting state and fail the entry with the expected and actual states when the page did not toggle. A radio button cannot be unchecked; `CHECK` another one in its group. A control hidden with `display: none` cannot take focus, and the error says so; locate the visible control instead.

`FILL` is the default way to enter text: it sets the value and fires one `input` event, which is what a plain field expects. `TYPE` is for the pages that listen for keys instead — segmented one-time-code inputs, masked fields, autocomplete boxes, and rich-text editors ignore a plain fill. It focuses the element and sends a `keydown`, `keypress`, `input`, and `keyup` per character, as Playwright's `pressSequentially` does. `TYPE` does not clear the element first. Use `FILL` unless the page needs key events; a flow that reaches for `TYPE` to slow down typing wants an assertion on the state the page should reach, not a slower `TYPE`.

`STORE` sets browser storage that a page reads to decide what to show — an onboarding flag, a dismissed banner, a feature switch — so a flow can skip a one-time screen without clicking through it or reaching for `EVAL`. `local` names `localStorage` and `session` names `sessionStorage`; both are per origin, so `STORE` runs against the origin of the current page, after a `VISIT` has landed there. `cookie` sets a session cookie for the current page's host with path `/` and no other attributes: no `Secure`, `HttpOnly`, `SameSite`, or expiry. That covers routing flags and feature switches, which is what `STORE` is for; a flow that needs an `HttpOnly` cookie is testing the server and should start from a saved `storage` state instead. A cookie is sent with the next request, so a page whose server reads it needs a second `VISIT`. `cookie` on a page without an http or https origin, such as a `data:` URL, fails the entry. `STORE` writes once and does not retry. A page that reads the key only while loading needs a second `VISIT` to see the value; a page that re-reads it on render picks the value up on its own. Values are strings, as in the browser; write `"true"` or `done`, not a number or a boolean. Section 11's masking applies to `STORE` values like any other line, so a masked variable stays masked in reports.

`SCREENSHOT` never fails the entry, even when it goes wrong: if the capture or the file write fails — a crashed page, an I/O error, or its step timeout expiring — Whirl skips the artifact and records a warning naming the screenshot and the cause, in the console output and in reports, so a missing artifact is always explained. One cap outranks this: an expiring `entry-timeout` fails the entry as usual, whatever line is in flight.

`SNAPSHOT` is retried like an assert through a shim-owned polling loop and uses Playwright's image comparator. Whirl captures frames until two consecutive frames are identical, then compares against the baseline at `<flow>.whirl-snapshots/<name>-<browser>-<platform>.png` next to the flow file. Images must have identical dimensions. By default no pixel may differ; a pixel differs when its color distance exceeds `snapshot-pixel-threshold` (default `0.2` on a 0–1 scale). On a mismatch Whirl recaptures and recompares until the step timeout. A stable mismatch fails the entry and writes the actual and diff images to the artifacts directory. Tolerances do not change frame stabilization or retry timing. The platform tag (`linux`, `darwin`, `win32`) keeps baselines rendered on one OS separate. The viewport is not part of the key. A missing baseline fails the run; `--update-snapshots` writes or refreshes baselines instead of comparing.

A locator after the name makes an element snapshot: `SNAPSHOT cart testid:cart`
captures only the element's rendered rectangle. The name stays first. Every
segment of the locator needs a prefix or is `nth:` (section 6.1), and chains,
`nth:`, and frames work as in other locators. The target is part of the action,
so it has no file default. Interpolation changes the locator's values, not its
segments. The target resolves on the selected tab and must match exactly one
element (section 6.2). A missing or hidden target waits until the step timeout.
Whirl resolves the target again for every capture, so a replaced element or a
changed size is seen. Each capture scrolls the element into view and waits
until it is visible and stable, as Playwright's `locator.screenshot()` does; the
scroll can affect later steps. An element taller than the viewport is captured
whole. The capture is a crop of the page: content that overlaps the element is
in it, and a scroll box shows only its visible content. Whirl does not disable
animations, isolate the element, or add padding.

An element snapshot checks the element's appearance and size, not its position
on the page. Use a full-page snapshot when the layout around the element
matters. Frame stabilization, masks, tolerances, retries, baselines, and
`--update-snapshots` work as for a full-page snapshot, on the element's image.
Activity outside the element does not delay stabilization unless it changes the
element's pixels. One step timeout covers finding, scrolling, capturing,
stabilizing, and comparing. When the page detaches or resizes the element
during a capture, Whirl finds it again and retries within the step timeout.
When a capture times out after a stable mismatch, Whirl checks the target once
without waiting: if the target is missing or hidden, the step fails with a
timeout and writes no actual or diff image, because the old pixels no longer
describe the page. If the target is still visible, the step fails with the
mismatch as usual. The actual and diff images have the element's size. Failure
screenshots, traces, and video are unchanged.

Element snapshots use the same baseline path as full-page snapshots, so a name
is unique across both kinds (section 14). Adding a locator to an existing
snapshot, or removing one, changes what its baseline shows: review the page and
run `--update-snapshots`, or use a new name to keep both. `SCREENSHOT` always
captures the full page.

The three snapshot options work in `[Options]` and on lines below a `SNAPSHOT`:

```whirl
[Options]
snapshot-mask: testid:clock
snapshot-max-diff: 0.1%
snapshot-pixel-threshold: 0.2

VISIT /checkout
SNAPSHOT checkout @20s
snapshot-mask: testid:order-number
snapshot-max-diff: 20

SNAPSHOT unmasked
snapshot-mask: none
snapshot-max-diff: 0
```

Each supplied local setting overrides only that setting. A local mask list replaces the complete file list. Omission inherits; explicit zero overrides. A later snapshot resumes the file defaults. Setup flows do not pass their snapshot settings to dependents. Repeated mask lines add masks within one scope. Literal, unquoted `none` clears the list and must be its only mask line. Duplicate scalar settings, conflicting masks, and unknown keys are parse errors. These duplicate rules apply only to the new options.

Masks use explicit locator prefixes and support chaining, substring matching, `nth:`, and frames (section 6). There is no default locator engine. Masks resolve from the selected tab, not from an element snapshot's target, so file masks mean the same in page and element snapshots; chain a mask through the target to limit it, as in `testid:cart >> testid:total`. Parts of a mask outside an element snapshot's crop add nothing to its image, and a mask that covers the target covers the whole image. Interpolation changes locator values, not grammar; a quoted or interpolated `none` is not a sentinel. A mask may match zero, one, or many elements. Missing masks do not wait. Frame selection retains strictness. Invalid selectors and browser errors fail the step. Playwright covers matching bounding boxes with pink (`#FF00FF`) without removing elements from layout. Every capture uses the masks, including stabilization, baseline updates, and saved actual images. Locators resolve again on each capture, including replaced elements. Hidden elements and frames follow the pinned Playwright screenshot behavior. Moving or resizing a masked element can still cause a difference. Masks apply only to `SNAPSHOT`, not to `SCREENSHOT`, failure screenshots, traces, or video.

`snapshot-max-diff` accepts an unsigned decimal integer from `0` through `9007199254740991`, or a percent literal from `0%` through `100%`, including decimals. Counts reject signs, fractions, and exponent notation. Equality with the limit passes. Percentage limits use every pixel in the captured image, including masked regions, as the denominator: the full page, or the element's image in an element snapshot, so 100 changed pixels in a 10,000-pixel element image are 1%. Percentages convert to a ratio without rounding to an integer count. Dimensions must still match at `100%`.

`snapshot-pixel-threshold` accepts a finite JSON number from `0` through `1`. It controls how different a pixel's color must be before that pixel counts toward `snapshot-max-diff`. It does not accept percentages.

Blank lines and comments do not end snapshot options. The next action, `PAGE`, check line, section header, or end of file ends them. Option lines are part of the snapshot step and its report text. Only the headline may have `@duration`. Options cannot attach to `SCREENSHOT` or follow a check line without a new `SNAPSHOT`. The formatter preserves option order, comments, and count versus percentage units.

Literal settings are validated before execution. File settings interpolate once at file start, including setup captures; local settings and the target interpolate when the snapshot starts, so earlier captures are available. Invalid resolved file settings fail the synthetic `[setup]` entry. Invalid resolved local settings or an undefined variable in the target fail the snapshot step. Reports include the target and effective settings, preserve count versus percentage units, and mask secret values using the normal rules in section 11.


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
ASSERT role:heading "Confirm payment" visible

CLICK role:button Confirm
ASSERT tab:payment closed @30s

TAB main
ASSERT text:"Payment complete" visible
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

The request that `RESPONSE` selects can be checked too, with the
`request:NAME` subject (section 9.2): its method, URL, headers, and body.

The command waits for response headers, not a completed body. Its timeout covers
both finding the request and receiving those headers. Checks and captures that
read the body (`body`, `bytes`, `json:`, and `xpath:`) wait for it within their
own step timeout. Network observation does not change
service-worker settings; worker-originated requests without a page frame are
outside the selected-tab scope. Observation retains at most 10,000 requests per
entry; exceeding this limit fails `RESPONSE` explicitly. Body reads are limited
to 1 MiB, with declared content length checked before reading when available.

```whirl
CLICK role:button "Place order"
RESPONSE order POST /api/orders
ASSERT response:order status == 201
ASSERT response:order header:content-type contains application/json
ASSERT response:order json:$.status == paid
ASSERT response:order json:$.items count >= 1
ASSERT request:order json:$.qty == 1
ASSERT text:"Order confirmed" visible
CAPTURE order_id: response:order json:$.id
```

### 7.3 Independent HTTP requests

`HTTP` starts its own entry and sends a request from Whirl's runtime. Its
headline is `HTTP METHOD url`, with an optional trailing step timeout. It has no
public name. Zero or more `NAME: value` header lines can follow. A JSON object,
a JSON array, or one fenced text body can then follow as the request body. The
body is last. A check line, another action, or end of file ends the request.

```whirl
HTTP POST /api/tests @30s
Authorization: "Bearer {{env.E2E_SETUP_TOKEN}}"
Content-Type: application/json
{
    "id": "4568",
    "evaluate": true
}
ASSERT status == 201
ASSERT json:$.status == RUNNING
CAPTURE test_id: json:$.id
```

A JSON body starts with `{` or `[` on the line after the headers and ends when
its outer value closes. Whirl validates its template-aware JSON structure while
parsing and preserves its authored text. If no `Content-Type` header is present,
Whirl sends `Content-Type: application/json`. An explicit header remains
authoritative.

A fenced text body starts and ends with three backticks on their own lines. The
newline after the opening fence and the newline before the closing fence are
delimiters, not body text. Other interior newlines are part of the body. Whirl
normalizes LF and CRLF source line endings to LF in both body forms. A line that
contains only three backticks cannot occur inside this body form.

````whirl
HTTP POST /api/import
Content-Type: text/csv
```
name,plan
Ada,pro
Grace,team
```
````

`HTTP` neither sends nor changes browser cookies, so an API-key assertion cannot
accidentally pass using the page's login session. Only explicitly supplied
headers carry credentials. URLs, header values, JSON bodies, and fenced bodies
support interpolation. Header names are literal. Case-insensitive duplicate
header names are rejected.

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

The `ASSERT` and `CAPTURE` lines in the same HTTP entry refer to its response
without a `response:name` prefix. A status code never fails the
request by itself, including a 3xx, 4xx, or 5xx status. Use an explicit `status`
check. Whirl emits the `unasserted-http-status` warning when an HTTP entry has no
status check. There is no implicit 2xx rule.

```whirl
HTTP GET /api/account
Authorization: "Bearer {{env.API_KEY}}"
ASSERT status == 200
ASSERT json:$.name == Ada
```

### 7.4 ACT

`ACT "instruction"` asks the language model named by the `model` option
(section 5) to choose one element action on the selected tab. Whirl then runs
that action.

```whirl
[Options]
model: anthropic/claude-sonnet-5

VISIT /products
ACT "add the first product to the cart"
ASSERT testid:cart-badge text == 1
```

Before each snapshot that a model reads, for `ACT`, `GOAL`, `ai:` targets,
`EXTRACT`, and `JUDGE`, Whirl waits for the page to settle: until the
document's DOM has loaded and no request has been open for 500 ms. WebSocket
and event-stream requests do not count, and a request open for 2 seconds stops
counting. The wait lasts at least 100 ms, so a request that the last action
just started is seen, and at most 5 seconds or half of the step's remaining
time. A page that does not settle is read as it is. Many pages load their
content after the document, so a model that read the page at once would not
see it. A cached locator's check (section 12.1) does not wait.

Whirl takes a Playwright AI snapshot of the selected tab. The snapshot is an
outline of the page's accessibility tree, and each element in it has a ref such
as `e12`. Elements inside iframes are included, with refs such as `f1e3`. Whirl
leaves out each link's URL and the cursor hints, which the model does not need
and which make up about a third of a link-heavy page's snapshot. Playwright
puts a line in YAML single quotes when its text holds characters such as `: `
or ` #`, as in `- 'button "Status: live" [ref=e5]'`. Whirl removes these
quotes, so every element line has the same form. Playwright shows every iframe
without a name, so Whirl adds the iframe's `aria-label`, or else its `title`,
as its name, as in `iframe "Incident history"`. Whirl sends the instruction
and the snapshot to the model in one structured-output call. The model answers with one element ref, one method, and the method's
arguments, or with no element. Whirl checks the answer and runs it as the
matching Whirl action, with that action's actionability and strictness rules:

| Method | Runs as |
| --- | --- |
| `click` | `CLICK` |
| `click` with `right` | `RIGHTCLICK` |
| `click` with `middle` | `MIDDLECLICK` |
| `doubleClick` | `DBLCLICK` |
| `hover` | `HOVER` |
| `fill` | `FILL` |
| `type` | `TYPE` |
| `press` | `PRESS` with a target |
| `selectOptionFromDropdown` | `SELECT` |
| `dragAndDrop` | `DRAG` |
| `scrollIntoView` | `SCROLL locator` |
| `scrollTo` | `SCROLL locator to N%` |
| `nextChunk` | `SCROLL locator down` |
| `prevChunk` | `SCROLL locator up` |
| `scrollLeft` | `SCROLL locator left` |
| `scrollRight` | `SCROLL locator right` |

For `dragAndDrop`, the element is the one to drag, and the one argument is the
ref of the element to drop it on, such as `e12`. That ref must be in the
snapshot and name another element. For `scrollTo`, the one argument is a
percent such as `50%`. In a snapshot of the whole page, the first element is
the page's `<body>`; a scroll method on it scrolls the page, and the line has
no locator, as in `SCROLL down`.

No method uploads or drops a file, so `ACT` cannot do either. Write an
`UPLOAD` or `DROP` line for that.

A long page can make the snapshot costly or larger than the model's context. A
locator before the instruction limits the snapshot to one element and what it
contains: `ACT css:form "click Buy"` shows the model only the form. The scope
waits for its element and must match exactly one (section 6.2), and every
segment carries a prefix (section 6.1).

To left-click a native checkbox or radio input, Whirl focuses it and presses Space,
as `CHECK` does, because a styled control often covers the input. The effect is
the same as a click: a checkbox toggles, and a radio is selected.

The snapshot shows a wrapper that has one visible child, such as a custom
dropdown's trigger inside a wider box, as one element. To click, double-click,
hover, drag, or drop onto such an element, Whirl points at the deepest element
inside it that shows the same text, instead of the element's center. The event still reaches
the element the model chose.

In a file that uses `ACT`, Whirl opens every shadow root that a page script
attaches, so `ACT` sees and acts inside closed shadow roots.
Other locators in that file can then reach inside them too. A page can notice
the change: a host's `shadowRoot` is no longer `null`. Closed shadow roots that
the HTML itself declares stay closed.

A model can change the case, spacing, or punctuation of text it copies. When
the text for `fill` or `type` matches a string that the instruction quotes,
ignoring case, spaces, and punctuation, Whirl types the instruction's
characters instead: for `ACT "type \"AbC 123\" into Search"`, an answer of
`abc 123` types `AbC 123`. A string counts as quoted in double quotes, in
curly quotes, or in single quotes at word boundaries, as in `'AbC 123'` but not
in `user's`. Text that matches no quoted string, and the option for
`selectOptionFromDropdown`, stay as the model wrote them.

After a `fill`, Whirl reads the field's value back. The line fails when the
field does not hold the value, as when a `maxlength` cuts it short or a script
clears it. Case, spaces, and punctuation do not count, so a field that formats
its value, such as a phone number, passes. Whirl skips the check when the
element has no value to read, such as a `contenteditable` element, or when the
page has removed it.

With `--jev` (section 13), TypeSafe's Jev plans first. Jev is a classifier:
it answers closed questions in a
few hundred milliseconds but cannot write text. Whirl asks it which kind of
action the instruction wants, then which element, from the snapshot elements
that kind of action can target. The first request also asks for the key to
press, the mouse button, the end state of a checkbox or switch, and whether
typing ends with choosing a suggestion. When Jev splits its vote between a
click and a select, a double-click, or a key press, and the two together reach
0.85, Whirl takes the more likely one; a merged vote on a page without a
native select is then a click. When Jev is sure the instruction chooses from a
list but the page has no native select, Whirl clicks the option the
instruction names: an element with an option role whose label the instruction
says as whole words, or else such a list item or clickable element, and Jev
confirms it. When no named option shows, Jev picks the control that opens the
list as the first of two steps, and step two clicks the named option on the
new snapshot. For a drag, Jev picks the element to drag, then where to drop
it, first from the parts of the page that can take a drop, such as regions,
lists, and dialogs, then from every named element. For a scroll, the first
request also asks which way it goes and whether the whole page or a part of it
scrolls. The page scrolls through its root element; Jev picks the part, from
the same candidates as a drop, or the element to bring into view; and a
position comes from the instruction's words: a percent, a fraction such as
0.75, halfway, the top, or the bottom. A suggestion to choose after typing and
a click that would undo a checkbox already in the asked state go to the model.
Each element is described by its role, name, and value, the text of its
table row or list item, the caption or name of its table and the header of its
column, the named sections around it, the nearest heading, and its place among
elements that look the same. Playwright shows a region that its own heading
names through `aria-labelledby` without a name. So an element with no name or
text that starts with a heading is described by that heading, not by the
heading before it. When no element in that list fits, Jev looks at
every element with a name or text, which catches custom controls built from
plain elements. A list of more than 40 elements is first cut to the 30 whose
descriptions share the most words with the instruction; when Jev rejects the
cut list, it sees the whole list, split into parts of at most 254 elements
that it reads in parallel. When exactly one element has the name the
instruction quotes, one small request confirms it. For `press`, a focused
control is the target without a question. Arguments come from the instruction
itself. For `fill`, a lone placeholder is the text to type. Otherwise Jev says
which quoted string or placeholder is the text, since another may name the
field. When none is, a small model call reads the text from the instruction
alone, without the page, and Whirl types it only when the instruction contains
those words, in the instruction's own characters. That call is one more model
call for the line. For `press` the key is the one Jev or the instruction
names, and for `select` the one option of a native select that the instruction
names. Whirl acts on Jev's pick when Jev is at least 0.7 confident and puts
none-of-these at 0.9 or less, or when it is at least 0.5 confident, puts
none-of-these at 0.1 or less, and has a clear leader: at least 0.6 and 2.5
times the runner-up. A pick with none-of-these above 0.5 waits while the next
list tries, and is used only if none-of-these stays at 0.7 or less. When Jev
splits its vote between copies of one control in one table row or list item,
either copy is the answer. Jev's choice is checked like a model answer. When
Jev is not confident, when it cannot supply an argument, for step two of a
two-step action when no named option shows, and when a Jev request fails, the
model plans the step instead, and its prompt lists Jev's likely matches when
Jev has some. Jev receives the instruction, with masked values as
placeholders, and the element descriptions. Whirl asks Jev through
`lithos-llm` as `typesafe/jev-latest` and prices its requests from the
catalog. Jev's usage is reported apart from the model's, and the step's cost
includes both.

A custom dropdown that must open before an option can be chosen is a two-step
action. The model marks its first answer as two-step. Whirl runs that action,
takes a new snapshot, and asks for the second action. When the second answer
names no element, the line passes with the first action.

A page can replace the chosen element while the model answers, as a framework
does when it renders the page again after load. A snapshot ref never matches
the replacement, so Whirl does not wait for it. The chosen element can also
fail its action's actionability checks, as when a banner covers it. In both
cases Whirl heals once: it takes a new snapshot and plans the same step once
more. A chosen action gets at most half of the line's remaining time, so a
heal has time to run. One `ACT` line heals at most once and makes at most
three model calls: two for a two-step action and one for a heal.

The AI cache (section 12.1) records the lines that each `ACT` line ran, so a
later run replays them without a model call. `--jev` plans only on a miss.

An `ACT` line fails the entry when:

- the model names no element (`act-no-match`); Whirl does not ask again,
- the answer does not match the schema, names an element that is not in the
  snapshot, gives the wrong number of arguments, drags an element onto
  itself, gives a scroll position that is not a percent from 0% to 100%, or
  uses an unknown placeholder (`act-invalid-decision`),
- the chosen element is replaced after the line already healed once
  (`stale-ref`),
- a filled field does not hold the value (`act-fill-mismatch`); the message
  shows what the field holds unless the value is masked,
- the chosen action fails after the line already healed once, with that
  action's error, or
- the step budget expires, like any step.

A model error is `act-model`. Content filtering and an input larger than the
model's context fail the entry (exit 1). Other model errors, such as rejected
credentials, a spent quota, a network failure, or a server error that remains
after retries, are runtime errors (exit 3).

The instruction goes to a third party. A value from `{{env.NAME}}` is sent as
the placeholder `%env.NAME%`, and a masked value inside another variable as
`%secretN%`. The model writes the placeholder into its arguments. Whirl puts
the value back only in the command it sends to the browser, and reports keep
the placeholder. Other variables and captures are sent as their values. The
snapshot contains text that the page shows, including values typed by earlier
steps; Whirl does not mask page content.

The step's report text is the authored line. The JSON report adds an `act`
object to the step: the model, the planner, each action that ran as a Whirl
line with the description of the element and the planner that chose it, and
the token usage and cost of the model calls, and with `--jev` Jev's requests,
tokens, and cost. A rendered line such as `CLICK role:button "Sign in"` describes the
element; it is not guaranteed to be unique on the page. The `act` object also
holds the line's cache status, and a heal's cached and new lines.

### 7.5 MOCK

`MOCK` serves a fixed response to the browser's requests, so a flow can test
the page against a known answer: a feature flag, an empty list, a server error,
or a network failure.

```whirl
MOCK GET /api/flags 200
{ "checkout_v2": true }

MOCK POST /api/orders 503
Retry-After: 30

MOCK GET https://cdn.example.com/fonts/* failed

VISIT /checkout
```

`MOCK METHOD url STATUS` is an action. `STATUS` is a status code from 200 to
599. Header lines and one JSON or fenced body can follow, under the rules of
`HTTP` (section 7.3). A JSON body without a `Content-Type` header gets
`Content-Type: application/json`. Without a body, the response body is empty.
`MOCK METHOD url failed` fails a matching request the way a dropped connection
does. It takes no header lines and no body. `MOCK` has no step timeout: it
registers at once, and a trailing `@duration` is a parse error.

`MOCK` lines can come before the first `VISIT`, so the first page load can use
them. A `MOCK` before `VISIT` belongs to the entry that `VISIT` joins.

A mock matches a request when both of these hold:

- The method is the same. Methods are literal uppercase names.
- The URL matches the pattern. A URL that starts with `/` resolves against
  `base`, as for `VISIT`. Whirl normalizes the pattern and the request URL as
  `RESPONSE` does (section 7.2) and ignores fragments. Every `*` in the pattern
  matches any run of characters, including none, `/`, and `?`. The rest
  matches exactly, so `/api/items` does not match `/api/items?page=2`, and
  `/api/items*` matches both. A pattern cannot match a literal `*`.

A mock lasts until the file ends. It applies to every tab and frame of the
flow, and to the page document that `VISIT` loads. A later `MOCK` with the same
method and the same URL, after interpolation and resolution against `base`,
replaces the earlier one.
When mocks with different patterns match one request, the one registered last
serves it. There is no way to remove a mock. `setup` does not carry mocks to a
dependent file.

`MOCK` applies only to requests that the browser sends. An `HTTP` request never
matches a mock. `RESPONSE` observes a mocked request like any other: it can
name a response that a mock served, and `request:NAME` can check what the page
sent. A request that a `failed` mock served has no response, so `RESPONSE`
fails on it (section 7.2).

A mocked request never reaches the network, so Whirl serves it even when
`allow-hosts` does not allow its host, and does not report that host as
blocked. A file that uses `MOCK` runs with service workers disabled, as with
`allow-hosts`, because a service worker can answer requests before a mock sees
them. The URL, header values, and body support interpolation, which happens
when the line runs.

Reports list each mock with its method, URL, and the number of requests it
served. When a file passes, a mock that served no request before the file ended
or before a later `MOCK` replaced it gets the warning `unused-mock`. A file that
fails stops early, so Whirl does not warn about its mocks. With `--har`, the
network log records a mocked response as an ordinary response with the mock's
status, headers, and body. It records a request that a `failed` mock served
with the status `-1` and the engine's failure text, as for any failed request.

### 7.6 EXTRACT

`EXTRACT` asks the language model named by the `model` option to read a value
from the page. The value is typed, and later lines check it with the
`extract:NAME` subject (section 9.2):

```whirl
[Options]
model: anthropic/claude-sonnet-5

VISIT /checkout
EXTRACT order testid:summary "the order total and line items"
{
    "type": "object",
    "properties": {
        "total": { "type": "number" },
        "items": { "type": "array", "items": { "type": "string" } }
    },
    "required": ["total", "items"]
}
ASSERT extract:order json:$.total > 0
ASSERT extract:order json:$.items count >= 1
```

`EXTRACT name [locator] "instruction"` is an action. A JSON Schema object can
follow on the next lines; it starts with `{` and ends when the object closes,
as an `HTTP` JSON body does (section 7.3). A schema cannot contain `{{ }}`.
Without a schema, the value is a string.

The name follows the rules of `RESPONSE` names (section 7.2), in a separate
namespace: a duplicate name, or a reference before the `EXTRACT` line, is a
lint error. A locator before the instruction limits what the model sees to one
element, as the scope of `ACT` does (section 7.4): it waits for its element,
must match exactly one (section 6.2), and every segment carries a prefix.
A file that uses `EXTRACT` needs the `model` option.

`EXTRACT` runs once, when its line runs. It does not retry, and the AI cache
(section 12.1) does not record it. Put an `ASSERT` before it that waits for
the page to show the value; `whirl check` warns with `extract-unsettled` when
`EXTRACT` directly follows an interaction, such as a `CLICK`, with no check
between them.

The model sees the instruction, with masked values as placeholders (section
7.4), and the AI snapshot of the selected tab, or of the scope, without link
URLs. It sees no screenshot. It answers through structured output in the shape
of the schema. The prompt tells it to copy text exactly, with every symbol; to
return every item when the instruction asks for a list or for "all"; to return
null when the page does not show a value; and to answer a link field with the
link's ref.

Only this subset of JSON Schema is allowed: `type` (`string`, `number`,
`integer`, `boolean`, `object`, `array`, `null`, or a list of these),
`properties`, `required`, `items`, `enum`, `const`, `anyOf`, `description`,
and `"format": "uri"` on a string. `whirl check` reports any other keyword or
format as the error `extract-schema-unsupported`. Before the call, Whirl
adapts the schema for providers that need strict schemas: every object gets
`"additionalProperties": false` and lists every property in `required`, a
property that the schema does not require may be null, and an `enum` or
`const` without a `type` gets the type of its values. A null answer for such
a property counts as absent. A schema whose root is not an object is sent as
an object with one `value` property, which may be null, and read back from
it.

A string with `"format": "uri"` is a link. The model answers it with the ref
of a link element in the snapshot, and Whirl reads that element's `href` and
resolves it against the page URL, so the value is an absolute URL.

JSON numbers keep their exact text (section 9.3). A null answer, or an empty
string without a schema, is a missing value (section 9.2).

An `EXTRACT` line fails the entry when:

- the answer does not match the schema (`extract-schema`),
- a link field names an element that is not a link with an `href`, or is not
  in the snapshot (`extract-ref`),
- the model fails, under the rules of `ACT` (section 7.4), with the code
  `extract-model`, or
- the step budget expires, like any step.

The step's report text is the authored headline. The JSON report adds an
`extract` object to the step: the model, the value with its type (masked as
a capture is, section 14), and the token usage and cost of the model call.

### 7.7 GOAL

`GOAL "goal"` asks the language model named by the `model` option to reach a
goal with several actions, one at a time. It takes no locator.

```whirl
[Options]
model: anthropic/claude-sonnet-5

VISIT /products
GOAL "add two blue mugs to the cart and open the cart"
ASSERT testid:cart-badge text == 2
ASSERT role:heading "Your cart" visible
```

A `GOAL` must be the last action of its entry, and the entry must have an
`ASSERT` that checks the result, because the model decides when the goal is
done. `whirl check` reports any other `GOAL` as the error `goal-unchecked`.
A file that uses `GOAL` needs the `model` option; without it, `whirl check`
reports the error `act-without-model`. In a file that uses `GOAL`, Whirl
opens every shadow root that a page script attaches, as for `ACT`.

Each model call receives the goal, with masked values as placeholders
(section 7.4), the Whirl lines that already ran for this `GOAL`, each with its
error when it failed, and the AI snapshot of the selected tab, without link
URLs. The model answers with actions in the form that `ACT` uses, or with
`done`, or with `impossible` and a reason. An answer holds one action, or,
to fill in a form, one action for each field that needs a value; they run in
order on the page that the snapshot showed, and the first that fails stops the
rest. Each counts as one action. Whirl checks each action as it checks an
`ACT` answer and runs it as the matching Whirl action, with the methods of
section 7.4. So `GOAL` cannot go to a URL, go back, reload the page, or upload
or drop a file. A custom dropdown takes two actions: one opens it, and the
next chooses the option. Each action gets at most the step timeout
(`step-timeout` option). An action that fails, as when its element never
becomes actionable, does not end the step: the next call shows the model the
failure, and the model plans again from a new snapshot.

The step ends when:

- the model answers `done`: the step passes, and the check lines run,
- the model answers `impossible`: the entry fails with `goal-impossible` and
  the model's reason,
- 20 actions have run, or failed, and the answer after the 20th is not `done`:
  the entry fails with `goal-limit`,
- the time runs out: the entry fails with `timeout`. The default is 2 minutes,
  not the step timeout; `@duration` overrides it, and `entry-timeout` caps it,
  as for any step (section 12).

An answer that `ACT` would reject fails the entry with `act-invalid-decision`.
A model error is `goal-model`, under the rules of `ACT`: content filtering and
an input larger than the model's context fail the entry (exit 1), and other
model errors are runtime errors (exit 3). `--jev` does not plan `GOAL`.

The AI cache (section 12.1) records the lines that ran, so a later run replays
them without a model call. A cached path is a straight list of lines, with no
branches. When a cached line misses because its element's role or name
changed, as when the page renamed a button, Whirl asks the model to find that
element again from the cached role and name, with an `ai:` target call
(section 6.3). When the model finds exactly one, the line runs on it with its
own method and arguments, and the rest of the path replays. After such a
re-find, the model sees every line that ran and says whether the goal is done,
as on any `GOAL` call. When a line misses in any other way, or the re-find
finds no element or several, the model plans the rest of the goal from the
current page, and it sees the lines that already ran. Either way the step
reports the warning `healed`, and `--cache=update` stores the path that ran. A
path that changes between runs heals on every run.

The step's report text is the authored line. The JSON report adds a `goal`
object to the step: the model, each action as a Whirl line with the
description of the element, who planned it (`llm` or `cache`), and its error
when it failed, how the goal ended (`done` or `impossible`) with the model's
reason, the cache status, a heal's cached lines, and the token usage and cost
of the model calls.

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

An `ASSERT` line holds one check. Check lines run in the order written. The first failing check fails the entry; a `JUDGE` line (section 9.8) fails it when the model answers `no`, and not when it answers `unsure`.

```
assert := "ASSERT" check [ "@" duration ]
check  := subject { filter } [ "not" ] predicate
        | locator state-check
        | "tab:" name "closed"
```

A check reads a value from its subject, passes it through zero or more filters from left to right, and tests the result with one predicate. `not` negates the predicate. For example, `url urlQueryParam page == 2` reads the current URL, takes its `page` query parameter, and compares it with 2.

```whirl
ASSERT testid:cart-badge text == 1
ASSERT css:.result count >= 1
ASSERT testid:total text replace , "" toInt > 1000
ASSERT url urlQueryParam page == 2
ASSERT eval "window.dataLayer" json:$[?@.event=='purchase'] count == 1
ASSERT response:order json:$.items[0].sku startsWith ABC-
```

Page checks retry until they pass or the step timeout expires. Response checks read data that never changes, so they fail at once. Section 9.7 gives the rules.

### 9.1 State checks

`visible`, `hidden`, `enabled`, `disabled`, `checked`, `unchecked`, `focused`.

`hidden` passes when the element is not visible, including when it does not exist. All others require the element to exist. A state check takes no filters and no `not`; write the opposite state instead.

`tab:name closed` retries until the named tab closes. An unknown name never counts as closed.

### 9.2 Subjects

| Subject | Type | Value |
| --- | --- | --- |
| `LOCATOR text` | string | Normalized text content of the element |
| `LOCATOR value` | string | Current value of an input, textarea, or select |
| `LOCATOR attr:NAME` | string | Value of attribute `NAME` |
| `LOCATOR count` | number | Number of matching elements |
| `url` | string | Full current URL |
| `title` | string | Normalized document title |
| `eval "script"` | any | Result of the script |
| `response:NAME status` | number | HTTP status |
| `response:NAME header:HEADER` | string | Value of the header; the name is case-insensitive |
| `response:NAME location` | string | The `Location` header, resolved against the request URL, so it is always absolute |
| `response:NAME body` | string | The body, decoded with the charset of its `Content-Type`, UTF-8 by default |
| `response:NAME bytes` | bytes | The body after content decoding, such as gzip |
| `response:NAME json:PATH` | any | Short for `response:NAME body json:PATH` |
| `response:NAME xpath:EXPR` | any | Short for `response:NAME body xpath:EXPR` |
| `request:NAME method` | string | The method of the request that `RESPONSE NAME` selected |
| `request:NAME url` | string | The request URL, without a fragment |
| `request:NAME header:HEADER` | string | Value of the request header; the name is case-insensitive |
| `request:NAME body` | string | The request body, decoded with the charset of its `Content-Type`, UTF-8 by default; empty when the request has no body |
| `request:NAME bytes` | bytes | The request body |
| `request:NAME json:PATH` | any | Short for `request:NAME body json:PATH` |
| `request:NAME xpath:EXPR` | any | Short for `request:NAME body xpath:EXPR` |
| `extract:NAME` | any, or string without a schema | The value that `EXTRACT NAME` read (section 7.6) |

Inside an HTTP entry (section 7.3), omit `response:NAME`: `status`, `header:HEADER`, `location`, `body`, `bytes`, `json:PATH`, and `xpath:EXPR` examine that entry's response. These implicit forms are invalid in a browser entry.

`NAME` in `attr:` and `HEADER` in `header:` are attribute names: a letter or underscore, then letters, digits, underscores, or hyphens. So `attr:aria-expanded` and `header:content-type` are valid. The formal grammar (section 17) calls this production `attr-name`. Header names support value interpolation.

Normalization collapses each run of whitespace to one space, trims both ends, and removes zero-width spaces and soft hyphens.

The `eval` subject runs its script under the rules of `EVAL` (section 7) each time Whirl reads the value. Its result follows the `eval` capture rules of section 10 and keeps its type: a string, a number, a boolean, `null`, a list, or an object. The script must not change the page, because it can run many times.

A response subject waits for the body within the step timeout. The 1 MiB body limit of sections 7.2 and 7.3 applies. All checks for one response examine the same response.

An `extract:NAME` subject reads the value of an earlier `EXTRACT` line with its type. Filters apply as usual: `extract:order json:$.total`. It reads once and does not retry, as a response check does (section 9.7). A null value is a missing value. A name that no earlier `EXTRACT` line declared is the lint error `unknown-extract`.

A request subject reads the request that `RESPONSE NAME` selected (section 7.2), with the headers the browser sent. A request check reads once and does not retry, as a response check does (section 9.7), and the 1 MiB body limit applies. `request:NAME` with a name that no earlier `RESPONSE` line declared is the lint error `unknown-response`. An `HTTP` entry has no `request:` subject. Its request is the one the file wrote.

An `HTTP` entry reads the exact bytes of its body. A `RESPONSE` reads its body through the browser, and Chromium and WebKit can hand a text body back already decoded. When the `Content-Type` names a charset other than UTF-8, Whirl undoes that decoding, so `body` and `bytes` match what the server sent. WebKit replaces bytes it cannot decode, so in WebKit a `RESPONSE` text body in a charset other than UTF-8 can differ from the bytes the server sent.

A subject can give a **missing value**:

- a locator with no match, for `text`, `value`, and `attr:`;
- an attribute that the element does not have;
- an absent response or request header, including an absent `Location` header for `location`;
- a JSONPath singular query that selects nothing (section 9.5).

`count` is never missing; zero is a value. Section 9.7 says how predicates treat a missing value.

### 9.3 Types

Every value has one of these types. Each type has one text form. Whirl uses the text form for interpolation (section 11), for reports, and for the `toString` filter.

| Type | Example sources | Text form |
| --- | --- | --- |
| string | `text`, `url`, a JSON string | The string itself |
| number | `count`, `status`, a JSON number, `toInt` | The number's JSON text, such as `42` or `1.5e3` |
| boolean | a JSON `true` or `false` | `true` or `false` |
| null | a JSON `null` | `null` |
| list | a JSON array, `split` | Compact JSON |
| object | a JSON object | Compact JSON, with keys in the order received |
| bytes | `bytes`, `base64Decode` | Base64 |
| date | `toDate` | RFC 3339 in UTC, such as `2026-09-26T08:00:00Z` |
| node set | an XPath expression that selects nodes | None; a node set supports only `count` and `exists` |

A JSON number keeps its exact text. Whirl does not convert it to floating point, except inside a JSONPath filter expression (section 9.5). So an integer larger than 2^53 compares and captures exactly. A number is an integer when its text has no fraction and no exponent, and a float otherwise. `toInt` gives an integer. `toFloat` gives a float, written with a fraction or an exponent, such as `3.0` or `1e20`.

### 9.4 Predicates

| Predicate | Passes when | Value types |
| --- | --- | --- |
| `== EXPECTED` | The value equals `EXPECTED` (section 9.6) | any |
| `!= EXPECTED` | The value does not equal `EXPECTED` | any |
| `> N`, `>= N`, `< N`, `<= N` | The value compares that way with `N` | number; a date against a date |
| `startsWith EXPECTED` | The value starts with `EXPECTED` | string, bytes |
| `endsWith EXPECTED` | The value ends with `EXPECTED` | string, bytes |
| `contains EXPECTED` | A string has `EXPECTED` as a substring, a list has an item equal to `EXPECTED`, or bytes have `EXPECTED` as a byte sequence | string, list, bytes |
| `matches /regex/` | The regex finds a match in the value | string |
| `exists` | The value is not missing, and is not an empty node set | any |
| `isBoolean` | The value is a boolean | any |
| `isEmpty` | The value is an empty list or object | list, object |
| `isFloat` | The value is a float | any |
| `isInteger` | The value is an integer | any |
| `isIpv4` | The value is a string that holds an IPv4 address | any |
| `isIpv6` | The value is a string that holds an IPv6 address | any |
| `isIsoDate` | The value is a string that holds an RFC 3339 date-time, such as `2026-09-26T08:00:00.000Z` | any |
| `isList` | The value is a list | any |
| `isNumber` | The value is a number | any |
| `isObject` | The value is an object | any |
| `isString` | The value is a string | any |
| `isUuid` | The value is a string that holds a version 4 UUID | any |

`not` before a predicate negates it: `not contains`, `not exists`, `not isEmpty`. `not ==` means the same as `!=`.

A value whose type is not in the predicate's "Value types" column fails with a type mismatch (section 9.7). When `whirl check` can see the types before the run, it reports the mismatch as the error `filter-type` (section 16).

### 9.5 Filters

Filters run from left to right. Each filter takes the value before it and gives a new value.

| Filter | Input | Output | Result |
| --- | --- | --- | --- |
| `count` | list, node set, bytes | number | Number of items, nodes, or bytes |
| `first` | list | any | First item |
| `last` | list | any | Last item |
| `nth INDEX` | list | any | Item at `INDEX`; `0` is the first and `-1` the last |
| `split SEPARATOR` | string | list | The parts between each occurrence of `SEPARATOR` |
| `regex /regex/` | string | string | Capture group 1, or the whole match when the regex has no group |
| `replace OLD NEW` | string | string | Every `OLD` replaced with `NEW` |
| `replaceRegex /regex/ NEW` | string | string | Every match replaced with `NEW`; `$1`, `$<name>`, `$&`, and `$$` refer to the match as in ECMAScript |
| `toString` | any | string | The text form (section 9.3) |
| `toInt` | string, number | number | An integer. A string must hold an optional minus sign and decimal digits. A float is truncated toward zero. |
| `toFloat` | string, number | number | A float. A string must hold a decimal number, such as `1.5`, `-2`, or `3e2`. |
| `toHex` | bytes | string | Lowercase hexadecimal |
| `toDate FORMAT` | string | date | The string parsed with `FORMAT` |
| `dateFormat FORMAT` | date | string | The date written with `FORMAT` |
| `daysAfterNow` | date | number | Whole days from now until the date, truncated toward zero |
| `daysBeforeNow` | date | number | Whole days from the date until now, truncated toward zero |
| `base64Decode` | string | bytes | Base64 decoded |
| `base64Encode` | bytes | string | Base64 encoded |
| `base64UrlSafeDecode` | string | bytes | URL-safe Base64 decoded, with or without padding |
| `base64UrlSafeEncode` | bytes | string | URL-safe Base64 encoded, without padding |
| `utf8Decode` | bytes | string | UTF-8 decoded |
| `utf8Encode` | string | bytes | UTF-8 encoded |
| `charsetDecode LABEL` | bytes | string | Decoded with the WHATWG Encoding Standard label `LABEL`, such as `gb2312` |
| `urlQueryParam NAME` | string | string | Value of the first query parameter `NAME`, decoded as form data (`%XX` and `+` for a space); missing when there is none |
| `urlEncode` | string | string | Percent-encoded, except unreserved characters and `/` |
| `urlDecode` | string | string | Percent-decoded |
| `htmlEscape` | string | string | `&`, `<`, and `>` replaced with character references |
| `htmlUnescape` | string | string | Named and numeric character references replaced with their characters |
| `json:PATH` | string, list, object | any | The result of the JSONPath query `PATH` |
| `xpath:EXPR` | string, bytes | string, number, boolean, node set | The result of the XPath 1.0 expression `EXPR` |

Filter arguments are values (section 3.1) and support interpolation. Regex arguments are regex literals and do not.

`toDate` and `dateFormat` use the `%` codes of the Rust `chrono` crate, as Hurl does; `%+` is RFC 3339. A date without a time zone is UTC.

`json:PATH` takes an RFC 9535 JSONPath query. A string input is parsed as JSON first; a list or an object is queried as it is. A singular query — one with only name and index selectors, such as `$.items[0].id` — gives one value, or a missing value when it selects nothing. Any other query gives a list of every match, which can be empty. Inside a filter expression such as `$.items[?@.price < 10]`, numbers compare as floating point, and the `match()` and `search()` functions use I-Regexp (RFC 9485), not ECMAScript. Whirl also accepts the Rust `regex` syntax that extends I-Regexp there, such as `\d`.

`xpath:EXPR` takes an XPath 1.0 expression and evaluates it with libxml2. Whirl parses the input as XML when it is the `body` or `bytes` of a response whose `Content-Type` is `text/xml`, `application/xml`, or ends in `+xml`. Otherwise it parses the input as HTML. `bytes xpath:` decodes the body like `body`; other bytes must be UTF-8. Whirl ignores an encoding that the document declares. XML must be well-formed, and Whirl never loads external entities. As in Hurl, the namespaces declared on the root element keep their prefixes, and the default namespace gets the prefix `_`: `xpath:"string(//_:feed/_:title)"`.

An expression that selects nodes gives a node set. An empty node set fails `exists`. Expressions such as `string(…)`, `count(…)`, and `boolean(…)` give a string, a number, and a boolean. A whole number is an integer, so `count(//li)` gives `3`; any other number is a float. NaN and infinity give a filter error.

A `json:` or `xpath:` argument is one bare token or one quoted value. JSONPath strings use single quotes, as in `json:$[?@.sku=='A-1']`. Quote the whole argument when it contains spaces or double quotes: `json:"$[?@.name == 'Ada Lovelace']"`. A token that joins bare and quoted parts, such as `json:$["a"]`, is a parse error, because the lexer would drop its inner quotes.

A filter that cannot work on its input gives a **filter error**. Examples are `toInt` on `abc`, `regex` with no match, invalid Base64, invalid JSON, an index outside a list, and `first` on an empty list. A missing value passes through filters without running them.

### 9.6 Typed comparison

How Whirl reads the expected value depends on the type that `whirl check` can see before the run:

1. **String values.** When the subject and its filters always give a string, the expected value is text. Quotes do not matter: `testid:badge text == 1` and `testid:badge text == "1"` mean the same. This covers the `text`, `value`, `attr:`, `url`, `title`, `header:`, `location`, and `body` subjects, alone or followed by filters that give a string.
2. **Typed values.** Otherwise the expected value is typed. This covers JSON values, `eval` results, numbers, and the output of filters such as `toInt`:
   - A bare number, `true`, `false`, or `null` is that value.
   - A quoted value is a string, and so is any other bare value.
   - A bare bytes literal is bytes.
   - A JSON literal is a list or an object.
   - A bare `{{name}}` that is the whole expected value takes the variable's type (section 11).

Equality follows JSON meaning:

- Values of different types are not equal, so `==` fails with a type mismatch and `!=` passes. Integers and floats are both numbers: `3` equals `3.0`.
- Numbers compare by value.
- Strings compare character by character, without normalization.
- Lists are equal when they have the same length and equal items in the same order.
- Objects are equal when they have the same keys with equal values, in any order.
- Bytes compare byte by byte, and dates compare as points in time.

`contains` on a list uses this equality for each item. `startsWith`, `endsWith`, and `contains` on a string compare with the expected value's text form, as in Hurl: `json:$.code startsWith 12` passes on `"123"`.

```whirl
ASSERT json:$.id == 42                     # the number 42, not the string "42"
ASSERT json:$.id == "42"                   # the string "42"
ASSERT json:$.active == true
ASSERT json:$.deleted_at == null
ASSERT json:$.tags == ["a", "b"]
ASSERT json:$.size == {"h": 20, "w": 10}   # key order does not matter
ASSERT json:$.items contains {"sku": "A-1", "qty": 1}
ASSERT testid:badge text == 1              # text, the same as "1"
```

### 9.7 Missing values, errors, and retries

A missing value passes `not exists` and fails `exists`. It fails every other predicate with a missing-value error, including `!=` and predicates with `not`. So a typo in a JSONPath or a header name does not pass.

One exception applies to attributes. When the element exists but does not have the attribute, `attr:NAME` passes `!=` and every predicate with `not`, and it fails every other predicate with a missing-value error. HTML gives absent attributes a meaning, so `attr:aria-current != page` passes on a link that has no `aria-current`.

Page checks retry. A page check is a check on a locator subject, `url`, `title`, or `eval`. Whirl reads the value, applies the filters, and tests the predicate. When the check does not pass, Whirl reads again after 100 ms, 250 ms, 500 ms, and then every 1000 ms, until the check passes or the step timeout expires. A false predicate, a type mismatch, a filter error, a missing value, and an `eval` exception all count as "not passing yet". At the timeout, the check fails with the result of its last attempt. A locator that matches more than one element fails at once (section 6.2).

Response checks and request checks do not retry. A false predicate, a type mismatch, a filter error, or a missing value fails the check at once.

Each failure has a stable report code: `assert` for a false predicate, `type-mismatch`, `filter-error`, `missing-value`, `eval` for an exception in an `eval` script, `eval-result` for an `eval` result outside the contract of section 10, `strictness`, and `read` for a subject that cannot be read, such as `value` on an element that is not an input or a `RESPONSE` body over the limit. See section 16 and [machine-readable output](docs/engineering/machine-output.md).

### 9.8 JUDGE

`JUDGE` asks the language model named by the `model` option whether a claim
about the page holds:

```whirl
[Options]
model: anthropic/claude-sonnet-5

VISIT /checkout
ASSERT testid:summary visible
JUDGE testid:summary "the total matches the sum of the line items"
JUDGE "the page shows no error message"
```

`JUDGE [locator] "claim"` is a check line, like `ASSERT`. It can appear only in
a browser entry. It runs once, when the entry reaches it; by then, the check
lines before it have passed. It does not retry, so put an `ASSERT` before it
that waits for the state the claim describes. `whirl check` warns with
`judge-alone` when its entry has no `ASSERT`. A locator before the
claim limits what the model sees to one element, as the scope of `ACT` does
(section 7.4): it waits for its element, must match exactly one (section 6.2),
and every segment carries a prefix.

The model sees:

- the claim, with masked values as placeholders (section 7.4),
- the AI snapshot of the selected tab, or of the element, without link URLs,
- a screenshot of the element, or of the viewport without a locator. Whirl
  captures frames until two in a row are identical, as `SNAPSHOT` does. It
  uses at most half of the time left in the step timeout, then sends the last
  frame, so the model call keeps the rest.

The prompt tells it to judge only from the outline and the screenshot, not to
use outside knowledge, not to assume anything that they do not show, to ignore
small differences that do not change what the claim means, and to answer
`unsure` when the evidence is missing, cut off, or ambiguous.

The model answers `yes`, `no`, or `unsure`, with a reason:

- `yes` passes.
- `no` fails the entry with the code `judge-false` and the model's reason.
- `unsure` passes with the warning `judge-unsure` and the model's reason. The
  entry goes on.

A model error is `judge-model`, under the rules of `ACT`: content filtering and
an input larger than the model's context fail the entry (exit 1), and other
model errors are runtime errors (exit 3). The step timeout, and a locator that
matches nothing, fail the entry as for any step. A run whose files use `JUDGE`
fails before any flow starts, with a runtime error (exit 3), when Whirl has no
credentials for the `model` option's provider.

The `model` option must name a model that the catalog marks as accepting
images; `whirl check` reports any other as the error `judge-without-images`.
A model whose image support the catalog does not know is the warning
`judge-images-unknown`. With `WHIRL_LLM_ENDPOINT` (section 13), neither
applies. The AI cache (section 12.1) never records `JUDGE`, so `--cache=only`
still calls the model.

The step's report text is the authored line. The JSON report adds a `judge`
object to the step: the model, the verdict, the reason, and the token usage and
cost of the model call.

## 10. Captures

A `CAPTURE` line extracts a value into a variable for later lines.

```
capture := "CAPTURE" name ":" subject { filter } [ "@" duration ]
```

- `name` matches `[A-Za-z_][A-Za-z0-9_]*`.
- The subjects and filters are those of section 9. A capture keeps the type of its value (section 9.3), so a later check can compare it by type (section 11).
- A page capture waits like a page check (section 9.7). Whirl reads again until the value is present and every filter succeeds, up to the step timeout. `text`, `value`, and `attr:` wait for the locator to resolve to exactly one element. A missing value at the timeout fails the entry; a capture never stores a missing value.
- `count` never waits: it records the current number of matches immediately, and zero is a valid result. Check a `count` first when the flow must wait for elements to appear.
- A response capture reads once, under the rules of section 9.7. Inside an HTTP entry, response subjects omit `response:name` (section 9.2).
- An `eval` capture runs its script once under the rules of section 7, and does not retry. Whirl owns the result contract, independent of Playwright's transport. A string is stored as a string. `null`, booleans, finite numbers, arrays, and plain objects that contain only those values, at any depth, are stored with their type. Anything else — `undefined`, non-finite numbers, `BigInt`, functions, symbols, cyclic structures, and browser objects — fails the entry.
- A capture that reuses a name overwrites it.

```whirl
CAPTURE order_id: testid:confirmation text regex /Order #(\w+)/
CAPTURE cart_url: url
CAPTURE item_count: response:cart json:$.items count
```

## 11. Variables

`{{name}}` interpolates a variable inside any value and inside an HTTP JSON or
fenced body.

Sources, later entries overriding earlier ones:

1. `--variables-file` entries (`name=value` lines),
2. `--var name=value` flags,
3. captures, as the file runs.

Variables are typed (section 9.3):

- A capture keeps the type of its value, and so does `{{setup.NAME}}`.
- A `--variables-file` entry, a `--var` flag, and `{{env.NAME}}` are typed the way Hurl types `--variable`. A JSON number, `true`, `false`, or `null` gives that type, and any other value is a string.

A variable's type matters in three places:

- **Typed comparisons.** In a typed comparison (section 9.6), an expected value that is exactly one bare `{{name}}` takes the variable's type. So `json:$.id == {{order_id}}` compares numbers when `order_id` holds a number. A quoted `"{{name}}"` is always a string.
- **JSON.** In an HTTP JSON body and in a JSON literal, a bare `{{name}}` in a value position inserts the variable as JSON; a string gets quotes and escapes. A `{{name}}` inside a JSON string inserts the variable's text form with JSON escapes.
- **Everywhere else.** `{{name}}` inserts the variable's text form (section 9.3). This includes a larger value, a URL, a header, and a fenced body.

Option values (section 5) use the text form. They resolve once, when the file starts, before the browser context is created. Only `--variables-file` entries, `--var` flags, and `{{env.NAME}}` are available there — captures do not exist yet, and a later capture never rewrites an option. A reference to an undefined variable in an option value fails the file before any entry runs and is reported as a failed run (exit 1).

`{{setup.NAME}}` reads a capture named `NAME` taken by the file's `setup` flow (section 5). It is available everywhere `{{name}}` is, including option values, and it is read-only: the file's own captures live in the plain namespace and never shadow it. A reference to a name the setup flow does not capture is a lint error, so `whirl check` catches it without a browser. A secret the setup flow masked stays masked in the dependent's output.

`{{env.NAME}}` reads the environment variable `NAME` at run time. This is the intended path for secrets; secret values never belong in `.whirl` files. A reference to an undefined variable or unset environment variable fails the step.

Whirl masks every value sourced from `env.*` in the textual output it generates: console failure details, rendered step text, the JSON and JUnit reports, and trace step titles. Masking works on the text form. A masked capture keeps its type in the JSON report. Browser-recorded artifacts — screenshots, video, HAR files, and saved storage state — can still contain secrets the flow typed or received. Treat the artifacts directory and storage-state files as sensitive, and prefer dedicated test credentials.

## 12. Execution model

- **Isolation.** Each file runs in a fresh browser context with an initial page named `main` and any popups it opens. Without the `storage` option the context starts empty; with it, the context starts from the saved storage state. Files never share live state either way.
- **Order.** Entries run top to bottom. Within a browser entry: actions, then `PAGE`, then check lines in the order written. Within an HTTP entry: the request, then its check lines in the order written.
- **Failure.** The first failing step fails the entry, and a failed entry stops its file; remaining entries in that file are skipped and reported as skipped. Other files still run. On failure Whirl saves a full-page screenshot and, with `--trace`, a Playwright trace to the artifacts directory.
- **Navigation.** `VISIT` completes when the new document reaches `DOMContentLoaded`: the HTML is parsed and its synchronous scripts have run. It does not wait for the `load` event, because images, fonts, iframes, and media hold `load` open for reasons a flow never asserted, and every later line waits for what it needs anyway: actions wait for their element to be actionable, asserts and `PAGE` retry. A page that only becomes usable after `load` needs an assert on that state before an `EVAL` or `SCREENSHOT`, which run once without waiting.
- **Retries.** Page checks and page captures read their value again on the schedule of section 9.7 until they pass or the step timeout expires. Response checks, response captures, and `eval` captures read once.
- **JUDGE.** A `JUDGE` line runs once, after the check lines before it in its entry pass. Its screenshot, snapshot, and model call share its step timeout, so a `JUDGE` line that needs more sets its own `@duration`.
- **ACT.** An `ACT` line is one step. Its snapshots, model calls, Jev requests, and actions share its step timeout. Model calls take seconds, so an `ACT` line that needs more than the step timeout sets its own, such as `@60s`.
- **Timeouts.** Each action, PAGE, assert, and capture line gets the step timeout (`step-timeout` option, default 10s); `VISIT` gets the navigation timeout (`nav-timeout` option, default 30s), and `GOAL` gets 2 minutes (section 7.7). A trailing `@duration` on any such line overrides its own budget: `CLICK "Generate report" @60s`. The optional `entry-timeout` option caps an entry's total time across all of its lines; when it expires, the in-flight step fails with an entry-timeout error. An entry without one is still bounded by its per-step timeouts. The suffix must be bare: a line’s final bare token of the form `@duration` is always its timeout, and a quoted `"@60s"` is an ordinary value. Timeouts are enforced from outside the page, so they hold even when the page cannot respond — an `EVAL` script blocking the renderer or returning a Promise that never settles. When a timed-out step cannot be cancelled cleanly, Whirl closes that flow's browser context; if closing also stalls, it terminates and restarts only that worker's shim process. Either way the flow fails and reports normally, and other files are unaffected.
- **Setup.** Files with a `setup` option run after their setup flows. Whirl first runs every distinct setup flow named by the inputs, once each and in parallel like any files, then runs the remaining files, each starting from its setup flow's saved state with the setup flow's captures as `{{setup.name}}`. A setup flow that is also an input runs once, as the setup. A failed setup flow reports normally, and each of its dependents reports a `[setup]` failure naming the setup flow and its first failing step, without opening a browser. Setup flows are one level deep.
- **Parallelism.** Files run in parallel across worker slots (`--jobs`, default: logical CPU count). A single file is never parallelized.
- **Dialogs.** `alert`, `confirm`, and `prompt` dialogs are auto-dismissed by default. The `dialogs: accept` option auto-accepts them instead.
- **AI targets.** An `ai:` target resolves inside its line's step timeout, and its model calls count against it (section 6.3). A check that retries asks the model at most once every 2 seconds, so its cost grows with its timeout. Give such a line its own `@duration` only when the page needs it.

### 12.1 The AI cache

A language model makes a decision once, Whirl writes it to a file next to the
flow, and later runs replay it without a model call. The file is ordinary
text for the repository: commit it and review its changes like the flow.

The cache of `flows/checkout.whirl` is `flows/checkout.whirl-cache.json`.
It records what each `ai:` target (section 6.3), each `ACT` line (section
7.4), and each `GOAL` line (section 7.7) resolved to:

```json
{
  "version": 1,
  "entries": [
    {
      "kind": "ai-target",
      "line": "CLICK ai:\"the Add to cart button\"",
      "occurrence": 1,
      "target": "ai:\"the Add to cart button\"",
      "model": "anthropic/claude-sonnet-5",
      "locator": "role:button \"Add to cart\"",
      "fingerprint": {"role": "button", "name": "Add to cart"}
    },
    {
      "kind": "act",
      "line": "ACT \"sign in as {{env.USER}}\"",
      "occurrence": 1,
      "model": "anthropic/claude-sonnet-5",
      "actions": [
        {
          "line": "FILL label:Email {{env.USER}}",
          "fingerprints": [{"role": "textbox", "name": "Email"}]
        },
        {
          "line": "CLICK role:button \"Sign in\"",
          "fingerprints": [{"role": "button", "name": "Sign in"}]
        }
      ]
    }
  ]
}
```

**Key.** An entry belongs to one line of the flow. Its key is the `kind`
(`ai-target`, `act`, or `goal`), the authored `line` before interpolation, without its
comment, and the `occurrence` of that text among identical lines of the file,
from 1 in file order. An `ai-target` entry also names its `target`: the
authored locator with the `ai:` segment, because one line can have two, as in
`DRAG`. Variable values are not part of the key: the fingerprint check below
finds an entry that no longer fits the page. Browser and platform are not part
of the key either.

**Values.** Every entry records the `model` that resolved it. An `ai-target`
entry holds the `locator` of the element and its `fingerprint`: the element's
ARIA role and accessible name, or `null` for an element without a name. An
`act` entry holds the Whirl `line` of each action that ran, in order, with the
fingerprint of each element in the line. A `goal` entry has the same form as
an `act` entry; it holds the actions that ran and leaves out those that
failed. Each locator comes from the locator
generator below, not from the report text of section 7.4, which is not
guaranteed to be unique. Entries follow the order of their lines in the flow.
Whirl writes the file as JSON with two-space indentation, the keys in the order
shown, and a final newline, so its diffs stay small.

**Secrets.** An entry never holds a masked value (section 11). It holds the
variable reference that the line used, such as `{{env.PASSWORD}}`: the model
answers with placeholders (section 7.4), and Whirl maps each one back to its
reference. Text that equals the value of another variable that the
instruction used is also stored as that variable's reference, so a cached
line types the current value. When Whirl cannot map a masked value back to a
reference, as for a secret inside a larger variable, it does not write the
entry: that line resolves with the model on every run and reports the warning
`cache-secret`.

**Locators.** The locator generator turns the element the model chose into a
strict Whirl locator. It prefers, in order, `testid:`, `role:TYPE "Name"`,
`label:`, `placeholder:`, and `text:`. When the best of these matches more than
one element, it adds the nearest named landmark, dialog, or region around the
element as a scope with `>>`, and it adds `nth:` only as a last resort. It
never uses `css:`, except in a `frame:` segment: an element inside an iframe
gets a `frame:` prefix whose CSS selector names the iframe by its `title`,
`name`, or `id` attribute, as in `frame:"iframe[title='Payment']"`. Whirl
checks that the locator finds that element and no other. When no such locator
exists, Whirl does not write the entry: the line resolves with the model on
every run and reports the warning `cache-unstable`.

**Replay.** For an `ai:` target with an entry, Whirl waits for the cached
locator for at most half of the step's remaining time. When it finds exactly
one element with the same role and name as the fingerprint, the line runs on
it with no model call: a hit. For an `ACT` or `GOAL` line with an entry,
Whirl runs each cached line in order, as if the flow held it, after the same
fingerprint check; each action gets at most half of the line's remaining time.
For `GOAL`, the step timeout stands in for the line's remaining time in both
limits when it is shorter, so a heal starts soon. Everything else is
a miss:

- no entry for the line,
- a cached locator that matches no element in that time, or several,
- an element whose role or name differs from the fingerprint,
- a cached action that fails, as when its element never becomes actionable.

On a miss, Whirl resolves the target, or plans the `ACT` line, with the model,
as if no entry existed, in the rest of the step's time. When a cached `ACT`
line misses after an earlier cached action ran, the model plans the rest of
the instruction from the current page, as for step two of a two-step action.
A cached `GOAL` line that misses finds its element again, or continues from
the current page (section 7.7). This is a heal. The step passes or fails on its new
result.

**Modes.** `--cache` (section 13) selects what a run does with the file:

- `replay`, the default: a hit runs with no model call. A miss resolves with
  the model, and a step that then passes reports a warning: `cache-miss` for a
  line with no entry, and `healed` for a stale entry, with the cached value and
  the new value. The run never writes the file.
- `update`: as `replay`, and after a file passes, Whirl writes its cache. It
  adds new entries, replaces healed ones, and removes the entries the run did
  not use. A file that fails writes nothing. A cache left with no entries is
  deleted.
- `only`: a miss fails the step with `cache-miss`, and `ai:` targets,
  `ACT`, and `GOAL` make no model calls. `EXTRACT`, `JUDGE`, and absence checks are never
  cached, so they still call the model.

An `ai:` check with `hidden` or `not exists` that passes because the model
found no element has no element to cache. It asks the model on every run, in
every mode, and reports the warning `uncached`. After a run that healed or
missed any step, the console says how many and names `--cache=update`.

`whirl check` reads the cache of each flow it checks. A cache that is not a
valid version 1 file is the error `cache-invalid`. An entry whose line no
longer exists is the warning `cache-stale-entry`; `--cache=update` removes it
on the next passing run.

## 13. Command line

```
whirl [OPTIONS] <PATH>...        Run files; directories recurse to *.whirl
whirl check [--json] <PATH>...            Parse and lint only; nothing runs
whirl install [BROWSER]...       Provision the shim bundle and selected browsers
whirl doctor [--browser NAME]    Check the runtime and browser; print repair commands
whirl show-trace <PATH>          Open a trace with the private runtime
whirl fmt [--check] <PATH>...    Rewrite files to canonical form
whirl report <REPORT>... --html <PATH>  Generate HTML from saved results
```

`whirl install chromium` provisions only Chromium; any combination of `chromium`, `firefox`, and `webkit` may be named. Without names, all three engines are provisioned. The bundle records the Whirl version that installed it; a binary of another version refuses to run that bundle and reports a runtime error naming `whirl install`, so an upgraded `whirl` never drives a stale shim. `whirl doctor` checks the selected Node runtime, the bundle's version, shim protocol, Playwright version, and a real headless browser launch (Chromium by default). It installs nothing, finishes within 30 seconds, exits 0 when ready or 3 when diagnosis fails, and prints repair commands. On Linux, a failed launch also prints the private-runtime command for installing system libraries. Unsupported browser names are usage errors.

`whirl fmt` rewrites files to the canonical form: single spaces between tokens,
quotes only where a value requires them, one HTTP header per line, check lines
directly after the actions of their entry, and one blank line between entries.
It rewrites removed `[Asserts]` and `[Captures]` sections as `ASSERT` and
`CAPTURE` lines (section 4.1). It keeps the quotes on a value whose bare form is a typed
literal, such as `"42"` or `"true"` (section 3.1), and it keeps JSON literals as
written. It preserves JSON and fenced body text, apart from the LF
line-ending normalization defined in section 7.3. `--check` writes
nothing and exits with code 1 when any file would change.

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
| `--report-metadata PATH` | Read author-written HTML report context from JSON; requires `--report-html` or `--report-json` |
| `--fail-fast` | Stop scheduling new files after the first failure |
| `--update-snapshots` | Write or refresh SNAPSHOT baselines instead of comparing |
| `--video` | Record a .webm video of each file's run into the artifacts directory |
| `--video-fps N` | Frames per second for `--video` on Chromium, 1 to 60; requires `--video` |
| `--har` | Record a .har network log per file into the artifacts directory |
| `--storage PATH` | Override the storage option |
| `--save-storage PATH` | Write the final storage state after a successful run (single file only) |
| `--entry-timeout DURATION` | Override the entry-timeout option |
| `--user-agent UA` | Override the user-agent option with `chrome`, `firefox`, `safari`, or a literal string |
| `--jev` | Plan `ACT` with TypeSafe's Jev first, and the `model` option when Jev is unsure (section 7.4) |
| `--cache MODE` | What to do with each flow's AI cache: `replay` (default), `update`, or `only` (section 12.1) |

Exit codes:

| Code | Meaning |
| --- | --- |
| 0 | All files passed |
| 1 | The command's negative result: a failed entry (run), formatting drift (`fmt --check`) |
| 2 | Parse or lint error |
| 3 | Runtime error (browser or shim failure) |
| 4 | Usage error |

Two environment variables change where `ACT` sends its model calls (section
7.4). `WHIRL_LLM_ENDPOINT=http://host:port` sends every call to one
OpenAI-compatible Chat Completions server at `<url>/v1/chat/completions`. The
`model` option is then the model name that server receives, and `whirl check`
does not check it against the catalog. `WHIRL_LLM_API_KEY`, when set, is sent
to that server as a bearer token. These variables serve local model servers,
proxies, and tests.

`--jev` reads Jev's key from `TYPESAFE_API_KEY`. Without it, a run whose files
use `ACT` fails before any flow starts, with a runtime error (exit 3).
`WHIRL_JEV_ENDPOINT=http://host:port` sends Jev's requests to
`<url>/v1/systemone` on another server, such as a test double. `--jev` does
nothing in a run whose files do not use `ACT`. `--rerun-failed` does not read
it from a report; pass it again.

`--video` records the `main` tab (section 7.1). On Chromium, Whirl records the page's own screencast frames through Playwright's bundled ffmpeg at 60 frames per second, or at the rate `--video-fps` names; a still page holds its last frame, so the recording always plays at a constant rate. Chromium sends a frame only when the page paints, so a short flow on a still page can end before the first frame arrives. Whirl then captures the page and holds that frame for the whole recording. Chromium cannot capture a page that has not painted yet, as right after a navigation, so Whirl tries again for up to 1 second. When Whirl still has no frame, as for a crashed page, the recording holds a white frame, as Playwright's recorder does, and Whirl reports a warning that says why. When ffmpeg fails while it finishes a recording, Whirl skips the recording and reports a warning. Neither warning changes the file's result. Firefox and WebKit use Playwright's recorder at its fixed rate of 25 frames per second. `--video-fps` on those engines is not an error: the file records at 25 frames per second and reports a warning, because the recording is evidence, not a result. A missing ffmpeg fails the file as a runtime error; `whirl install` provisions it with every browser build, and `whirl doctor` checks for it.

`--rerun-failed` reads a version 1 or version 2 report with an absolute `workingDirectory`. Relative file paths resolve against that directory, even when the report is moved. Each selected file runs from the beginning, including its setup. Existing CLI overrides and secrets must be supplied again; a report is not executable configuration. An unsupported or malformed report is a usage error. A report with no failed or errored files exits 0 with a message and launches no browser.

If the input paths select no `.whirl` files, run, `check`, and `fmt` report a usage error (exit 4).

Whirl parses and lints every input file before it launches any browser: a parse or lint error anywhere stops the invocation with exit 2 and nothing runs. When one invocation hits several categories, the highest applicable code wins — a usage error (4) is detected before parsing and preempts everything, and within a run a runtime error (3) outranks failed entries (1), which outrank 0.

## 14. Output and reports

- Action failures retain Playwright's actionability log, including the locator being awaited and any element blocking interaction. Output masking applies to the log. Each trace artifact includes a shell-quoted `whirl show-trace -- <PATH>` command. The viewer uses Whirl's private runtime; a missing trace is a usage error, and a viewer launch failure is a runtime error.
- Default console output: one line per file with pass/fail and duration, then a failure detail block per failed entry: file, line, the failing step, expected versus actual, and the artifact paths.
- The JUnit report maps one file to one test suite and one entry to one test case. An entry is named by the comment line directly above it — the nearest comment line with no other content line between it and the entry's first action (`# Log in.`) — falling back to its first action line and line number. A failure before the first entry — option resolution, storage loading, or browser launch — reports as a synthetic test case named `[setup]` in that file's suite: an `<error>` for runtime errors, a `<failure>` otherwise. The JSON report carries the same synthetic entry, and masking applies to it like any other output.
- The JSON report is version 2. Version 2 differs from version 1 only in the shape of captures (below). The report includes `workingDirectory`, `whirlVersion`, `platform`, and `architecture`. Files whose browser context starts include `runtime`: browser engine, viewport, the actual user agent string (with section 11's masking), and actual browser, Node, and Playwright versions. Unavailable user agent and version fields are null. Each step error has a stable `code`, separate from its human-readable message. Additive fields do not change the report version; readers must ignore unknown fields. See [machine-readable output](docs/engineering/machine-output.md) for schemas and codes.
- The JSON report is the machine-readable superset: per-step timing, captures, and artifact paths. Each capture is written as its type and value, such as `{"type": "number", "value": 42}`. Bytes are written as Base64, dates as RFC 3339 text, and numbers with their exact JSON text. A capture with a value sourced from `env.*` keeps its type and has its value masked.
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
  "details": { "Application commit": "abc123", "Fixture SHA-256": "..." },
  "files": {
    "flows/login.whirl": {
      "title": "Account access",
      "description": "Sign in and verify that the account page is available."
    }
  }
}
```

All fields are optional. `details` maps labels to plain-text values, such as application commits and fixture checksums. These are author-provided facts, not verified by Whirl. Metadata can be saved with `--report-json` without generating HTML. Without a title, the report uses `Browser test report` and each flow uses its path. `files` keys resolve relative to the metadata file; Whirl matches canonical paths, including symlinks. Paths must name existing files; duplicate canonical paths, invalid JSON, wrong field types, and unknown fields are usage errors before execution. Metadata for unselected flows is ignored. Metadata is not interpolated or executed and cannot change results. When JSON output is also requested, its optional `metadata` field contains the author context with selected file keys matching the report's file paths.

### 14.2 HTML from saved results

`whirl report report.json --html evidence.html` renders a version 1 or version 2 JSON report without parsing flows, resolving variables, installing a runtime, or starting browsers. It preserves the recorded results, original Whirl version, platform, browser environment, and run timestamps. It never uses the JSON file's modification time as execution time.

`--metadata context.json` replaces the saved author context. It uses the section 14.1 schema, but `files` keys match recorded `files[].path` strings exactly. Source files do not need to exist. Unmatched keys are ignored. Invalid author metadata remains a usage error.

Relative artifact paths resolve against the report's absolute `workingDirectory`, even if the JSON is moved. `--working-directory DIR` selects another directory for relative artifact paths, such as a copied project tree. Absolute artifact paths remain absolute. Keep each run's JSON and artifacts together in a distinct output location if later runs would overwrite the media. The command embeds the available files at their recorded paths; source hashes do not authenticate artifacts.

A successful render exits 0, including when the saved tests failed or media is missing. Invalid or unsupported input is a usage error (exit 4); an output failure is a runtime error (exit 3). The protection and atomic-write rules in section 14.1 also apply. The command never changes the input JSON. Older version 1 reports with `workingDirectory` are supported; absent timestamps, hashes, roles, and recording settings are shown as not recorded.

### 14.3 Run records

New JSON reports include `startedAt` and `finishedAt` as UTC RFC 3339 timestamps for the run and each reported file. Run timestamps enclose preparation, scheduled flows, and cleanup. File timestamps enclose each scheduled attempt, including failures before browser startup. Files not scheduled because of `--fail-fast` remain absent. `durationMs` continues to use the monotonic clock; wall-clock adjustments can affect timestamps.

Each file includes `sourceSha256`: the lowercase SHA-256 of the exact UTF-8 bytes parsed, including comments and line endings, before interpolation. Whirl retains this hash even if the source changes during execution. The hash covers only that flow file, not external fixtures, variable values, browser artifacts, or application code.

Each file includes `roles` with independent `requested` and `setup` booleans. `requested` means the file was selected by the invocation's input paths or `--rerun-failed`. `setup` means another selected flow names it through the `setup` option. Both can be true; the flow still runs once as setup. A synthetic `[setup]` entry in a requested scenario does not make that scenario a setup flow. HTML labels these roles and shows both counts; a flow with both roles appears in both counts. Status totals count each reported file once.

The run's `videoRequested` boolean distinguishes an absent requested recording from a run made without `--video`. A recorded file's `runtime.videoFps` is the frame rate of its recording (section 13), so a consumer can tell a 60 frames per second Chromium recording from a 25 frames per second one; the HTML report shows it in the recording's caption. These fields were added within report version 1 and remain in version 2. Consumers must parse timestamps to compare execution times and must ignore unknown fields.

### 14.4 Combined evidence and expected scenarios

`whirl report first.json second.json --html evidence.html` selects one requested attempt per complete recorded flow path. It compares each file's parsed `startedAt`, falling back to its run's `startedAt`. A newer failure replaces an older pass. Source hash and browser changes do not create separate scenario identities; their recorded values remain visible. File modification times and input order never select attempts.

Identical copies of a report are counted once. Repeated paths with missing start timestamps or conflicting equal start timestamps are usage errors. Older reports without timestamps can still be combined when their flow paths are distinct. Different filenames with the same basename remain distinct when their recorded paths differ.

`--expected expected.json` supplies a nonempty JSON array of unique, nonempty recorded flow paths. It defines scenario order and coverage without reading flow sources:

```json
["flows/login.whirl", "flows/checkout.whirl"]
```

Expected paths without a requested attempt show `Not run`. This is a report view state, not a recorded execution status. A skipped attempt stays `Skipped`. Setup-only attempts appear separately, retain their source run, and do not satisfy expected scenarios. Flows with both roles remain scenarios and keep their setup label. Older reports with unknown roles can satisfy expected paths; their roles remain labeled as not recorded. With an expected list, unmatched results appear under Other flows. Setup and other flows stay outside scenario totals.

Multiple distinct input reports produce an explicit combined-evidence label. The report does not claim that one complete suite passed and does not invent one run timestamp or wall-clock duration. Each displayed attempt links to its source report's original producer, timestamps, and author context. Its browser, source hash, media base, and results remain attached to that attempt. Setup attempts from every supplied run remain visible, including failures from older runs.

For combined evidence or expected-scenario views, `--metadata` supplies the overall title, description, and details, and overrides flow context by exact path. Without a flow override, each selected attempt uses its own source metadata. Original report-level context stays under Source reports. A single input without `--expected` retains the section 14.2 behavior.

The artifact override applies to each input's relative paths. Destination protection includes all supplied reports, metadata, the expected list, recorded source paths, and artifacts, including unselected attempts. Input and output errors retain the section 14.2 exit codes.

## 15. Architecture

Rust source, configuration, and project setup follow the [Brynary Rust Style Guide](https://github.com/brynary/rust-style-guide). TypeScript source and language tooling follow the [Brynary TypeScript Style Guide](https://github.com/brynary/typescript-style-guide) for language-level and authoring conventions. The shim targets the pinned private Node runtime specified here, so the TypeScript guide's Bun-specific runtime, API, package-management, and test-runner policies do not apply. This specification and accepted Whirl ADRs take precedence over both guides.

- `whirl` is a single Rust binary containing the parser, the runner, the check engine, the reporters, and the shim manager. It also makes `ACT`'s language model calls and, with `--jev`, its Jev requests itself, through the `lithos-llm` client; the shim only takes the page snapshot and runs the chosen action.
- Whirl drives browsers through a thin Node shim that Whirl owns: a small, stable JSON API over stdio pipes, shaped like Whirl's closed vocabulary and implemented on the Playwright library. The Rust binary launches the shim as a child process. Whirl does not reimplement browser automation and does not speak CDP or Playwright's internal driver protocol, so it inherits Playwright's auto-waiting, retrying assertions, locator engine, tracing, and three browser engines — and Playwright upgrades stay internal to the shim.
- `whirl install` downloads the pinned shim bundle (a private Node runtime, the shim, and the `@playwright/test` package) and the browser builds. Users do not need Node installed. Each Whirl release pins exactly one Playwright version.
- Rust evaluates every filter and predicate (ADR [evaluate-checks-in-rust](docs/engineering/decisions/evaluate-checks-in-rust.md)). The shim reads raw values: page strings, `eval` results, and each response's status, headers, and body bytes. Rust owns the retry loop for page checks and page captures, on Playwright's poll schedule.
- The shim bundles `@playwright/test` and drives its standalone `expect` for state checks, `tab:name closed`, and `PAGE`: they compile to Playwright's web-first assertions (`toBeVisible`, `toHaveURL`, ...). `SNAPSHOT` runs as a shim-owned poll loop with the same step timeout, because `toHaveScreenshot` runs only inside Playwright's test runner; snapshot comparison uses Playwright's image comparator with the defaults of section 7.

## 16. Errors

- **JSON diagnostics.** `whirl check --json` writes one version 1 JSON document to stdout, containing `exitCode` and `diagnostics`, with no diagnostic text on stderr. Each diagnostic includes a stable code, severity, path, line, column, length, message, and expected alternatives. Positions are 1-based Unicode character positions; locations unavailable for input or I/O errors are null. CLI argument syntax errors still use the ordinary usage message.
- **Parse errors** (exit 2) are reported with file, line, column, a caret under the offending token, and the expected alternatives. `whirl check` surfaces them without launching a browser. Lint warnings do not change the exit code. Whirl warns about a capture that is never used, about an HTTP entry without a `status` check, and about a `count >= 1` assert directly followed by a check on the same locator, only when the following check requires at least one element. A `hidden` check or a count comparison that accepts zero does not make the presence check redundant. `whirl check` reports a check whose types cannot work, such as `text toHex` or `url > 3`, as the error `filter-type`. It reports an invalid literal regex, JSONPath, or XPath as a parse error. A file with an `[Asserts]` or `[Captures]` section is the parse error `sections-removed`, and a file that mixes such sections with check lines is the parse error `mixed-check-syntax` (section 4.1).
- **Test failures** (exit 1) report the failing step the same way, plus expected versus actual and the artifacts. Check failures use the codes of section 9.7.
- **Warnings** do not change a step's status or the exit code. Each has a stable code in the JSON report: `unused-mock` (section 7.5); `cache-miss`, `healed`, `uncached`, `cache-secret`, and `cache-unstable` (section 12.1). `whirl check` reports `cache-stale-entry` as a warning and `cache-invalid`, `ai-count`, `unknown-extract`, `duplicate-extract`, `extract-schema-unsupported`, `judge-without-images`, and `goal-unchecked` as errors, and `extract-unsettled`, `judge-alone`, and `judge-images-unknown` as warnings. A `JUDGE` that answers `unsure` is the step warning `judge-unsure`, and one that answers `no` fails with `judge-false`.
- **Runtime errors** (exit 3) cover shim crashes, missing browsers, and similar environmental failures.

## 17. Grammar

```ebnf
file       = [ options ] , entry , { entry } ;
options    = "[Options]" , { option-line } ;
option-line= key , ":" , value , { value } ;

entry      = browser-entry | http-entry ;
browser-entry = action , { action } , [ page ] , { check-line } ;
http-entry = http-request , { http-check-line } ;
check-line = assert | judge | capture ;
judge      = "JUDGE" , [ locator ] , value , [ step-timeout ] ;   (* prefixed segments only *)
http-check-line = http-assert | http-capture ;

action     = action-body , [ step-timeout ] | snapshot | mock | extract ;
extract    = "EXTRACT" , artifact-name , [ locator ] , value , [ step-timeout ]
           , [ json-object ] ;   (* schema: section 7.6 *)
snapshot   = "SNAPSHOT" , artifact-name , [ locator ] , [ step-timeout ]
           , { snapshot-option } ;   (* prefixed segments only; 6.1 *)
snapshot-option = "snapshot-mask:" , ( locator | "none" )
                | "snapshot-max-diff:" , value
                | "snapshot-pixel-threshold:" , value ;
action-body = "VISIT" , value
           | "RESPONSE" , artifact-name , http-method , value
           | ( "POPUP" | "TAB" | "CLOSE" ) , artifact-name
           | ( "CLICK" | "RIGHTCLICK" | "MIDDLECLICK" ) , locator
           | "DBLCLICK" , locator
           | "FILL" , locator , value
           | "TYPE" , locator , value
           | "PRESS" , [ locator ] , value
           | "CHECK" , locator
           | "UNCHECK" , locator
           | "SELECT" , locator , value
           | "HOVER" , locator
           | "DRAG" , locator , "to" , locator
           | "SCROLL" , locator
           | "SCROLL" , [ locator ] , scroll-motion
           | "UPLOAD" , locator , "file:" , value
           | "DROP" , locator , "file:" , value
           | "SCREENSHOT" , artifact-name
           | "EVAL" , value
           | "ACT" , [ locator ] , value
           | "GOAL" , value   (* default timeout 2 minutes; section 7.7 *)
           | "STORE" , ( "local" | "session" | "cookie" ) , value , value ;

mock       = "MOCK" , http-method , value
           , ( status-code , { http-header } , [ http-body ] | "failed" ) ;
status-code = digit , digit , digit ;   (* 200 to 599 *)

http-request = http-headline , { http-header } , [ http-body ] ;
http-headline = "HTTP" , http-method , value , [ step-timeout ] ;
http-header = attr-name , ":" , value ;
http-body  = json-object | json-array | fenced-text ;

page       = "PAGE" , ( value | "matches" , regex ) , [ step-timeout ] ;

assert     = "ASSERT" , assert-body , [ step-timeout ] ;
assert-body = locator , state-check
           | "tab:" , artifact-name , "closed"
           | subject , { filter } , [ "not" ] , predicate ;
http-assert = "ASSERT" , response-field , { filter } , [ "not" ] , predicate
            , [ step-timeout ] ;
state-check= "visible" | "hidden" | "enabled" | "disabled"
           | "checked" | "unchecked" | "focused" ;

subject    = locator , extractor
           | "url" | "title"
           | "eval" , value
           | "response:" , artifact-name , response-field
           | "request:" , artifact-name , request-field
           | "extract:" , artifact-name ;
request-field = "method" | "url" | "header:" , value | "body" | "bytes"
              | json-path | xpath-expr ;
extractor  = "text" | "value" | "count" | "attr:" , attr-name ;
response-field = "status" | "header:" , value | "location" | "body" | "bytes"
               | json-path | xpath-expr ;

filter     = "count" | "first" | "last" | "nth" , index
           | "split" , value | "regex" , regex
           | "replace" , value , value | "replaceRegex" , regex , value
           | "toString" | "toInt" | "toFloat" | "toHex"
           | "toDate" , value | "dateFormat" , value
           | "daysAfterNow" | "daysBeforeNow"
           | "base64Decode" | "base64Encode"
           | "base64UrlSafeDecode" | "base64UrlSafeEncode"
           | "utf8Decode" | "utf8Encode" | "charsetDecode" , value
           | "urlQueryParam" , value | "urlEncode" | "urlDecode"
           | "htmlEscape" | "htmlUnescape"
           | json-path | xpath-expr ;
json-path  = "json:" , value ;    (* RFC 9535; one bare token or one quoted value *)
xpath-expr = "xpath:" , value ;   (* XPath 1.0; one bare token or one quoted value *)

predicate  = ( "==" | "!=" | ">" | ">=" | "<" | "<="
             | "startsWith" | "endsWith" | "contains" ) , expected
           | "matches" , regex
           | "exists" | "isBoolean" | "isEmpty" | "isFloat" | "isInteger"
           | "isIpv4" | "isIpv6" | "isIsoDate" | "isList" | "isNumber"
           | "isObject" | "isString" | "isUuid" ;
expected   = value | json-literal ;   (* typed reading: section 9.6 *)

capture    = "CAPTURE" , name , ":" , subject , { filter } , [ step-timeout ] ;
http-capture = "CAPTURE" , name , ":" , response-field , { filter } , [ step-timeout ] ;
http-method = uppercase-letter , { uppercase-letter } ;

locator    = segment , { ">>" , segment } ;
segment    = ( "role:" | "role~:" ) , name , [ value ]
           | "ai:" , value     (* the last segment only; 6.3 *)
           | ( "label" | "placeholder" | "text" | "alt"
             | "title" ) , [ "~" ] , ":" , value
           | ( "testid:" | "css:" | "frame:" ) , value
           | "nth:" , index    (* never the first segment *)
           | value ;             (* default engine; actions only — see 6.1 *)

step-timeout = "@" , duration ;
scroll-motion = "down" | "up" | "left" | "right" | "to" , percent ;
percent    = digit , { digit } , [ "." , digit , { digit } ] , "%" ;   (* 0% to 100% *)
value      = quoted-string | bare-token ;
name       = letter-or-underscore , { letter-digit-underscore } ;
artifact-name = letter-or-underscore , { letter-digit-underscore | "-" } ;
attr-name  = letter-or-underscore , { letter-digit-underscore | "-" } ;
regex      = "/" , pattern , "/" , [ flags ] ;
index      = [ "-" ] , digit , { digit } ;
json-literal = json-array | json-object ;   (* on one line; section 3.1 *)
```

`json-object` and `json-array` are JSON values. In an HTTP body, the outer
delimiter can span lines; in a `json-literal`, the value ends on the same line.
Interpolation is permitted as defined in sections 7.3 and 11. `fenced-text` is
the text between lines that contain only three backticks. Comments and blank
lines may appear between any two structural lines and are not part of the
grammar. Inside a body they are body text.

## 18. Non-goals and deferred features

Permanent non-goals — these keep the format Hurl-grade:

- Conditionals, loops, functions, includes, or user-defined keywords.
- Arbitrary JavaScript woven into the language. `EVAL` (section 7) is the single, explicit escape hatch: Whirl passes its script to the browser without reading it, and offers no way to branch on the result.
- A programming language of Whirl's own. The format has no Whirl-native expressions, conditionals, or control flow; when a flow outgrows Whirl, the answer is Playwright itself. CSS selectors, ECMAScript regex, JSONPath (RFC 9535), and XPath 1.0 are standard query languages that Whirl accepts as values. They select data; they add no control flow.

Deferred beyond V1 (candidate V2 features, not promised):

- Request-count assertions, and `MOCK` responses served from a HAR file.
- The Hurl features that the check vocabulary does not adopt: the `sha256`, `md5`, `cookie`, `certificate`, `redirects`, `duration`, `ip`, `version`, `variable`, and `rawbytes` queries; `file,…;` values; and following redirects in HTTP entries.
- Per-entry `[Options]` overrides and mobile device emulation.
- Retrying a `JUDGE` that answers `no`, and a `JUDGE` that sees only a screenshot.
- A step-two prompt for `ACT` that sends only the part of the snapshot that changed.
- External schema files for `EXTRACT`, and a cache for `EXTRACT` results.
- `--jev` for `ai:` targets, segments after an `ai:` segment, and `ai:` with `count`.
- Branches in a cached `GOAL` path, `--jev` for `GOAL`, and later `GOAL` calls that send only the part of the snapshot that changed.
