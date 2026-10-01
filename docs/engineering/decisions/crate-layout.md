---
status: draft
---

# Split Whirl into crates with one responsibility each

Whirl's Rust code is a workspace of nine crates. Each crate owns one
responsibility and exposes a small public surface. Every dependency edge
points down, from the `whirl` binary to the leaf crates. One `whirl` binary
still ships (SPEC section 15).

## 1. Decision

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and
"OPTIONAL" in this document are to be interpreted as described in BCP 14
(RFC 2119, RFC 8174) when, and only when, they appear in all capitals, as
shown here.

### 1.1. Crates

The workspace MUST contain these crates, each under `crates/<name>/`:

- `whirl` — the binary: argument parsing, exit codes (SPEC section 13),
  telemetry, `whirl install`, `whirl doctor`, and `build.rs`.
- `whirl-run` — the runner: flows and steps, timeouts, variables and
  secret masking, the AI cache, and artifacts (SPEC sections 11, 12, 14).
- `whirl-ai` — every language model call: the model catalog and client,
  prompts, AI snapshots, and the `ACT`, `GOAL`, `ai:`, `EXTRACT`, and
  `JUDGE` decisions.
- `whirl-check` — the check engine of ADR `evaluate-checks-in-rust`:
  filters, predicates, reads, and literal validation.
- `whirl-report` — the run report model and the console, JSON, JUnit, and
  HTML renderers (SPEC section 14).
- `whirl-shim` — the browser shim boundary: the client, the wire format,
  launch resolution, and the bundle layout.
- `whirl-lang` — the `.whirl` language: the AST, parser, formatter, lints,
  and command-line option resolver.
- `whirl-types` — the value vocabulary that `whirl-lang` and `whirl-check`
  share: `Value`, `Number`, and the static filter and predicate types.
- `whirl-xpath` — XPath 1.0 through libxml2 (ADR `evaluate-checks-in-rust`
  §1.6).

### 1.2. Dependency direction

The list in §1.1 is the layer order, top down. A crate MUST depend only on
Whirl crates below it. The edges today, where `A -> B` means `A` depends on
`B`; a crate not listed depends on no Whirl crate:

```text
whirl        -> whirl-run, whirl-ai, whirl-check, whirl-report, whirl-shim, whirl-lang
whirl-run    -> whirl-ai, whirl-check, whirl-report, whirl-shim, whirl-lang, whirl-types
whirl-ai     -> whirl-shim, whirl-lang
whirl-check  -> whirl-lang, whirl-types, whirl-xpath
whirl-shim   -> whirl-lang
whirl-lang   -> whirl-types
```

A crate MUST NOT depend on `whirl-run` or `whirl`. `whirl-lang` MUST NOT
depend on `whirl-check`, and it MUST NOT depend on native code, so the
language builds without libxml2.

### 1.3. Shared vocabulary

The Rust style guide warns against broad shared-types crates. `whirl-types`
exists for one reason: `whirl-lang` and `whirl-check` name the same values
and static types, and `whirl-lang` cannot depend on `whirl-check` (§1.2).
`whirl-types` MUST hold only that shared vocabulary, as data and `Display`
implementations. It MUST NOT evaluate a filter or a predicate, and it MUST
NOT depend on another Whirl crate. A type that only one crate uses MUST
stay in that crate.

### 1.4. Literal validation

The parser in `whirl-lang` keeps each literal filter argument and its span in
the AST and does not validate it. `whirl-check` MUST validate literal regex,
JSONPath, XPath, date format, and charset arguments after parsing, with the
same code that builds filters at run time, and report an invalid literal as
SPEC sections 3.1 and 16 require. Every command that checks or runs a file
MUST call this validation after `parse_file`.

### 1.5. Boundaries

`whirl-shim` MUST own the whole protocol of
`docs/engineering/shim-protocol.md`, including the conversion from AST nodes
to wire JSON, so one crate defines what crosses the process boundary.

`whirl-ai` MUST be the only crate that depends on `lithos-llm`.

The `whirl` binary MUST own every acceptance test, because each one starts
`whirl` as a process (ADR `cli-acceptance-tests` §1.1). A unit test lives in
the crate that owns the code it tests.
