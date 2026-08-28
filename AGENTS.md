# Whirl Repository Instructions

## Document authority

- [SPEC.md](SPEC.md) defines Whirl's product behavior and file format.
- Accepted ADRs in [docs/engineering/decisions](docs/engineering/decisions)
  define architecture and repository policy.
- The Rust and TypeScript style guides define implementation defaults for
  their languages.
- The SPEC and accepted ADRs override the style guides.
- A draft or retired ADR is not authoritative.

Do not resolve a conflict between authoritative documents silently. Report the
conflict and ask the user which document to change.

## Required reading

Before architecture, planning, implementation, refactoring, or code review:

1. Read [SPEC.md](SPEC.md) completely.
2. Read every ADR with `status: accepted` in
   [docs/engineering/decisions](docs/engineering/decisions).
3. Read the relevant language guide entry point and every page that it routes
   for the task.

Read these decisions for their specific tasks:

- Before creating or changing an ADR, read
  [ADR drafting](docs/engineering/decisions/adr-drafting.md).
- Before creating or changing developer, build, or verification tasks, read
  [repository-owned tasks](docs/engineering/decisions/repository-owned-tasks.md).
- Before creating or changing acceptance tests, read
  [CLI acceptance tests](docs/engineering/decisions/cli-acceptance-tests.md).

## Rust

Before changing Rust code, configuration, project structure, or tests, read the
[Rust Style Guide entry point](../../brynary/rust-style-guide/SKILL.md)
completely. Follow its routing instructions and read each page required for
the task.

If the local guide is unavailable, use the
[Rust Style Guide on GitHub](https://github.com/brynary/rust-style-guide/blob/main/SKILL.md).

## TypeScript

Before changing TypeScript code, configuration, project structure, or tests,
read the
[TypeScript Style Guide entry point](../../brynary/typescript-style-guide/SKILL.md)
completely. Follow its routing instructions and read each page required for
the task.

If the local guide is unavailable, use the
[TypeScript Style Guide on GitHub](https://github.com/brynary/typescript-style-guide/blob/main/SKILL.md).

The browser shim runs on Whirl's pinned private Node runtime, but Bun is the
repository's package manager and script runner for TypeScript (see
[Bun package management](docs/engineering/decisions/bun-package-management.md)).
The TypeScript guide's Bun runtime, API, and test-runner rules do not apply
to shim code; its package-management rules do.
