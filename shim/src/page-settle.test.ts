import assert from "node:assert/strict";
import { test } from "node:test";
import { PageActivity, quietMs, staleRequestMs } from "./page-settle.js";

function request(type: string): { resourceType: () => string } {
	return { resourceType: () => type };
}

test("the network is quiet only after every request ends and the quiet time passes", () => {
	let now = 10_000;
	const activity = new PageActivity(() => now);
	assert.equal(activity.quiet(), true, "a page with no requests is quiet");
	const fetch = request("fetch");
	activity.started(fetch);
	now += quietMs * 2;
	assert.equal(activity.quiet(), false, "an open request blocks");
	activity.ended(fetch);
	now += quietMs - 1;
	assert.equal(activity.quiet(), false, "the quiet time starts at the end");
	now += 1;
	assert.equal(activity.quiet(), true);
});

test("streams and requests open too long do not block", () => {
	let now = 0;
	const activity = new PageActivity(() => now);
	activity.started(request("websocket"));
	activity.started(request("eventsource"));
	now += quietMs;
	assert.equal(activity.quiet(), true, "streams never count");
	activity.started(request("xhr"));
	now += staleRequestMs;
	assert.equal(activity.quiet(), true, "a long poll stops counting");
});
