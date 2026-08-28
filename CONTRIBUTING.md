# Contributing to Whirl

## Setup

Install [mise](https://mise.jdx.dev) and [rustup](https://rustup.rs), then:

```console
$ mise run setup   # toolchains, shim dependencies, Playwright Chromium
$ mise run check   # the full verification gate — must pass before a PR
```

`mise run check` runs formatting, Clippy, the Rust test suite (including
real-browser acceptance tests), the shim's type check, lint, and tests, and
a zizmor audit of the CI workflows. Checks print nothing when they pass;
set `WHIRL_VERBOSE=1` to stream tool output.

## Layout

- `crates/whirl` — the Rust binary: parser, formatter, lints, runner,
  reporters, `whirl install`.
- `shim/` — the TypeScript browser shim on the Playwright library. It runs
  on a pinned Node runtime; Bun is the package manager and script runner.
- `docs/engineering/shim-protocol.md` — the JSON-over-stdio contract
  between the two.

## Document authority

- [SPEC.md](SPEC.md) defines product behavior and the file format.
- Accepted ADRs in [docs/engineering/decisions](docs/engineering/decisions)
  define architecture and repository policy. Amend or add an ADR before
  changing policy.
- Implementation defaults come from the
  [Rust](https://github.com/brynary/rust-style-guide) and
  [TypeScript](https://github.com/brynary/typescript-style-guide) style
  guides; the SPEC and ADRs override them.

Do not resolve a conflict between these documents silently — raise it.

## Tests

Per the [CLI acceptance tests
ADR](docs/engineering/decisions/cli-acceptance-tests.md): acceptance tests
invoke the `whirl` binary as a process. Pure stdout/stderr/exit-code cases
are `trycmd` cases in `crates/whirl/tests/cmd/`; browser behavior is tested
against the local site in `crates/whirl/tests/site/`. Unit tests cover
parsing, formatting, lints, and the shim protocol.

Regenerate trycmd snapshots after intentional CLI output changes:

```console
$ TRYCMD=overwrite cargo test -p whirl --test cli_acceptance
```

Review regenerated snapshots by eye before committing.
