---
status: accepted
---

# Use Bun for TypeScript package management and scripts

Bun is the only package manager and script runner for TypeScript in this
repository. The browser shim still runs on Whirl's pinned private Node
runtime (SPEC section 15), so Bun replaces npm, not Node.

## 1. Decision

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and
"OPTIONAL" in this document are to be interpreted as described in BCP 14
(RFC 2119, RFC 8174) when, and only when, they appear in all capitals, as
shown here.

### 1.1. Package management and scripts

The repository MUST manage TypeScript dependencies with Bun: a committed
`bun.lock`, exact version pins, and `bun install --frozen-lockfile` in
tasks. Tasks and scripts MUST invoke package.json scripts and package
binaries through Bun (`bun run`, `bun x`). The repository MUST NOT invoke
npm and MUST NOT commit npm lockfiles.

`whirl install` MUST NOT run npm on the user's machine. It MUST assemble
the shim bundle's dependencies with a pinned Bun binary that it downloads
and verifies.

### 1.2. Runtime

The shim, its tests, and the installed bundle MUST run on the pinned Node
runtime. Bun MUST NOT be the shim's runtime or test runtime.
