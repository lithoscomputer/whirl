import assert from "node:assert/strict";
import { test } from "node:test";
import { FlowMocks, mockUrl } from "./flow-mocks.js";
import type { MockParams } from "./protocol.js";

function mock(id: number, method: string, pattern: string): MockParams {
	return {
		id,
		method,
		pattern,
		response: { type: "fulfill", status: 200, headers: [], body: null },
	};
}

test("a mock matches its method and anchored pattern only", () => {
	const mocks = new FlowMocks();
	mocks.register(mock(1, "GET", "^https://x\\.test\\/api\\/items$"));
	assert.equal(mocks.match("GET", "https://x.test/api/items")?.id, 1);
	assert.equal(mocks.match("POST", "https://x.test/api/items"), undefined);
	assert.equal(
		mocks.match("GET", "https://x.test/api/items?page=2"),
		undefined,
	);
});

test("a match ignores the request URL's fragment", () => {
	assert.equal(mockUrl("https://x.test/a#top"), "https://x.test/a");
	const mocks = new FlowMocks();
	mocks.register(mock(1, "GET", "^https://x\\.test\\/a$"));
	assert.equal(mocks.match("GET", "https://x.test/a#top")?.id, 1);
});

test("the mock registered last wins when two patterns match", () => {
	const mocks = new FlowMocks();
	mocks.register(mock(1, "GET", "^https://x\\.test\\/.*$"));
	mocks.register(mock(2, "GET", "^https://x\\.test\\/api\\/.*$"));
	assert.equal(mocks.match("GET", "https://x.test/api/items")?.id, 2);
	assert.equal(mocks.match("GET", "https://x.test/home")?.id, 1);
});

test("a later mock with the same method and pattern replaces the earlier one", () => {
	const mocks = new FlowMocks();
	mocks.register(mock(1, "GET", "^https://x\\.test\\/a$"));
	mocks.register(mock(2, "GET", "^https://x\\.test\\/.*$"));
	mocks.register(mock(3, "GET", "^https://x\\.test\\/a$"));
	assert.equal(mocks.match("GET", "https://x.test/a")?.id, 3);
	assert.deepEqual(mocks.hits(), [
		{ id: 1, hits: 0 },
		{ id: 2, hits: 0 },
		{ id: 3, hits: 0 },
	]);
});
