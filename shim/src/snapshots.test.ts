import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { test } from "node:test";
import { compareSnapshot, retryUnableToCapture } from "./snapshots.js";
import { Deadline } from "./step-util.js";

interface Png {
	readonly data: Buffer;
}
interface PngBundle {
	readonly PNG: {
		new (options: { width: number; height: number }): Png;
		readonly sync: { readonly write: (image: Png) => Buffer };
	};
}
// Use the same PNG encoder bundled with the pinned comparator. No browser or
// fonts are involved, so the changed-pixel count is exact.
const require = createRequire(import.meta.url);
const { PNG } = require("playwright-core/lib/utilsBundle") as PngBundle;
function frame(width = 100, height = 100, changed = 0, gray = 0): Buffer {
	const png = new PNG({ width, height });
	png.data.fill(255);
	for (let pixel = 0; pixel < changed; pixel++) {
		for (let color = 0; color < 3; color++) png.data[pixel * 4 + color] = gray;
	}
	return PNG.sync.write(png);
}

test("counts and fractional percentages pass exactly at the difference limit", () => {
	const baseline = frame();
	const actual = frame(100, 100, 100);
	for (const [type, value, passes] of [
		["pixels", 100, true],
		["pixels", 99, false],
		["pixels", 0, false],
		["percent", 1, true],
		["percent", 0.999, false],
		["percent", 100, true],
	] as const) {
		const result = compareSnapshot(actual, baseline, {
			pixelThreshold: 0.2,
			maxDiff: { type, value },
		});
		assert.equal(result === null, passes, `${type} ${String(value)}`);
		if (result !== null) assert.ok(result.diff);
	}
});

test("threshold controls color sensitivity, but never relaxes dimensions", () => {
	const baseline = frame();
	const actual = frame(100, 100, 100, 240);
	const maxDiff = { type: "pixels", value: 0 } as const;
	assert.notEqual(
		compareSnapshot(actual, baseline, { pixelThreshold: 0, maxDiff }),
		null,
	);
	assert.equal(
		compareSnapshot(actual, baseline, { pixelThreshold: 0.2, maxDiff }),
		null,
	);
	assert.notEqual(
		compareSnapshot(frame(100, 101), baseline, {
			pixelThreshold: 1,
			maxDiff: { type: "percent", value: 100 },
		}),
		null,
	);
});

test("a capture that Chromium refuses for a moment runs again until the deadline", async () => {
	let calls = 0;
	const flaky = async (): Promise<string> => {
		calls++;
		if (calls < 3)
			throw new Error(
				"Protocol error (Page.captureScreenshot): Unable to capture screenshot",
			);
		return "png";
	};
	assert.equal(await retryUnableToCapture(flaky, new Deadline(5_000)), "png");
	assert.equal(calls, 3);

	let other = 0;
	await assert.rejects(
		retryUnableToCapture(async () => {
			other++;
			throw new Error("Timeout 10ms exceeded");
		}, new Deadline(5_000)),
		/Timeout 10ms exceeded/,
	);
	assert.equal(other, 1, "any other error fails at once");

	await assert.rejects(
		retryUnableToCapture(async () => {
			throw new Error("Unable to capture screenshot");
		}, new Deadline(0)),
		/Unable to capture screenshot/,
	);
});
