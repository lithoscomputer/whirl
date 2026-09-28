# Machine-readable output

`whirl check --json flows/` writes diagnostics to stdout. `whirl --report-json
report.json flows/` writes the run report. The check document uses `version: 1`,
and the run report uses `version: 2`. Consumers must ignore unknown fields.
Breaking shape changes require a new version.

Schemas: [check](check.schema.json), [run report](report.schema.json).

## Diagnostic codes

| Code | Meaning |
| --- | --- |
| `parse-error` | Invalid flow syntax; `expected` lists alternatives when available |
| `mixed-check-syntax` | A file uses both `[Asserts]` or `[Captures]` sections and `ASSERT` or `CAPTURE` lines |
| `sections-removed` | A file has an `[Asserts]` or `[Captures]` section; `whirl fmt` rewrites it |
| `input-selection` | Missing input path or no selected flow files |
| `input-io` | An input could not be read |
| `setup-io` | A setup flow could not be found or read |
| `nested-setup` | A setup flow names another setup |
| `unknown-setup-capture` | Referenced capture is not defined by the setup |
| `missing-setup` | A setup capture reference has no setup option |
| `interpolated-setup` | A setup path contains a variable |
| `conflicting-storage` | Both setup and storage are specified |
| `duplicate-artifact` | An artifact name is repeated |
| `duplicate-response` | A response name is used twice in one file |
| `unknown-response` | A check reads a response or request name that no earlier line defines |
| `duplicate-tab` | A tab name is used twice in one file |
| `unknown-tab` | A line names a tab that no earlier line opens |
| `unused-capture` | A capture is never read; warning |
| `redundant-presence` | The following assertion requires presence; warning |
| `unasserted-http-status` | An independent HTTP entry has no status assertion; warning |
| `act-without-model` | A file uses `ACT` without a `model` option |
| `unknown-model` | The `model` option names a model the catalog cannot route |
| `filter-type` | A check's subject, filters, predicate, and literal expected value cannot work together, such as `text toHex` or `status == "200"` |

Locations use 1-based Unicode character positions, not bytes or UTF-16 units.
`length` is the source span length on that line. Input and I/O diagnostics may
have null locations. `expected` is an array, empty when there are no alternatives.
Warnings do not change exit status. Successful checks emit an empty diagnostics
array. Argument syntax errors, such as an unknown flag, still use CLI usage text.

## Run errors and runtime metadata

Step `error.code` uses the [shim error kinds](shim-protocol.md#7-error-kinds), plus
`entry-timeout`, `variable-resolution`, `shim-crash`, and `setup-failed`.
Checks and captures add `type-mismatch`, `filter-error`, and `missing-value`,
and report a false predicate as `assert` (SPEC 9.7). An `eval` subject reports
the shim kinds `eval` and `eval-result`.
`ACT` steps add `act-no-match`, `act-invalid-decision`, `act-model`,
`act-fill-mismatch`, and `stale-ref` (SPEC 7.4). An `ACT` step also has an `act` object: `model`;
`planner`, `llm` or with `--jev` `jev`; `actions`, each with a `line` in
Whirl syntax, the `description` of the element, and `plannedBy`, the planner
that chose it; and `usage`, with `modelCalls`, `inputTokens`, `outputTokens`,
`costUsdMicros` when every call was priced, and with `--jev` a `jev` object
of `requests`, `inputTokens`, `outputTokens`, and `costUsdMicros` when every
answered request was priced. The step's `costUsdMicros` is the model's cost
plus Jev's; a step with no model call costs only what Jev did. Arguments keep their `%name%` placeholders.
`internal` covers an invalid shim result.

A step can have `warnings`, each with a stable `code` and a `message`. The
step's status does not change. `unused-mock` marks a `MOCK` line whose mock
served no request in a file that passed (SPEC 7.5). A file that ran `MOCK`
lines has `mocks`: each with its `line`, `method`, resolved `url`, and
`hits`, the number of requests it served. Codes are stable; message text may
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

The saved command reads version 1 and version 2 reports and available media without execution. It retains the original producer context and results. Successful HTML generation exits 0 even if the saved tests failed. Metadata keys match recorded flow paths exactly and do not require source files. `--working-directory DIR` changes the base for relative artifact paths; absolute paths stay absolute. Keep distinct artifact directories for runs whose evidence you need to retain.

Version 1 added the following optional fields, and version 2 keeps them:

| Location | Field | Meaning |
| --- | --- | --- |
| Run and file | `startedAt`, `finishedAt` | UTC RFC 3339 boundaries; file boundaries cover a scheduled attempt, including pre-browser failure |
| File | `sourceSha256` | Lowercase SHA-256 of the exact parsed flow bytes, before interpolation |
| File | `roles.requested` | Selected by input paths or rerun selection |
| File | `roles.setup` | Used as another selected flow's setup |
| Run | `videoRequested` | Whether the run requested browser recordings |

Both roles can be true; that flow runs once. `[setup]` is an entry name for a pre-entry failure, not a file role. Fail-fast files that were never scheduled are absent. Hashes cover flow files only. They do not cover fixtures, variables, artifacts, or application code. Durations still use the monotonic clock.

Compare parsed timestamps to choose the latest result. File modification times do not identify when a run happened. Older reports lack these fields; consumers must handle missing information without inventing an execution time. See [SPEC 14.2 and 14.3](../../SPEC.md#142-html-from-saved-results).

## Report version 2

Version 2 changes only the shape of `captures`. Version 1 wrote each capture
as a string. Version 2 writes its type and its value (SPEC 9.3 and 14):

```json
"captures": {
  "order_id": {"type": "number", "value": 1234567890123456789},
  "tags": {"type": "list", "value": ["a", "b"]},
  "token": {"type": "bytes", "value": "PDw/Pz8+Pg=="},
  "password": {"type": "string", "value": "***"}
}
```

The types are `string`, `number`, `boolean`, `null`, `list`, `object`,
`bytes`, and `date`. Numbers keep their exact JSON text, so a consumer that
needs exact integers must not parse them as floating point. Bytes are Base64
strings, and dates are RFC 3339 strings. A masked capture keeps its type, and
its value is the string `***`.

`whirl report` and `--rerun-failed` read versions 1 and 2. They read a
version 1 capture as a string.

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

`SNAPSHOT` steps include an optional `snapshot` object after settings resolve:
`masks` is the effective list of explicit locator strings, `maxDiff` is the
count or percentage text (such as `20` or `0.1%`), and `pixelThreshold` is
numeric text. An element snapshot adds `target`, the explicit locator text of
the captured element; a full-page snapshot omits it. These strings follow
normal secret masking. The object records file defaults and local overrides,
including on comparison failures. The step text includes its local option
lines. Older reports without this object, or without `target`, remain
readable. HTML renders the capture and settings for both live and saved
reports.
