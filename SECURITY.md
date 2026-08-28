# Security Policy

Report suspected vulnerabilities privately through
[GitHub security advisories](https://github.com/lithoscomputer/whirl/security/advisories/new).
Please do not open public issues for security reports.

Notes for operators:

- `whirl` runs the scripts your `.whirl` files contain (`EVAL`) and
  navigates wherever they point. Treat flow files like code.
- Browser-recorded artifacts (screenshots, video, HAR, storage state) can
  contain secrets the flow typed or received; the artifacts directory is
  sensitive. Env-sourced values are masked in textual output only.
- `whirl install` downloads the pinned Node runtime and Bun over HTTPS and
  verifies both against their published SHA-256 checksums before use.
