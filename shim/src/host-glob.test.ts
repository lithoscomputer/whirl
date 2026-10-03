import assert from "node:assert/strict";
import { test } from "node:test";
import {
	BlockedHostLog,
	createHostAllowlist,
	createHostPolicy,
	describeBlockedHost,
	hostnameMatchesGlob,
} from "./host-glob.js";

test("plain glob matches the exact hostname only", () => {
	assert.equal(hostnameMatchesGlob("example.com", "example.com"), true);
	assert.equal(hostnameMatchesGlob("www.example.com", "example.com"), false);
	assert.equal(
		hostnameMatchesGlob("example.com.evil.io", "example.com"),
		false,
	);
	assert.equal(hostnameMatchesGlob("anexample.com", "example.com"), false);
});

test("*.example.com matches subdomains but never the apex", () => {
	assert.equal(hostnameMatchesGlob("www.example.com", "*.example.com"), true);
	assert.equal(hostnameMatchesGlob("a.b.example.com", "*.example.com"), true);
	assert.equal(hostnameMatchesGlob("example.com", "*.example.com"), false);
});

test("* matches any run of characters including dots", () => {
	assert.equal(
		hostnameMatchesGlob("deep.sub.example.com", "*.example.com"),
		true,
	);
	assert.equal(hostnameMatchesGlob("anything.at.all", "*"), true);
	assert.equal(
		hostnameMatchesGlob("api-v2.example.com", "api-*.example.com"),
		true,
	);
});

test("matching is case-insensitive", () => {
	assert.equal(hostnameMatchesGlob("Example.COM", "example.com"), true);
	assert.equal(hostnameMatchesGlob("www.EXAMPLE.com", "*.Example.Com"), true);
});

test("IP literals match textually", () => {
	assert.equal(hostnameMatchesGlob("127.0.0.1", "127.0.0.1"), true);
	assert.equal(hostnameMatchesGlob("127.0.0.2", "127.0.0.1"), false);
	// A dot in the glob is literal, never a wildcard.
	assert.equal(hostnameMatchesGlob("127a0b0c1", "127.0.0.1"), false);
});

test("IPv6 literals match with or without brackets", () => {
	assert.equal(hostnameMatchesGlob("[::1]", "::1"), true);
	assert.equal(hostnameMatchesGlob("::1", "[::1]"), true);
	assert.equal(hostnameMatchesGlob("[::1]", "[::1]"), true);
	assert.equal(hostnameMatchesGlob("[::2]", "::1"), false);
});

test("createHostAllowlist accepts a hostname matching any glob", () => {
	const isAllowed = createHostAllowlist(["example.com", "*.example.com"]);
	assert.equal(isAllowed("example.com"), true);
	assert.equal(isAllowed("shop.example.com"), true);
	assert.equal(isAllowed("evil.io"), false);
});

test("a block-hosts glob wins over allow-hosts and names its rule", () => {
	const policy = createHostPolicy(
		["127.0.0.1", "example.com", "*.example.com"],
		["ads.example.com", "127.0.0.1"],
	);
	assert.equal(policy("shop.example.com"), null);
	assert.deepEqual(policy("ADS.example.com"), {
		host: "ads.example.com",
		option: "block-hosts",
		glob: "ads.example.com",
	});
	// The app-url host that Rust appends to allow-hosts is blocked too.
	assert.equal(policy("127.0.0.1")?.option, "block-hosts");
	assert.deepEqual(policy("evil.io"), {
		host: "evil.io",
		option: "allow-hosts",
		glob: null,
	});
});

test("block-hosts alone blocks only its matches", () => {
	const policy = createHostPolicy(null, ["*.analytics.example.com"]);
	assert.equal(policy("analytics.example.com"), null);
	assert.equal(policy("shop.example.com"), null);
	const blocked = policy("eu.analytics.example.com");
	assert.notEqual(blocked, null);
	if (blocked !== null) {
		assert.equal(
			describeBlockedHost(blocked),
			"block-hosts *.analytics.example.com",
		);
	}
});

test("the blocked host log keeps each host's first rule, sorted", () => {
	const log = new BlockedHostLog();
	log.record({ host: "b.example", option: "allow-hosts", glob: null });
	log.record({ host: "a.example", option: "block-hosts", glob: "a.example" });
	log.record({ host: "b.example", option: "block-hosts", glob: "*.example" });
	assert.deepEqual(log.list(), [
		{ host: "a.example", option: "block-hosts", glob: "a.example" },
		{ host: "b.example", option: "allow-hosts", glob: null },
	]);
});

test("regex metacharacters in globs stay literal", () => {
	assert.equal(hostnameMatchesGlob("a+b.example.com", "a+b.example.com"), true);
	assert.equal(
		hostnameMatchesGlob("aab.example.com", "a+b.example.com"),
		false,
	);
});
