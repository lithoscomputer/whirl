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
| `unasserted-http-status` | An independent HTTP entry has no status assertion; warning |

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
`runtime` containing `browser`, `viewport`, `userAgent`, `browserVersion`,
`nodeVersion`, and `playwrightVersion`. `userAgent` is the actual string the
browser reports, after alias resolution, with secret values masked. Versions
come from the active shim and browser. User agent and version fields can be null
when an older shim omits them. With `--video`, `runtime` also has `videoFps`,
the recording's frames per second (60 or the `--video-fps` value on Chromium,
25 elsewhere); it is absent without a recording and in older reports. A failure
before context startup has no runtime object.

## Saved reports and run records

```sh
whirl report report.json --html evidence.html
whirl report report.json --html evidence.html --metadata context.json
```

The saved command reads version 1 reports and available media without execution. It retains the original producer context and results. Successful HTML generation exits 0 even if the saved tests failed. Metadata keys match recorded flow paths exactly and do not require source files. `--working-directory DIR` changes the base for relative artifact paths; absolute paths stay absolute. Keep distinct artifact directories for runs whose evidence you need to retain.

New reports add the following optional fields to version 1:

| Location | Field | Meaning |
| --- | --- | --- |
| Run and file | `startedAt`, `finishedAt` | UTC RFC 3339 boundaries; file boundaries cover a scheduled attempt, including pre-browser failure |
| File | `sourceSha256` | Lowercase SHA-256 of the exact parsed flow bytes, before interpolation |
| File | `roles.requested` | Selected by input paths or rerun selection |
| File | `roles.setup` | Used as another selected flow's setup |
| Run | `videoRequested` | Whether the run requested browser recordings |

Both roles can be true; that flow runs once. `[setup]` is an entry name for a pre-entry failure, not a file role. Fail-fast files that were never scheduled are absent. Hashes cover flow files only. They do not cover fixtures, variables, artifacts, or application code. Durations still use the monotonic clock.

Compare parsed timestamps to choose the latest result. File modification times do not identify when a run happened. Older reports lack these fields; consumers must handle missing information without inventing an execution time. See [SPEC 14.2 and 14.3](../../SPEC.md#142-html-from-saved-results).

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
