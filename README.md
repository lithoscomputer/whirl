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

```console
$ whirl flows/checkout.whirl
$ whirl --report-junit report.xml flows/
```

## Why

- **Closed vocabulary.** A fixed set of actions, checks, and extractors. No
  conditionals, no loops, no user-defined keywords. A flow that needs
  branching is two files.
- **No waits in the language.** Actions auto-wait for their target and
  assertions retry until they pass or time out. There is no `SLEEP`.
- **Semantic locators first.** `role:button "Sign in"` and `label:"Email"`
  up front; raw CSS is the visually distinct escape hatch.
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
$ whirl install
```

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
[Asserts]
role:heading "Example Domain" visible
title contains "Example"
```

Run it:

```console
$ whirl example.whirl
example.whirl passed (0.4s)
```

More commands:

```console
$ whirl check flows/        # parse and lint only; nothing runs
$ whirl fmt flows/          # rewrite files to the canonical form
$ whirl --headed flow.whirl # watch the browser
$ whirl --trace flow.whirl  # save a trace when the flow fails
$ whirl show-trace whirl-artifacts/flow/trace.zip
```

See [examples/](examples/) for more flows.

## Secrets

Reference secrets with `{{env.NAME}}`; Whirl masks every env-sourced value
in its console output, reports, and trace step titles. Browser-recorded
artifacts — screenshots, video, HAR files, and saved storage state — can
still contain secrets the flow typed or received. Treat the artifacts
directory as sensitive and prefer dedicated test credentials.

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
