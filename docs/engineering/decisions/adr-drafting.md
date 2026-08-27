---
status: accepted
---

# Write each ADR as a short RFC

Each Architecture Decision Record (ADR) is a short RFC that follows the rules
in this document.

## 1. Decision

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and
"OPTIONAL" in this document are to be interpreted as described in BCP 14
(RFC 2119, RFC 8174) when, and only when, they appear in all capitals, as
shown here.

### 1.1. Structure

An ADR is a document at `docs/engineering/decisions/<slug>.md`. Its `status`
frontmatter records the ADR status: `draft`, `accepted`, or `retired`.

An ADR MUST contain a Title and a Decision section, and MUST open with a
one-paragraph summary directly after the Title. An ADR MAY contain a Context
or a Consequences section. The author MUST omit these sections unless they add
clear value beyond the Decision and its references.

An ADR MUST number its sections. A document MUST reference a requirement by
its ADR and section number instead of restating it.

### 1.2. Language

An ADR MUST state requirements with the BCP 14 key words and MUST include the
BCP 14 boilerplate above.

An ADR SHOULD follow the principles of ASD-STE100 Simplified Technical
English:

- Use plain language.
- Use short, direct sentences in active voice.
- State one main idea or requirement per sentence.
- Use one term for each concept.
- Preserve established technical terms, code identifiers, commands,
  quotations, and literal output.

Clear, natural communication takes priority over strict ASD-STE100 compliance.

### 1.3. Concision

An ADR SHOULD NOT exceed two pages. A shorter ADR is welcome.

The author MUST make a dedicated deletion pass after drafting. An ADR MUST NOT
restate content the reader can derive from a referenced document.

An ADR SHOULD use tables and lists only for short, enumerable facts. An ADR
SHOULD keep reasoning in prose.
