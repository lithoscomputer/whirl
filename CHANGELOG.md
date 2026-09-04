# Changelog

## Unreleased

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
