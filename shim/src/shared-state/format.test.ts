import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import { z } from "zod";
import { databaseRecord } from "./database.js";
import { checkStateCapabilities, parseState, stateSchema } from "./format.js";

const fixture = async () =>
	parseState(
		JSON.parse(
			await readFile(
				new URL("../../../state/fixtures/full.state.json", import.meta.url),
				"utf8",
			),
		),
	);
const minimal = () => ({
	format: "whirl-state",
	version: 1,
	redacted: false,
	cookies: [],
	origins: [],
});

test("shared fixture preserves binary aliases, undefined and nextKey", async () => {
	const state = await fixture();
	const saved = state.origins[0]?.indexedDB?.[0]?.stores[0]?.records[0];
	assert.ok(saved);
	const value = databaseRecord(saved, true).value;
	assert.ok(value !== null && typeof value === "object");
	assert.ok("bytes" in value && value.bytes instanceof Uint8Array);
	assert.ok("alias" in value && value.alias instanceof DataView);
	assert.equal(value.bytes.buffer, value.alias.buffer);
	assert.deepEqual([...value.bytes], [11, 22, 33, 44]);
	assert.ok(Object.hasOwn(value, "optional"));
	assert.ok("optional" in value && value.optional === undefined);
	assert.equal(state.origins[0]?.indexedDB?.[0]?.stores[0]?.nextKey, 50);
});

test("schema artifact matches the runtime schema", async () => {
	const schema = JSON.parse(
		await readFile(
			new URL("../../../state/whirl-state.schema.json", import.meta.url),
			"utf8",
		),
	);
	assert.deepEqual(schema, z.toJSONSchema(stateSchema));
});

test("missing storage differs from captured empty storage", () => {
	const state = parseState({
		...minimal(),
		origins: [
			{ origin: "https://app.test" },
			{ origin: "https://login.test", localStorage: [], indexedDB: [] },
		],
	});
	const first = state.origins[0];
	assert.ok(first);
	assert.ok(!Object.hasOwn(first, "localStorage"));
	assert.deepEqual(state.origins[1]?.localStorage, []);
});

test("unknown versions fail without exposing values", () => {
	assert.throws(
		() =>
			parseState({
				...minimal(),
				version: 2,
				cookies: [{ value: "private-cookie" }],
			}),
		(error) =>
			error instanceof Error && !error.message.includes("private-cookie"),
	);
});

test("origins must be canonical HTTP(S) origins", () => {
	assert.throws(
		() =>
			parseState({
				...minimal(),
				origins: [{ origin: "https://app.test/path" }],
			}),
		/Invalid state/,
	);
});

test("duplicate identities fail instead of overwriting state", async () => {
	const state = await fixture();
	const cookie = state.cookies[0];
	assert.ok(cookie);
	state.cookies.push({ ...cookie, hostOnly: false });
	assert.throws(() => parseState(state), /duplicate identities/);
});

test("autoIncrement requires nextKey", async () => {
	const state = await fixture();
	const store = state.origins[0]?.indexedDB?.[0]?.stores[0];
	assert.ok(store);
	delete store.nextKey;
	assert.throws(() => parseState(state), /requires nextKey/);
});

test("binary data has no reserved redaction marker", async () => {
	const state = await fixture();
	const saved = state.origins[0]?.indexedDB?.[0]?.stores[0]?.records[0];
	assert.ok(saved);
	assert.ok(
		saved.value !== null &&
			typeof saved.value === "object" &&
			"bytes" in saved.value,
	);
	saved.value.bytes = "browsersim-redacted";
	assert.throws(() => parseState(state), /unsupported IndexedDB record/);
});

test("partitioned cookies fail capability checks", async () => {
	const state = await fixture();
	const cookie = state.cookies[0];
	assert.ok(cookie);
	cookie.partition = {
		topLevelSite: "https://example.test",
		hasCrossSiteAncestor: false,
	};
	assert.throws(() => checkStateCapabilities(state), /Partitioned cookies/);
});

test("unbound pages fail rather than losing session storage", () => {
	const state = parseState({
		...minimal(),
		pages: [{ id: "popup", origins: [] }],
	});
	assert.throws(() => checkStateCapabilities(state), /main page/);
});
