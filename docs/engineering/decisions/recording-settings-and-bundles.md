---
status: accepted
---

# Store effective settings in recordings and write one bundle per flow

BrowserSim recording will build on the settings rules of `whirl run`. This
ADR fixes the shape of the recording command, what a recording stores, how
replay uses it, and where recordings go, before the recording commands
exist.

## 1. Decision

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and
"OPTIONAL" in this document are to be interpreted as described in BCP 14
(RFC 2119, RFC 8174) when, and only when, they appear in all capitals, as
shown here.

### 1.1. Commands

`whirl record` MUST be the only command that creates a recording. Other
commands MUST NOT take a `--record` flag.

`--engine browsersim` MUST select the execution engine. `--browser` MUST
continue to select the browser.

Recording settings MUST come from `[Options]` and `-O` only. Whirl MUST NOT
add `--privacy`, `--matching`, `--flow`, `--documents`, or `--keep-origin`.

Whirl MUST NOT expose a command or flag before it works.

### 1.2. Stored settings

Recording creation MUST resolve settings as `whirl run` does (SPEC section
13): the built-in defaults, then the flow's `[Options]`, then `-O`. It MUST
use the shared resolver, `lang::cli_options`.

A `*.sim.json` recording MUST store every effective execution setting,
including defaults and command-line overrides. It MUST NOT store output or
report destinations, such as `--out` or `--report-json`.

Replay MUST use the stored settings. It MUST NOT read the source flow again,
resolve environment variables again, or substitute the current defaults.

In v1, replay, recording-based comparison, and repair MUST reject `-O` and
every flag that overrides a stored setting, as usage errors.

### 1.3. Bundles

A recording of a written flow MUST be a bundle directory named after the
flow's file stem, directly under the output directory. Whirl MUST NOT mirror
source directories. So `checkout.whirl` and `flows/search.whirl` give sibling
bundles:

```text
whirl-artifacts/
  checkout/
    checkout.sim.json
    files/
    screenshots/
    result.json
  search/
    search.sim.json
    files/
    screenshots/
    result.json
```

Before capture starts, Whirl MUST reject distinct input flows with the same
stem and name their paths. It MUST NOT add suffixes, hashes, or directories
to avoid the collision.

Interactive recording from a URL MUST use the output directory itself as its
bundle, with `recording.sim.json` inside it.

An output path MUST NOT resolve to an input file or to a path inside an input
directory. Whirl MUST compare canonical paths, so a symlink alias of an input
counts as the input, and it MUST check before it writes.

This layout applies to recordings only. `whirl run` keeps the per-flow
layout of SPEC section 14.

### 1.4. Open decisions

Two decisions MUST be made before `whirl record` writes bundles: the names
and list or mapping syntax of the privacy and matching options, and what
happens when a bundle already exists.
