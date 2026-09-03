# Changelog

## Unreleased

- Add `TYPE locator "text"`, which sends one key event per character for
  inputs that ignore a plain `FILL`, such as segmented one-time-code fields.
- Add `STORE local "key" "value"`, which writes one `localStorage` entry on
  the current origin so a flow can skip onboarding screens and dismissed
  banners without `EVAL`.

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
