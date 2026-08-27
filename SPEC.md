# Whirl V1 Specification

Status: draft
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
| `storage` | file path | none | Saved storage state loaded into each file's browser context |

`allow-hosts` takes one or more host globs (`allow-hosts: example.com *.example.com`). Globs match the request's hostname only — scheme and port are ignored — and `*.example.com` does not match the apex `example.com`; list both to cover both. The `base` host is always allowed. Whirl aborts requests to any other host, including fetch/XHR, WebSockets, and subresources, and the reports list every blocked host. Service workers are disabled when `allow-hosts` is set, because they can bypass request routing. IP-literal hosts match textually; `data:` and `blob:` URLs have no host and are always allowed. Without the option, all hosts are allowed.

`storage` names a Playwright storageState JSON file, resolved relative to the `.whirl` file. Each browser context starts from that saved state (cookies and local storage) instead of empty, so flows can skip UI login. Produce the file with `--save-storage`, which writes the final context state of a successful run — typically of a dedicated login flow.

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
| `css:"selector"` | `locator('selector')` — escape hatch |
| `nth:N` | `.nth(N - 1)` — 1-based position |

Text matching is exact (after whitespace normalization). For partial or pattern matching, assert on the element instead (`text contains`, `text matches`).

Every text-matching prefix has a substring variant marked with `~` — `role~:`, `label~:`, `placeholder~:`, `text~:`, `alt~:`, `title~:` — which matches by case-insensitive substring, Playwright's default matching. So `text~:"Added"` matches "Added to cart". `testid:` and `css:` have no `~` form, and the unprefixed default engine stays exact.

An unprefixed value in locator position selects a default engine: `label:` for form actions (`FILL`, `SELECT`, `CHECK`, `UNCHECK`, `UPLOAD`, and `PRESS` with a target), and `text:` for pointer actions (`CLICK`, `DBLCLICK`, `HOVER`) — buttons and links have no label; their accessible name is their text. So `FILL "Email" alice@example.com` fills the input labeled Email, and `CLICK "Add to cart"` clicks the element with that exact text. Prefixes stay available everywhere for precision. Default engines exist only in actions: in `[Asserts]` and `[Captures]` every segment must carry a prefix (or be `nth:`), and an unprefixed value there is a parse error.

### 6.2 Strictness

When an action or a single-element check runs, the locator must resolve to exactly one element. Zero matches fails after the timeout — except the `hidden` check, which passes when nothing matches (section 9.1). More than one match fails immediately with the candidate list, for `hidden` as well. Narrow the locator or add `nth:`. Only `count` accepts any number of matches.

## 7. Actions

An action is a verb, an optional locator, and an optional value. Element-targeting actions use Playwright's actionability checks for that operation up to the applicable timeout. A trailing `@duration` overrides the step timeout for that line (section 12).

| Syntax | Meaning |
| --- | --- |
| `VISIT url` | Navigate. A `url` starting with `/` resolves against `base`. |
| `CLICK locator` | Click the element. |
| `DBLCLICK locator` | Double-click the element. |
| `FILL locator "text"` | Replace the input's content with `text`. |
| `PRESS "Key"` | Send a key or chord (Playwright key names, for example `"Enter"`, `"Control+A"`) to the focused element. |
| `PRESS locator "Key"` | Focus the element, then send the key. |
| `CHECK locator` | Set a checkbox or radio to checked. |
| `UNCHECK locator` | Set a checkbox to unchecked. |
| `SELECT locator "Label"` | Choose the `<select>` option with visible text `Label`. |
| `HOVER locator` | Move the pointer over the element. |
| `UPLOAD locator file:path` | Set the file input to `path`, resolved relative to the `.whirl` file. |
| `SCREENSHOT name` | Save a full-page screenshot as artifact `name.png`. Never fails the entry (see below). |
| `SNAPSHOT name` | Compare a full-page screenshot against the stored baseline; fails the entry on visual difference. |
| `EVAL "script"` | Run a JavaScript script in the page. The escape hatch; rules below. |

`PRESS` with a single argument treats it as the key: `PRESS Enter` presses Enter on the focused element, even though `Enter` could also parse as a locator. Only when two arguments are present is the first a locator.

`SCREENSHOT` never fails the entry, even when it goes wrong: if the capture or the file write fails — a crashed page, an I/O error, or its step timeout expiring — Whirl skips the artifact and records a warning naming the screenshot and the cause, in the console output and in both reports, so a missing artifact is always explained. One cap outranks this: an expiring `entry-timeout` fails the entry as usual, whatever line is in flight.

`SNAPSHOT` is retried like an assert through a shim-owned polling loop and uses Playwright's image comparator; V1 exposes no tuning knobs. Whirl captures frames until two consecutive frames are identical, then compares against the baseline at `<flow>.whirl-snapshots/<name>-<browser>-<platform>.png` next to the flow file. Images match only when their dimensions are identical and no pixel differs; a pixel differs when its color distance exceeds the comparator's default per-pixel threshold (0.2 on a 0–1 scale), which absorbs invisible anti-aliasing noise and nothing more. On a mismatch Whirl recaptures and recompares until the step timeout, so a difference that settles late can still pass; a stable mismatch fails the entry when the timeout expires and writes the actual and diff images to the artifacts directory. The platform tag (`linux`, `darwin`, `win32`) keeps baselines rendered on one OS from failing on another; the viewport is not part of the key, because the flow's `viewport` option already pins it. A missing baseline fails the run; `--update-snapshots` writes or refreshes baselines instead of comparing.

`EVAL` is the JavaScript escape hatch — the `css:` of actions — and the one place Whirl runs code it does not read: a script of one or more statements, such as `EVAL "foo(); bar();"`. Whirl runs the script as the body of an async function in the page's main world through Playwright's `page.evaluate`: a script that parses as a single expression runs as `return (expression);`, so its value is the result; any other script runs as written and yields its `return` value, or `undefined` without one. `await` is available in both forms, and a returned Promise is awaited. A syntax error, a thrown exception, a rejected Promise, or the step timeout fails the entry; the action form discards the result. `EVAL` has no target, does not auto-wait, and does not retry: it runs once, after the preceding line completes. `{{name}}` interpolation happens textually before evaluation, so interpolated values become source text — and a value sent into the page escapes the output masking of section 11, so keep secrets out of `EVAL`. A script the page cannot cancel — one that blocks the renderer or never settles — is still bounded: section 12 defines how Whirl enforces timeouts from outside the page.

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

An `[Asserts]` section holds one check per line. Checks run in order. Every check retries until it passes or the step timeout expires; the first check that times out fails the entry.

```
assert  := subject check
subject := locator | "url" | "title"
```

### 9.1 Element state checks

`visible`, `hidden`, `enabled`, `disabled`, `checked`, `unchecked`, `focused`.

`hidden` passes when the element is not visible, including when it does not exist. All others require the element to exist.

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

## 10. Captures

A `[Captures]` section extracts values into variables for later entries.

```
capture   := name ":" source ["regex" /re/]
source    := locator extractor | "url" | "title" | "eval" value
extractor := "text" | "value" | "count" | "attr:" NAME
```

- `name` matches `[A-Za-z_][A-Za-z0-9_]*`.
- The optional `regex` filter applies the pattern to the extracted string and stores capture group 1 (the whole match if there is no group). No match fails the entry.
- Extraction waits like an assert: `text`, `value`, and `attr:` wait for the locator to resolve to exactly one element, up to the step timeout. Once the element resolves, an absent attribute fails the entry — it does not wait further and does not become an empty value. `count` never waits: it records the current number of matches immediately, and zero is a valid result; assert a `count` first when the flow must wait for elements to appear.
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

`{{env.NAME}}` reads the environment variable `NAME` at run time. This is the intended path for secrets; secret values never belong in `.whirl` files. A reference to an undefined variable or unset environment variable fails the step.

Whirl masks every value sourced from `env.*` in the textual output it generates: console failure details, rendered step text, the JSON and JUnit reports, and trace step titles. Browser-recorded artifacts — screenshots, video, HAR files, and saved storage state — can still contain secrets the flow typed or received. Treat the artifacts directory and storage-state files as sensitive, and prefer dedicated test credentials.

## 12. Execution model

- **Isolation.** Each file runs in a fresh browser context with its own single page. Without the `storage` option the context starts empty; with it, the context starts from the saved storage state. Files never share live state either way.
- **Order.** Entries run top to bottom. Within an entry: actions, then `PAGE`, then asserts, then captures.
- **Failure.** The first failing step fails the entry, and a failed entry stops its file; remaining entries in that file are skipped and reported as skipped. Other files still run. On failure Whirl saves a full-page screenshot and, with `--trace`, a Playwright trace to the artifacts directory.
- **Timeouts.** Each action, PAGE, assert, and capture line gets the step timeout (`step-timeout` option, default 10s); `VISIT` gets the navigation timeout (`nav-timeout` option, default 30s). A trailing `@duration` on any such line overrides its own budget: `CLICK "Generate report" @60s`. The optional `entry-timeout` option caps an entry's total time across all of its lines; when it expires, the in-flight step fails with an entry-timeout error. An entry without one is still bounded by its per-step timeouts. The suffix must be bare: a line’s final bare token of the form `@duration` is always its timeout, and a quoted `"@60s"` is an ordinary value. Timeouts are enforced from outside the page, so they hold even when the page cannot respond — an `EVAL` script blocking the renderer or returning a Promise that never settles. When a timed-out step cannot be cancelled cleanly, Whirl closes that flow's browser context; if closing also stalls, it terminates and restarts only that worker's shim process. Either way the flow fails and reports normally, and other files are unaffected.
- **Parallelism.** Files run in parallel across worker slots (`--jobs`, default: logical CPU count). A single file is never parallelized.
- **Dialogs.** `alert`, `confirm`, and `prompt` dialogs are auto-dismissed by default. The `dialogs: accept` option auto-accepts them instead.

## 13. Command line

```
whirl [OPTIONS] <PATH>...        Run files; directories recurse to *.whirl
whirl check <PATH>...            Parse and lint only; nothing runs
whirl install                    Provision the shim bundle and browsers
whirl fmt [--check] <PATH>...    Rewrite files to canonical form
```

`whirl fmt` rewrites files to the canonical form: single spaces between tokens, quotes only where a value requires them, and one blank line between entries. `--check` writes nothing and exits with code 1 when any file would change.

| Flag | Meaning |
| --- | --- |
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
| `--fail-fast` | Stop scheduling new files after the first failure |
| `--update-snapshots` | Write or refresh SNAPSHOT baselines instead of comparing |
| `--video` | Record a .webm video of each file's run into the artifacts directory |
| `--har` | Record a .har network log per file into the artifacts directory |
| `--storage PATH` | Override the storage option |
| `--save-storage PATH` | Write the final storage state after a successful run (single file only) |
| `--entry-timeout DURATION` | Override the entry-timeout option |

Exit codes:

| Code | Meaning |
| --- | --- |
| 0 | All files passed |
| 1 | The command's negative result: a failed entry (run), formatting drift (`fmt --check`) |
| 2 | Parse or lint error |
| 3 | Runtime error (browser or shim failure) |
| 4 | Usage error |

Whirl parses and lints every input file before it launches any browser: a parse or lint error anywhere stops the invocation with exit 2 and nothing runs. When one invocation hits several categories, the highest applicable code wins — a usage error (4) is detected before parsing and preempts everything, and within a run a runtime error (3) outranks failed entries (1), which outrank 0.

## 14. Output and reports

- Default console output: one line per file with pass/fail and duration, then a failure detail block per failed entry: file, line, the failing step, expected versus actual, and the artifact paths.
- The JUnit report maps one file to one test suite and one entry to one test case. An entry is named by the nearest comment line above it (`# Log in.`), falling back to its first action line and line number. A failure before the first entry — option resolution, storage loading, or browser launch — reports as a synthetic test case named `[setup]` in that file's suite: an `<error>` for runtime errors, a `<failure>` otherwise. The JSON report carries the same synthetic entry, and masking applies to it like any other output.
- The JSON report is the machine-readable superset: per-step timing, captures (with values sourced from `env.*` masked), and artifact paths.
- Artifacts: each flow writes to `<artifacts>/<flow path without the .whirl extension>/`. The mirrored path is the flow file's canonical path (made absolute, symlinks resolved) relative to the current working directory, so parallel flows never collide, and overlapping inputs or symlinked duplicates of one file resolve to one flow, run once, and write to one directory. A flow outside the working directory writes to `<file stem>-<hash>/` instead, where `<hash>` is the first 16 hex digits of the SHA-256 of the canonical path, so absolute paths and `..` segments never escape the artifacts directory. Because all inputs are known before the run starts, Whirl verifies that no two flows map to the same directory; a collision is a runtime error. Names inside are fixed: screenshots by their given name, `snapshot-<name>-actual.png` and `snapshot-<name>-diff.png`, `failure.png`, `trace.zip`, `video.webm`, and `network.har`. Duplicate `SCREENSHOT` or `SNAPSHOT` names within one flow are a lint error.

## 15. Architecture

- `whirl` is a single Rust binary containing the parser, the runner, the reporters, and the shim manager.
- Whirl drives browsers through a thin Node shim that Whirl owns: a small, stable JSON API over stdio pipes, shaped like Whirl's closed vocabulary and implemented on the Playwright library. The Rust binary launches the shim as a child process. Whirl does not reimplement browser automation and does not speak CDP or Playwright's internal driver protocol, so it inherits Playwright's auto-waiting, retrying assertions, locator engine, tracing, and three browser engines — and Playwright upgrades stay internal to the shim.
- `whirl install` downloads the pinned shim bundle (a private Node runtime, the shim, and the `@playwright/test` package) and the browser builds. Users do not need Node installed. Each Whirl release pins exactly one Playwright version.
- The shim bundles `@playwright/test` and drives its standalone `expect` for retried checks: they compile to Playwright's web-first assertions (`toHaveText`, `toHaveCount`, `toHaveURL`, ...) where a usable one exists, so retry timing and regex semantics match Playwright's. Checks with no usable web-first assertion — count comparators other than `==`, negated attribute checks, and `SNAPSHOT`, whose `toHaveScreenshot` runs only inside Playwright's test runner — run as shim-owned poll loops with the same step timeout; snapshot comparison uses Playwright's image comparator with the defaults of section 7.

## 16. Errors

- **Parse errors** (exit 2) are reported with file, line, column, a caret under the offending token, and the expected alternatives. `whirl check` surfaces them without launching a browser. Lint warnings (for example a capture that is never used) do not change the exit code.
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
           | "CLICK" , locator
           | "DBLCLICK" , locator
           | "FILL" , locator , value
           | "PRESS" , [ locator ] , value
           | "CHECK" , locator
           | "UNCHECK" , locator
           | "SELECT" , locator , value
           | "HOVER" , locator
           | "UPLOAD" , locator , "file:" , value
           | "SCREENSHOT" , name
           | "SNAPSHOT" , name
           | "EVAL" , value ;

page       = "PAGE" , ( value | "matches" , regex ) , [ step-timeout ] ;

asserts    = "[Asserts]" , { assert } ;
assert     = assert-body , [ step-timeout ] ;
assert-body = locator , state-check
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
source     = locator , extractor | "url" | "title" | "eval" , value ;
extractor  = "text" | "value" | "count" | "attr:" , attr-name ;

locator    = segment , { ">>" , segment } ;
segment    = ( "role:" | "role~:" ) , name , [ value ]
           | ( "label" | "placeholder" | "text" | "alt"
             | "title" ) , [ "~" ] , ":" , value
           | ( "testid:" | "css:" ) , value
           | "nth:" , number   (* N >= 1; never the first segment *)
           | value ;             (* default engine; actions only — see 6.1 *)

step-timeout = "@" , duration ;
value      = quoted-string | bare-token ;
name       = letter-or-underscore , { letter-digit-underscore } ;
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

- iframe locators (`frame:` segment).
- Multi-tab and popup handling.
- Network stubbing and request assertions.
- Per-entry `[Options]` overrides and mobile device emulation.
- An LLM-as-judge assertion (a `JUDGE` keyword with an explicit model option and advisory rather than hard-failing verdicts).
