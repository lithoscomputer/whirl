# Changelog

## Unreleased

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
