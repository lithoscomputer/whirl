import assert from "node:assert/strict";
import { test } from "node:test";
import { decodeSnapshotComparison } from "./params.js";
import { ShimError } from "./protocol.js";

test("snapshot comparison retains units and explicit zero", () => {
	for (const maxDiff of [
		{ type: "pixels", value: 0 },
		{ type: "pixels", value: Number.MAX_SAFE_INTEGER },
		{ type: "percent", value: 0.125 },
		{ type: "percent", value: 100 },
	]) {
		const params = { pixelThreshold: 0, maxDiff };
		assert.deepEqual(decodeSnapshotComparison(params), params);
	}
});

test("snapshot comparison rejects malformed protocol values before capture", () => {
	const valid = { pixelThreshold: 0.2, maxDiff: { type: "pixels", value: 0 } };
	for (const pixelThreshold of [-1, 1.01, Number.NaN, Infinity, "0.2", null]) {
		assert.throws(
			() => decodeSnapshotComparison({ ...valid, pixelThreshold }),
			ShimError,
		);
	}
	for (const maxDiff of [
		{ type: "pixels", value: -1 },
		{ type: "pixels", value: 1.5 },
		{ type: "pixels", value: Number.MAX_SAFE_INTEGER + 1 },
		{ type: "percent", value: 100.01 },
		{ type: "percent", value: -0.1 },
		{ type: "percent", value: Infinity },
		{ type: "ratio", value: 0.1 },
		{ type: "pixels", value: "10" },
		null,
	]) {
		assert.throws(
			() => decodeSnapshotComparison({ ...valid, maxDiff }),
			ShimError,
		);
	}
});
