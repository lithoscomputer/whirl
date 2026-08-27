---
status: accepted
---

# Test Whirl through its CLI

Whirl acceptance tests invoke the CLI because it is Whirl's only public
interface. Each test uses the simplest form that proves the required behavior.

## 1. Decision

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and
"OPTIONAL" in this document are to be interpreted as described in BCP 14
(RFC 2119, RFC 8174) when, and only when, they appear in all capitals, as
shown here.

### 1.1. CLI tests

An acceptance test MUST start `whirl` as a process. It MUST NOT call an
internal Rust API or the browser shim directly.

A test that needs only stdout, stderr, and an exit status MUST use a `trycmd`
case. A test MUST use a Rust integration test when it needs files, signals,
several process steps, `whirl install`, or a real browser.

### 1.2. Browser tests

A browser acceptance test MUST run a `.whirl` file against a local test site
in a real browser. It MUST check the CLI result and any required artifacts.

### 1.3. Internal tests

Unit tests SHOULD cover parsing, formatting, and lint rules. Tests SHOULD cover
messages between Rust and the browser shim. These internal tests MUST NOT
replace CLI acceptance tests.
