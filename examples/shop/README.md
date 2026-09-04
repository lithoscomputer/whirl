# Runnable sample shop

This local app demonstrates login, validation, setup reuse, and visual snapshots.
It needs Python 3 and the repository toolchain. It uses demo credentials and has
no database or payment integration. Each flow has independent browser state.

From the repository root:

```sh
mise run setup
mise run example:test
```

The task builds Whirl, starts the app on an available loopback port, waits for
`/health`, and runs the flows with `--base`, `--trace`, JSON, and JUnit reports.
It stops the app even when a flow fails. Reports, traces, screenshots, and the
server log are in `whirl-artifacts/example/`.

To use the app by hand:

```sh
mise run example:serve
```

Open <http://127.0.0.1:4173>. Sign in with `demo@example.com` and `demo-password`.
Submit the empty checkout form, then enter an address and submit again.

## What the flows prove

- `flows/login.whirl` signs in and captures the account email. Whirl runs it
  once, then shares its saved cookie and capture with both dependent flows.
- `flows/checkout.whirl` checks the account, requires an address, and confirms
  the corrected order. It reads `{{setup.email}}` from the login flow.
- `flows/preview.whirl` compares a widget preview against a committed baseline.
  The preview uses integer CSS rectangles and no fonts or animation. The macOS
  and Linux Chromium baselines intentionally show the same geometry.

`setup` transfers saved browser storage and captures. It does not transfer the
live page or session storage. These flows write no shared server records, so
running them in parallel is safe.

## Try a failure

Change `role:button "Place order"` in the checkout flow to
`role:button "Buy now"`, then run `mise run example:test`. The failure log shows
which locator Whirl awaited and prints a `whirl show-trace` command. Restore
`Place order` to repair the test.

Change the widget color in `preview.html` to produce a visual failure. Inspect
the actual and diff images. Restore the color, or deliberately update the
baseline with `mise run example:update-snapshots`. Review the PNG diff before
committing. Update each supported platform's baseline when rendering changes.
CI compares committed baselines; it never updates them.

## CI recipe

The repository's [CI workflow](../../.github/workflows/ci.yml) is the working
recipe. It pins the toolchain, installs Chromium and its Linux system
libraries, runs `mise run check`, then runs `mise run example:test`. The last
step uploads `whirl-artifacts/example/` even after a test failure.

The same task works for a downstream app after changing its startup command,
health endpoint, and flow directory in `scripts/test-example.py`. Keep the
readiness check and process cleanup. Use separate test accounts or records if
parallel flows write server data. Use environment variables for real secrets;
the demo password in this example has no access outside the local sample app.
