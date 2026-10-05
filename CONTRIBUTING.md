# Contributing to Whirl

## Setup

Install [mise](https://mise.jdx.dev) and [rustup](https://rustup.rs). The
libxml2 build also needs a C compiler, `make`, and libclang (Xcode Command
Line Tools on macOS; `build-essential` and `libclang-dev` on Debian and
Ubuntu). Then:

```console
$ mise run setup   # toolchains, shim dependencies, Playwright Chromium
$ mise run check   # the full verification gate — must pass before a PR
```

`mise run check` runs formatting, Clippy, the Rust test suite (including
real-browser acceptance tests), the shim's type check, lint, and tests, and
a zizmor audit of the CI workflows. Checks print nothing when they pass;
set `WHIRL_VERBOSE=1` to stream tool output.

Every build links a pinned libxml2 statically. `mise run build:libxml2`
builds it from source into `target/libxml2`, and the Mise tasks that build
Rust run it first. Mise also points pkg-config at it, so run Cargo inside
the Mise environment (`mise exec -- cargo …` or an activated shell).

## Layout

The Rust code is a workspace of nine crates under `crates/`. ADR
`crate-layout` records what each owns and the rule that dependencies point
down only.

- `crates/whirl` — the binary: CLI, exit codes, telemetry, `whirl install`,
  `whirl doctor`, and every acceptance test.
- `crates/whirl-run` — the runner: flows, steps, variables, the AI cache,
  and artifacts.
- `crates/whirl-ai` — language model calls for `ACT`, `GOAL`, `ai:`,
  `EXTRACT`, and `JUDGE`.
- `crates/whirl-check` — the check engine: filters, predicates, and literal
  validation.
- `crates/whirl-report` — the run report model and its renderers.
- `crates/whirl-shim` — the browser shim client, wire format, launch
  resolution, and bundle layout.
- `crates/whirl-lang` — the `.whirl` language: AST, parser, formatter, lints,
  and option resolver.
- `crates/whirl-types` — the values and static types that `whirl-lang` and
  `whirl-check` share.
- `crates/whirl-xpath` — XPath 1.0 through libxml2, the one crate allowed
  `unsafe` code (ADR `evaluate-checks-in-rust` §1.6).
- `shim/` — the TypeScript browser shim on the Playwright library. It runs
  on a pinned Node runtime; Bun is the package manager and script runner.
- `docs/engineering/shim-protocol.md` — the JSON-over-stdio contract
  between the two.
- `evals/act` — model evals for `ACT`, run by `mise run eval:act`. They call
  real models and cost money, so no check runs them; `mise run test:evals`
  checks their flows and script. See [evals/act/README.md](evals/act/README.md).

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
against the local site in `crates/whirl/tests/site/`. Unit tests live in
the crate that owns the code they test, such as parsing, formatting, and
lints in `crates/whirl-lang` and the shim protocol in `crates/whirl-shim`.

Regenerate trycmd snapshots after intentional CLI output changes:

```console
$ TRYCMD=overwrite cargo test -p whirl --test cli_acceptance
```

Review regenerated snapshots by eye before committing.
