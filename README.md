# Whirl

Whirl runs web UI tests written in plain text files. It is to browser flows
what [Hurl](https://hurl.dev/) is to HTTP: a tight, closed, file-based format
that is readable, diffable, and easy to generate. The `whirl` binary is
written in Rust and drives real browsers (Chromium, Firefox, WebKit) through
Playwright.

```whirl
# checkout.whirl — buy a widget as a signed-in user.
[Options]
base: https://shop.example.com

VISIT /login
FILL "Email" alice@example.com
FILL "Password" {{env.TEST_PASSWORD}}
CLICK button:"Sign in"
PAGE /dashboard
ASSERT heading:"Welcome back" visible
ASSERT testid:user-menu text == Alice

# Find a product.
FILL placeholder:"Search products" widget
PRESS Enter
ASSERT url contains "q=widget"
ASSERT testid:result-card count >= 1
CAPTURE first_product: testid:result-card >> nth:0 >> link:* attr:href

# Add it to the cart.
VISIT {{first_product}}
CLICK "Add to cart"
ASSERT testid:cart-badge text == 1
ASSERT alert:* text contains "Added to cart"
```

```console
$ whirl run flows/checkout.whirl
$ whirl run --report-junit report.xml flows/
```

## Why

- **Closed vocabulary.** A fixed set of actions, checks, and extractors. No
  conditionals, no loops, no user-defined keywords. A flow that needs
  branching is two files.
- **No waits in the language.** Actions auto-wait for their target and
  assertions retry until they pass or time out. There is no `SLEEP`.
- **Semantic locators first.** `button:"Sign in"` and `label:"Email"` up
  front; raw CSS is the visually distinct escape hatch.
- **Plain text.** Flows review well in a diff and are easy for people and
  machines to write.

The full language — every action, check, capture, option, and the grammar —
is specified in [SPEC.md](SPEC.md).

## Install

With [Homebrew](https://brew.sh):

```console
$ brew install lithoscomputer/tap/whirl
```

Or download a binary from the
[releases page](https://github.com/lithoscomputer/whirl/releases).

Then provision the browser runtime (a pinned Node runtime, the Playwright
package, and the browser builds — no Node installation required on your
machine):

```console
$ whirl install chromium
$ whirl doctor
```

Use `whirl install` to provision all three engines, or name the engines you
need: `whirl install chromium firefox`. `whirl doctor --browser firefox` checks
that engine and gives repair commands, including missing Linux libraries.

### Supported platforms

| Platform | Status |
| --- | --- |
| macOS (Apple silicon) | Supported |
| Linux x86_64 | Supported |
| Linux arm64 | Supported |
| Windows | Not supported in v1; `whirl install` exits with a clear error |

## Quickstart

Write a flow:

```whirl
# example.whirl
VISIT https://example.com
ASSERT heading:"Example Domain" visible
ASSERT title contains "Example"
```

Run it:

```console
$ whirl run example.whirl
example.whirl passed (0.4s)
```

Use an HTTP entry to create fixture data before the browser starts its flow:

```whirl
[Options]
base: https://shop.example.com

HTTP POST /api/test-fixtures/users
Authorization: "Bearer {{env.E2E_SETUP_TOKEN}}"
{
    "name": "Ada"
}
ASSERT status == 201
CAPTURE user_id: json:$.id

VISIT /users/{{user_id}}
ASSERT heading:Ada visible
```

The status check is explicit. Whirl does not treat 2xx as implicit success.

Checks use Hurl's vocabulary. A check reads a value, passes it through
filters, and tests it with one predicate. JSON values keep their type,
JSONPath selects them, and XPath reads HTML and XML:

```whirl
ASSERT url urlQueryParam page == 2
ASSERT testid:price text replaceRegex /[^0-9.]/ "" toFloat < 200
ASSERT response:order json:$.items[*].sku contains ABC-1
ASSERT response:order json:$.id isInteger
ASSERT response:feed xpath:"count(//_:entry)" >= 1
ASSERT eval "window.dataLayer" json:$[?@.event=='purchase'] count == 1
```

Page checks retry until they pass or time out. See
[SPEC section 9](SPEC.md#9-asserts) for every subject, filter, and predicate.

Use `MOCK` to give the page a fixed answer, and `request:NAME` to check what
the page sent:

```whirl
MOCK GET /api/flags 200
{ "checkout_v2": true }
MOCK GET https://fonts.example.com/* failed

VISIT /checkout
MOCK POST /api/cart 201
CLICK "Add to cart"
RESPONSE cart POST /api/cart
ASSERT request:cart json:$.qty == 1
```

A mock serves every matching browser request until the file ends. `*`
matches any run of characters. See [MOCK](SPEC.md#75-mock).

Use `ACT` when a step is easier to describe than to locate. A language model
reads a snapshot of the page and chooses one action, which Whirl runs like
any other action:

```whirl
[Options]
model: anthropic/claude-sonnet-5

VISIT https://shop.example.com/products
ACT "add the first product to the cart"
ASSERT testid:cart-badge text == 1
```

Set the provider's key, such as `ANTHROPIC_API_KEY`. The model's choice can
change between runs, so assert the result. `{{env.NAME}}` values in an
instruction reach the model only as placeholders. See [ACT](SPEC.md#74-act).

`GOAL` lets the model run several actions, one at a time, until it says the
goal is done. Follow it with an `ASSERT` that checks the result:

```whirl
GOAL "add two blue mugs to the cart and open the cart"
ASSERT testid:cart-badge text == 2
```

A `GOAL` runs at most 20 actions within 2 minutes. `@duration` changes the
time. See [GOAL](SPEC.md#77-goal).

An `ai:` segment names one element in words, wherever a locator goes:

```whirl
CLICK ai:"the Add to cart button for the first product"
ASSERT dialog:* >> ai:"the order total" text == "$42.00"
```

Whirl writes what each `ai:` target, `ACT` line, and `GOAL` line resolved to
in `<flow>.whirl-cache.json`, next to the flow. Commit it. Later runs replay
it without a model call, and a step whose page changed heals with a warning:

```console
$ whirl run --cache=update flows/  # resolve with the model and write the cache
$ whirl run flows/                 # replay; heal a miss with a warning
$ whirl run --cache=only flows/    # fail a miss instead of asking the model
```

See [AI targets](SPEC.md#63-ai-targets) and [the AI cache](SPEC.md#121-the-ai-cache).

`EXTRACT` reads a value from the page into a typed variable, shaped by an
optional JSON Schema, for later checks:

```whirl
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
```

See [EXTRACT](SPEC.md#76-extract).

`JUDGE` asks the model whether a claim about the page holds. The model sees a
screenshot and the page outline. Put an `ASSERT` first that waits for the
state the claim describes, because `JUDGE` does not retry:

```whirl
ASSERT testid:summary visible
JUDGE testid:summary "the total matches the sum of the line items"
JUDGE "the page shows no error message"
```

`yes` passes, `no` fails with the model's reason, and `unsure` passes with a
warning. The model must accept images. See [JUDGE](SPEC.md#98-judge).

More commands:

```console
$ whirl check flows/        # parse and lint only; nothing runs
$ whirl check --json flows/ # diagnostics for editors and agents
$ whirl run --rerun-failed report.json --trace
$ whirl fmt flows/          # rewrite files to the canonical form
$ whirl run --headed flow.whirl  # watch the browser
$ whirl run --trace flow.whirl   # save a trace when the flow fails
$ whirl run --video --report-html evidence.html flows/
$ whirl run --video --video-fps 30 flows/  # lighter recordings; Chromium records at 60 by default
$ whirl show-trace whirl-artifacts/flow/trace.zip
```

Run the complete [sample shop](examples/shop/README.md) with
`mise run example:test`. It covers login, validation, shared setup, snapshots,
and CI reports. See [examples/](examples/) for more flows.

### Snapshot comparison settings

Set defaults in `[Options]`, then override each setting below a `SNAPSHOT`:

```whirl
[Options]
snapshot-mask: testid:clock
snapshot-max-diff: 0.1%
snapshot-pixel-threshold: 0.2

VISIT /dashboard
SNAPSHOT dashboard
snapshot-max-diff: 20

SNAPSHOT unmasked
snapshot-mask: none

# Compare only the cart.
SNAPSHOT cart testid:cart
snapshot-mask: testid:cart >> testid:delivery-estimate
snapshot-max-diff: 0.5%
```

A locator after the name captures only that element. It must match exactly one
element, and every segment needs a prefix, such as `testid:` or `region:`. An
element snapshot checks what the element looks like and its size. It does not
check where the element is on the page; use a full-page snapshot for that.

`snapshot-mask` covers matching elements with pink. Repeat it for several
locators. A local list replaces the file list; `none` clears it. Masks search
the whole page, not only the element.
`snapshot-max-diff` permits a pixel count or a percentage of the captured
image: the full page, or the element.
`snapshot-pixel-threshold` sets the color difference that counts as a changed
pixel. Omitted options inherit file defaults. The built-in defaults are no
masks, zero different pixels, and a pixel threshold of `0.2`. Image dimensions
must always match. Masks do not redact other screenshots, traces, or video.

## HTML reports

Create a portable browser test report with recordings and screenshots:

```console
$ whirl run --video --report-html evidence.html flows/
$ whirl run --video --report-html evidence.html --report-metadata context.json flows/
```

The HTML opens offline. It shows results, execution checkpoints, failure details,
and browser recordings. Missing recordings are shown separately from test status.
Use optional [report metadata](SPEC.md#141-html-reports) to add a title, scope note,
and flow descriptions. Metadata paths resolve relative to the metadata file.

Save JSON and artifacts once, then regenerate HTML without repeating browser actions:

```console
$ whirl run --video --report-json report.json --out run-artifacts flows/
$ whirl report report.json --html evidence.html
$ whirl report report.json --html evidence.html --metadata revised-context.json
```

For the saved command, metadata flow keys match the JSON's `files[].path` exactly. Source files and a browser installation are not required. Relative media paths use the original working directory; use `--working-directory DIR` for a copied project tree. Keep the original artifacts to embed them again, and use a distinct artifact location for each run.

Combine saved runs and show missing scenarios:

```sh
$ whirl report first.json second.json --expected expected.json --html evidence.html
```

`expected.json` is an ordered array of recorded flow paths, for example `["flows/login.whirl", "flows/checkout.whirl"]`. Whirl selects the latest recorded attempt for each path, including failures. It uses execution timestamps, never file modification times. Missing expected scenarios show **Not run**. Setup attempts and other flows appear separately. Each attempt retains its source report and provenance. Combined evidence does not establish one complete suite pass. See [selection rules](SPEC.md#144-combined-evidence-and-expected-scenarios).

Use metadata `details` to attach application commits or fixture checksums as labeled text. `--report-metadata context.json --report-json report.json` saves that context without rendering HTML.

New reports record UTC start and finish times, the SHA-256 of each parsed flow, and whether each flow was requested, used as setup, or both. These details survive copied JSON files and regenerated HTML. See [run records](SPEC.md#143-run-records) for their scope.

New reports record UTC start and finish times, the SHA-256 of each parsed flow, and whether each flow was requested, used as setup, or both. These details survive copied JSON files and regenerated HTML. See [run records](SPEC.md#143-run-records) for their scope.

## Secrets

Reference secrets with `{{env.NAME}}`; Whirl masks every env-sourced value
in its console output, reports, and trace step titles. `ACT` sends such values
to its model only as placeholders, but the page snapshot it sends includes any
text the page shows. Browser-recorded
artifacts — screenshots, video, HAR files, and saved storage state — can
still contain secrets the flow typed or received. Treat the output
directory and HTML reports containing embedded media as sensitive, and prefer
dedicated test credentials.

## Development

Development is driven by [mise](https://mise.jdx.dev):

```console
$ mise run setup   # toolchains, shim dependencies, browsers
$ mise run check   # the full verification gate
$ mise run dev     # build the shim and the whirl binary
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the layout, the document
authority rules, and the test policy.

## License

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE)
or the [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

## Acknowledgments

Whirl drives every browser through [Playwright](https://playwright.dev), by
Microsoft. Its locators, auto-waiting, tracing, and browser builds do much of
the work under each flow.

`ACT` is modeled on the `act()` method of
[Stagehand](https://github.com/browserbase/stagehand), by Browserbase.
Whirl's `ACT` prompts and parts of its Jev planner are ported from Stagehand
under its MIT license, and some `ACT` evals run on pages saved from
Stagehand's eval sites.
