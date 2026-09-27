import assert from "node:assert/strict";
import { test } from "node:test";
import type { Page } from "@playwright/test";
import {
	iframeName,
	iframeRefs,
	nameIframes,
	withIframeNames,
} from "./iframe-names.js";
import { Deadline } from "./step-util.js";

interface IframeAttributes {
	readonly label: string | null;
	readonly title: string | null;
}

/**
 * A page whose `aria-ref=` locators find the iframes in `frames`, by ref.
 * A read of a ref in `failing` throws, as a closed page does. It records
 * each selector it resolves.
 */
function fakePage(
	frames: ReadonlyMap<string, IframeAttributes>,
	failing: ReadonlySet<string> = new Set(),
): { page: Page; selectors: string[] } {
	const selectors: string[] = [];
	const stub = {
		locator(selector: string) {
			selectors.push(selector);
			const ref = selector.replace("aria-ref=", "");
			const frame = frames.get(ref);
			return {
				count: async () => (frame === undefined ? 0 : 1),
				evaluate: async () => {
					if (failing.has(ref) || frame === undefined) {
						throw new Error("Target page, context or browser has been closed");
					}
					return frame;
				},
			};
		},
	};
	return { page: stub as unknown as Page, selectors };
}

const twoFrames =
	"- iframe [ref=e4]:\n  - paragraph [ref=f1e2]: Resolved\n- iframe [ref=e5]";

const statusPage = [
	"- generic [active] [ref=e1]:",
	'  - heading "Status page" [level=1] [ref=e2]',
	"  - iframe [ref=e4]:",
	"    - generic [ref=f1e1]:",
	"      - paragraph [ref=f1e2]: Resolved",
	"      - iframe [ref=f1e3]:",
	'        - button "Refresh" [ref=f2e2]',
	"  - iframe [active] [ref=e5]",
	"  - text: iframe [ref=e9]",
].join("\n");

test("iframe refs come in document order, framed refs included", () => {
	assert.deepEqual(iframeRefs(statusPage), ["e4", "f1e3", "e5"]);
});

test("a named iframe shows its name after its role", () => {
	const named = withIframeNames(
		statusPage,
		new Map([["e4", "Incident history"]]),
	);
	assert.equal(named.split("\n")[2], '  - iframe "Incident history" [ref=e4]:');
});

test("a nested iframe is named at its own depth", () => {
	const named = withIframeNames(
		statusPage,
		new Map([["f1e3", "Uptime chart"]]),
	);
	assert.equal(
		named.split("\n")[5],
		'      - iframe "Uptime chart" [ref=f1e3]:',
	);
});

test("the name comes before the active mark, as Playwright orders them", () => {
	const named = withIframeNames(statusPage, new Map([["e5", "Live chat"]]));
	assert.equal(
		named.split("\n")[7],
		'  - iframe "Live chat" [active] [ref=e5]',
	);
});

test("a name is quoted with JSON escapes", () => {
	const named = withIframeNames(
		"- iframe [ref=e4]",
		new Map([["e4", 'Say "hi" \\ bye']]),
	);
	assert.equal(named, '- iframe "Say \\"hi\\" \\\\ bye" [ref=e4]');
});

test("a name that YAML would misread does not wrap the line in quotes", () => {
	const named = withIframeNames(
		"- iframe [ref=e4]",
		new Map([["e4", "Status: live #1"]]),
	);
	assert.equal(named, '- iframe "Status: live #1" [ref=e4]');
});

test("iframes without a name and other lines stay as they are", () => {
	assert.equal(withIframeNames(statusPage, new Map()), statusPage);
});

test("aria-label names an iframe before its title", () => {
	assert.equal(iframeName("Live chat", "Chat widget"), "Live chat");
});

test("an empty aria-label leaves the title as the name", () => {
	assert.equal(iframeName("  ", "Chat widget"), "Chat widget");
});

test("a name has its whitespace collapsed", () => {
	assert.equal(iframeName(null, "  Incident\n  history "), "Incident history");
});

test("an iframe without a label or title has no name", () => {
	assert.equal(iframeName(null, null), undefined);
});

test("a name longer than Playwright shows is no name", () => {
	assert.equal(iframeName(null, "x".repeat(901)), undefined);
});

test("the page names each iframe it still has", async () => {
	const { page } = fakePage(
		new Map([
			["e4", { label: null, title: "Incident history" }],
			["e5", { label: "Live chat", title: null }],
		]),
	);
	assert.equal(
		await nameIframes(page, twoFrames, new Deadline(1000)),
		'- iframe "Incident history" [ref=e4]:\n  - paragraph [ref=f1e2]: Resolved\n- iframe "Live chat" [ref=e5]',
	);
});

test("an iframe the page replaced keeps its line", async () => {
	const { page } = fakePage(
		new Map([["e4", { label: null, title: "Incident history" }]]),
	);
	assert.equal(
		await nameIframes(page, twoFrames, new Deadline(1000)),
		'- iframe "Incident history" [ref=e4]:\n  - paragraph [ref=f1e2]: Resolved\n- iframe [ref=e5]',
	);
});

test("a failed read keeps the line and does not fail", async () => {
	const { page } = fakePage(
		new Map([
			["e4", { label: null, title: "Incident history" }],
			["e5", { label: null, title: "Advertisement" }],
		]),
		new Set(["e4"]),
	);
	assert.equal(
		await nameIframes(page, twoFrames, new Deadline(1000)),
		'- iframe [ref=e4]:\n  - paragraph [ref=f1e2]: Resolved\n- iframe "Advertisement" [ref=e5]',
	);
});

test("a spent budget reads nothing and keeps the snapshot", async () => {
	const { page, selectors } = fakePage(
		new Map([["e4", { label: null, title: "Incident history" }]]),
	);
	assert.equal(await nameIframes(page, twoFrames, new Deadline(0)), twoFrames);
	assert.deepEqual(selectors, []);
});
