---
status: accepted
---

# Evaluate checks in Rust

Rust evaluates every filter and predicate in Whirl's `ASSERT` and
`CAPTURE` lines (SPEC sections 9 and 10). The browser shim only reads
raw values from the page and from responses. This gives exact JSON numbers,
Hurl's behavior through the same Rust libraries that Hurl uses, and one
definition of each check that `whirl check` can also type-check.

## 1. Decision

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and
"OPTIONAL" in this document are to be interpreted as described in BCP 14
(RFC 2119, RFC 8174) when, and only when, they appear in all capitals, as
shown here.

### 1.1. Evaluation

Rust MUST evaluate every filter and every predicate. The shim MUST NOT
evaluate a predicate for a check with a subject, and it MUST NOT parse
response JSON. State checks, `tab:NAME closed`, `PAGE`, and `SNAPSHOT` MAY
keep Playwright's assertions.

### 1.2. Shim reads

The shim MUST return raw values. For a page value, it returns a string that
uses Whirl's text normalization. For an `eval` result, it returns the JSON
value under SPEC section 10. For a response, it returns the status, the
headers, and the decoded body bytes. A read MUST apply the SPEC's locator
strictness rules. A read MUST NOT wait for its value to appear.

### 1.3. Retries

Rust MUST own the retry loop for page checks and page captures. The loop
MUST use Playwright's poll schedule. It MUST keep the step and entry timeouts
of SPEC section 12. The reads of one check MUST appear under one trace group.

### 1.4. Libraries

Rust MUST keep the exact text of every JSON number that a check reads,
compares, captures, or reports. The build MUST NOT turn on a dependency
feature that changes JSON number handling for other crates, such as
serde_json's `arbitrary_precision`. Rust MUST use an RFC 9535 JSONPath
implementation, chrono for dates, the WHATWG Encoding Standard for charsets,
an ECMAScript regex engine in Unicode mode, and libxml2 for XPath.

### 1.5. libxml2

Release builds MUST link a pinned libxml2 statically. A Mise task MUST build
it from source (ADR `repository-owned-tasks` §1.2). A release binary MUST NOT
need libxml2 on the user's system.

### 1.6. Unsafe code

The `libxml` crate's safe API cannot read the type of an XPath result or stop
libxml2 from writing errors to stderr. So Whirl calls libxml2 directly, as
Hurl does, from one small crate.

Only the `whirl-xpath` crate MAY contain `unsafe` code. It MUST call only
libxml2, through the `libxml` crate's bindings, and it MUST give the rest of
Whirl a safe API. It MUST keep the workspace lints and allow `unsafe_code` at
crate scope. Each `unsafe` block MUST have a `SAFETY:` comment. All other
project code MUST NOT add `unsafe`, under the workspace
`unsafe_code = "deny"` policy.
