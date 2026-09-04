// Opt-in real-browser smoke test. Skipped unless WHIRL_SHIM_SMOKE=1 so the
// default `bun run test` run stays browser-free.

import assert from "node:assert/strict";
import { test } from "node:test";

const smokeEnabled = process.env["WHIRL_SHIM_SMOKE"] === "1";

test("the driver runs a flow against a data: URL in chromium", {
	skip: !smokeEnabled,
}, async () => {
	const { PlaywrightDriver } = await import("./playwright-driver.js");
	const driver = new PlaywrightDriver();
	try {
		await driver.startFlow({
			browser: "chromium",
			headed: false,
			viewport: { width: 800, height: 600 },
			storageStatePath: null,
			dialogs: "dismiss",
			allowHosts: null,
			navTimeoutMs: 30000,
			userAgent: null,
			reducedMotion: null,
			video: null,
			harPath: null,
			trace: false,
		});
		await driver.runStep("visit", {
			timeoutMs: 10000,
			title: "VISIT data:",
			url: "data:text/html,<h1>Hello</h1><button>Go</button>",
		});
		await driver.runStep("assert", {
			timeoutMs: 5000,
			title: 'role:heading "Hello" visible',
			spec: {
				subject: {
					type: "locator",
					locator: [
						{ type: "role", role: "heading", name: "Hello", exact: true },
					],
				},
				check: { type: "state", state: "visible" },
			},
		});
		const captured = await driver.runStep("capture", {
			timeoutMs: 5000,
			title: "capture eval",
			source: { type: "eval", script: "1 + 2" },
			filter: null,
		});
		assert.deepEqual(captured, { value: "3" });
		const result = await driver.endFlow({
			saveStoragePath: null,
			tracePath: null,
		});
		assert.deepEqual(result, { blockedHosts: [], videoPath: null });
	} finally {
		await driver.dispose();
	}
});
