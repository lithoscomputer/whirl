# Machine-readable output

`whirl check --json flows/` writes diagnostics to stdout. `whirl --report-json
report.json flows/` writes the run report. Both use `version: 1`; consumers must
ignore unknown fields. Breaking shape changes require a new version.

Schemas: [check](check.schema.json), [run report](report.schema.json).

## Diagnostic codes

| Code | Meaning |
| --- | --- |
| `parse-error` | Invalid flow syntax; `expected` lists alternatives when available |
| `input-selection` | Missing input path or no selected flow files |
| `input-io` | An input could not be read |
| `setup-io` | A setup flow could not be found or read |
| `nested-setup` | A setup flow names another setup |
| `unknown-setup-capture` | Referenced capture is not defined by the setup |
| `missing-setup` | A setup capture reference has no setup option |
| `interpolated-setup` | A setup path contains a variable |
| `conflicting-storage` | Both setup and storage are specified |
| `duplicate-artifact` | An artifact name is repeated |
| `unused-capture` | A capture is never read; warning |
| `redundant-presence` | The following assertion requires presence; warning |

Locations use 1-based Unicode character positions, not bytes or UTF-16 units.
`length` is the source span length on that line. Input and I/O diagnostics may
have null locations. `expected` is an array, empty when there are no alternatives.
Warnings do not change exit status. Successful checks emit an empty diagnostics
array. Argument syntax errors, such as an unknown flag, still use CLI usage text.

## Run errors and runtime metadata

Step `error.code` uses the [shim error kinds](shim-protocol.md#7-error-kinds), plus
`entry-timeout`, `variable-resolution`, `shim-crash`, and `setup-failed`.
`internal` covers an invalid shim result. Codes are stable; message text may
change and must not be parsed. Secret masking covers diagnostic action logs in
run reports. `check` reports source text without resolving environment variables.

The report records the invocation's absolute `workingDirectory`, Whirl version,
platform, and architecture. Each file with a started browser context has
`runtime` containing `browser`, `viewport`, `browserVersion`, `nodeVersion`, and
`playwrightVersion`. Versions come from the active shim and browser. Version
fields can be null when an older shim omits them. A failure before context startup
has no runtime object.

## Rerunning failures

```sh
whirl --report-json report.json flows/
whirl --rerun-failed report.json --trace
```

Reruns select failed and errored files, resolve relative paths using the report's
working directory, and execute each whole file plus its setup. Reports do not
store executable configuration or secret values. Supply the original `--base`,
`--browser`, variable flags, and environment again when needed. Move a report
freely, but update file paths if the project itself has moved.

A report with no failures exits successfully without opening a browser. Reports
without `workingDirectory`, invalid statuses, and unknown versions are rejected
as usage errors. This intentionally excludes reports from older Whirl releases
that did not record the working directory.
