import assert from "node:assert/strict";
import { test } from "node:test";
import type { Page } from "@playwright/test";
import { buildLocator, describeLocator } from "./locators.js";
import type { LocatorSegment } from "./protocol.js";
import { ShimError } from "./protocol.js";

interface RecordedCall {
	readonly method: string;
	readonly args: readonly unknown[];
}

/** Records the Playwright calls a segment chain produces. */
function recordingStub(): { page: Page; calls: RecordedCall[] } {
	const calls: RecordedCall[] = [];
	const methods = [
		"getByRole",
		"getByLabel",
		"getByPlaceholder",
		"getByText",
		"getByAltText",
		"getByTitle",
		"getByTestId",
		"locator",
		"nth",
	];
	const stub: Record<string, unknown> = {};
	for (const method of methods) {
		stub[method] = (...args: unknown[]) => {
			calls.push({ method, args });
			return stub;
		};
	}
	return { page: stub as unknown as Page, calls };
}

test("role with a name passes name and exact", () => {
	const { page, calls } = recordingStub();
	buildLocator(page, [
		{ type: "role", role: "button", name: "Sign in", exact: true },
	]);
	assert.deepEqual(calls, [
		{ method: "getByRole", args: ["button", { name: "Sign in", exact: true }] },
	]);
});

test("role without a name passes no options", () => {
	const { page, calls } = recordingStub();
	buildLocator(page, [
		{ type: "role", role: "heading", name: null, exact: true },
	]);
	assert.deepEqual(calls, [{ method: "getByRole", args: ["heading"] }]);
});

test("the ~ variants pass exact false", () => {
	const { page, calls } = recordingStub();
	buildLocator(page, [
		{ type: "role", role: "button", name: "Add", exact: false },
		{ type: "text", text: "Added", exact: false },
	]);
	assert.deepEqual(calls, [
		{ method: "getByRole", args: ["button", { name: "Add", exact: false }] },
		{ method: "getByText", args: ["Added", { exact: false }] },
	]);
});

test("every text engine maps to its getBy call", () => {
	const { page, calls } = recordingStub();
	const segments: LocatorSegment[] = [
		{ type: "label", text: "Email", exact: true },
		{ type: "placeholder", text: "Search", exact: true },
		{ type: "text", text: "Add to cart", exact: true },
		{ type: "alt", text: "Logo", exact: true },
		{ type: "title", text: "Info", exact: true },
	];
	buildLocator(page, segments);
	assert.deepEqual(
		calls.map((call) => call.method),
		[
			"getByLabel",
			"getByPlaceholder",
			"getByText",
			"getByAltText",
			"getByTitle",
		],
	);
	for (const call of calls) {
		assert.deepEqual(call.args[1], { exact: true });
	}
});

test("testid and css map to getByTestId and locator", () => {
	const { page, calls } = recordingStub();
	buildLocator(page, [
		{ type: "testid", id: "cart-badge" },
		{ type: "css", selector: ".foo > .bar" },
	]);
	assert.deepEqual(calls, [
		{ method: "getByTestId", args: ["cart-badge"] },
		{ method: "locator", args: [".foo > .bar"] },
	]);
});

test("nth is 1-based and subtracts 1", () => {
	const { page, calls } = recordingStub();
	buildLocator(page, [
		{ type: "testid", id: "result-card" },
		{ type: "nth", index: 1 },
		{ type: "nth", index: 3 },
	]);
	assert.deepEqual(calls, [
		{ method: "getByTestId", args: ["result-card"] },
		{ method: "nth", args: [0] },
		{ method: "nth", args: [2] },
	]);
});

test("nth as the first segment is an internal error", () => {
	const { page } = recordingStub();
	assert.throws(
		() => buildLocator(page, [{ type: "nth", index: 1 }]),
		(error: unknown) => error instanceof ShimError && error.kind === "internal",
	);
});

test("an empty locator is an internal error", () => {
	const { page } = recordingStub();
	assert.throws(
		() => buildLocator(page, []),
		(error: unknown) => error instanceof ShimError && error.kind === "internal",
	);
});

test("describeLocator renders the chain", () => {
	const description = describeLocator([
		{ type: "role", role: "button", name: "Sign in", exact: true },
		{ type: "nth", index: 2 },
		{ type: "css", selector: "a[href]" },
	]);
	assert.equal(
		description,
		'getByRole("button", { name: "Sign in", exact: true }) >> nth(1) >> locator("a[href]")',
	);
});

test("describeLocator covers every segment type", () => {
	const description = describeLocator([
		{ type: "role", role: "heading", name: null, exact: true },
		{ type: "label", text: "Email", exact: true },
		{ type: "placeholder", text: "Search", exact: false },
		{ type: "text", text: "Add", exact: true },
		{ type: "alt", text: "Logo", exact: true },
		{ type: "title", text: "Info", exact: true },
		{ type: "testid", id: "x" },
	]);
	assert.equal(
		description,
		'getByRole("heading") >> getByLabel("Email", { exact: true }) >> ' +
			'getByPlaceholder("Search", { exact: false }) >> ' +
			'getByText("Add", { exact: true }) >> getByAltText("Logo", { exact: true }) >> ' +
			'getByTitle("Info", { exact: true }) >> getByTestId("x")',
	);
});
