import assert from "node:assert/strict";
import { test } from "node:test";
import type { Page } from "@playwright/test";
import type { ReadSubject } from "./protocol.js";
import { runRead } from "./reads.js";

interface ElementStub {
	readonly count: number;
	readonly text?: string | null;
	readonly value?: string;
	readonly attributes?: Readonly<Record<string, string>>;
}

/** A page whose every locator resolves to the same stubbed element set. */
function stubPage(element: ElementStub, url = "", title = ""): Page {
	const locator: Record<string, unknown> = {
		count: async () => element.count,
		evaluate: async () => element.text ?? "",
		inputValue: async () => {
			if (element.value === undefined) {
				throw new Error("Not an input element");
			}
			return element.value;
		},
		getAttribute: async (name: string) => element.attributes?.[name] ?? null,
	};
	locator["getByTestId"] = () => locator;
	return {
		getByTestId: () => locator,
		url: () => url,
		title: async () => title,
	} as unknown as Page;
}

const testid = [{ type: "testid", id: "badge" }] as const;

function element(extract: "text" | "value"): ReadSubject {
	return { type: "element", locator: testid, extract: { type: extract } };
}

test("a locator with no match reads as missing", async () => {
	const result = await runRead(stubPage({ count: 0 }), element("text"), 1000);
	assert.deepEqual(result, { type: "missing", reason: "no-element" });
});

test("text reads are whitespace-normalized", async () => {
	const page = stubPage({ count: 1, text: "  Order\n  42​ " });
	const result = await runRead(page, element("text"), 1000);
	assert.deepEqual(result, { type: "value", value: "Order 42" });
});

test("an absent attribute reads as missing", async () => {
	const page = stubPage({ count: 1, attributes: { href: "/x" } });
	const subject: ReadSubject = {
		type: "element",
		locator: testid,
		extract: { type: "attr", name: "aria-current" },
	};
	assert.deepEqual(await runRead(page, subject, 1000), {
		type: "missing",
		reason: "absent-attribute",
	});
});

test("value on a non-input element is a read error", async () => {
	const page = stubPage({ count: 1 });
	await assert.rejects(runRead(page, element("value"), 1000), {
		name: "ShimError",
		kind: "read",
	});
});

test("count returns a number without waiting", async () => {
	const page = stubPage({ count: 3 });
	const subject: ReadSubject = { type: "count", locator: testid };
	assert.deepEqual(await runRead(page, subject, 1000), {
		type: "value",
		value: 3,
	});
});

test("url and title read the page", async () => {
	const page = stubPage({ count: 0 }, "https://example.com/a", " Shop  home ");
	assert.deepEqual(await runRead(page, { type: "url" }, 1000), {
		type: "value",
		value: "https://example.com/a",
	});
	assert.deepEqual(await runRead(page, { type: "title" }, 1000), {
		type: "value",
		value: "Shop home",
	});
});
