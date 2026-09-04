import assert from "node:assert/strict";
import { PassThrough } from "node:stream";
import { test } from "node:test";
import { Dispatcher } from "./dispatcher.js";
import type { ShimDriver } from "./driver.js";
import type { Params } from "./params.js";
import type {
	EndFlowParams,
	EndFlowResult,
	StartFlowParams,
	StepCommand,
} from "./protocol.js";
import { ShimError } from "./protocol.js";

interface Deferred {
	readonly promise: Promise<Params>;
	resolve: (value: Params) => void;
	reject: (reason: unknown) => void;
}

function deferred(): Deferred {
	let resolve: (value: Params) => void = () => {};
	let reject: (reason: unknown) => void = () => {};
	const promise = new Promise<Params>((res, rej) => {
		resolve = res;
		reject = rej;
	});
	return { promise, resolve, reject };
}

class FakeDriver implements ShimDriver {
	readonly playwrightVersion = "0.0.0-test";
	readonly log: string[] = [];
	stepGate: Deferred | null = null;
	cancelled = 0;

	async startFlow(_params: StartFlowParams): Promise<Params> {
		this.log.push("startFlow");
		return {};
	}

	async endFlow(_params: EndFlowParams): Promise<EndFlowResult> {
		this.log.push("endFlow");
		return { blockedHosts: ["a.example.com"], videoPath: null };
	}

	async cancelFlow(): Promise<void> {
		this.cancelled += 1;
		this.log.push("cancelFlow");
	}

	async runStep(cmd: StepCommand, _params: Params): Promise<Params> {
		this.log.push(`step:${cmd}`);
		if (this.stepGate !== null) {
			return this.stepGate.promise;
		}
		return {};
	}

	async dispose(): Promise<void> {
		this.log.push("dispose");
	}
}

interface Harness {
	readonly driver: FakeDriver;
	readonly send: (line: string) => void;
	readonly responses: () => Record<string, unknown>[];
	readonly waitForResponses: (
		count: number,
	) => Promise<Record<string, unknown>[]>;
	readonly exitCodes: number[];
	readonly close: () => void;
}

function startHarness(): Harness {
	const input = new PassThrough();
	const output = new PassThrough();
	const driver = new FakeDriver();
	const exitCodes: number[] = [];
	const dispatcher = new Dispatcher({
		input,
		output,
		driver,
		onExit: (code) => exitCodes.push(code),
		log: () => {},
	});
	dispatcher.run();
	let buffered = "";
	const parsed: Record<string, unknown>[] = [];
	output.on("data", (chunk: Buffer) => {
		buffered += chunk.toString("utf8");
		let index = buffered.indexOf("\n");
		while (index !== -1) {
			const line = buffered.slice(0, index);
			buffered = buffered.slice(index + 1);
			parsed.push(JSON.parse(line) as Record<string, unknown>);
			index = buffered.indexOf("\n");
		}
	});
	const waitForResponses = async (
		count: number,
	): Promise<Record<string, unknown>[]> => {
		const deadline = Date.now() + 2000;
		while (parsed.length < count) {
			if (Date.now() > deadline) {
				throw new Error(
					`timed out waiting for ${String(count)} responses; got ${String(
						parsed.length,
					)}`,
				);
			}
			await new Promise((resolve) => setTimeout(resolve, 5));
		}
		return parsed;
	};
	return {
		driver,
		send: (line) => {
			input.write(`${line}\n`);
		},
		responses: () => parsed,
		waitForResponses,
		exitCodes,
		close: () => {
			input.end();
			output.destroy();
		},
	};
}

const startFlowParams = JSON.stringify({
	browser: "chromium",
	headed: false,
	viewport: { width: 1280, height: 720 },
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

test("hello answers protocol 1 and the Playwright version", async (t) => {
	const harness = startHarness();
	t.after(harness.close);
	harness.send('{"id": 1, "cmd": "hello", "params": {}}');
	const [response] = await harness.waitForResponses(1);
	assert.deepEqual(response, {
		id: 1,
		ok: true,
		result: { protocol: 1, playwrightVersion: "0.0.0-test" },
	});
});

test("startFlow and endFlow round-trip through the driver", async (t) => {
	const harness = startHarness();
	t.after(harness.close);
	harness.send(`{"id": 1, "cmd": "startFlow", "params": ${startFlowParams}}`);
	harness.send(
		'{"id": 2, "cmd": "endFlow", "params": {"saveStoragePath": null, "tracePath": null}}',
	);
	const responses = await harness.waitForResponses(2);
	assert.deepEqual(responses[0], { id: 1, ok: true, result: {} });
	assert.deepEqual(responses[1], {
		id: 2,
		ok: true,
		result: { blockedHosts: ["a.example.com"], videoPath: null },
	});
});

test("cancelFlow interleaves with a slow in-flight step", async (t) => {
	const harness = startHarness();
	t.after(harness.close);
	harness.driver.stepGate = deferred();
	harness.send(`{"id": 1, "cmd": "startFlow", "params": ${startFlowParams}}`);
	await harness.waitForResponses(1);
	harness.send(
		'{"id": 2, "cmd": "click", "params": {"timeoutMs": 5000, "title": "CLICK x", "locator": []}}',
	);
	// The read loop keeps consuming stdin while the step is in flight.
	harness.send('{"id": 3, "cmd": "cancelFlow", "params": {}}');
	const responses = await harness.waitForResponses(3);
	const stepResponse = responses.find((response) => response["id"] === 2);
	const cancelResponse = responses.find((response) => response["id"] === 3);
	assert.ok(stepResponse !== undefined);
	assert.equal(stepResponse["ok"], false);
	const error = stepResponse["error"] as Record<string, unknown>;
	assert.equal(error["kind"], "cancelled");
	assert.deepEqual(cancelResponse, { id: 3, ok: true, result: {} });
	assert.equal(harness.driver.cancelled, 1);

	// The abandoned step settling later must not produce a second response.
	harness.driver.stepGate.resolve({});
	harness.driver.stepGate = null;
	await new Promise((resolve) => setTimeout(resolve, 50));
	assert.equal(harness.responses().length, 3);

	// The shim is ready for the next startFlow.
	harness.send(`{"id": 4, "cmd": "startFlow", "params": ${startFlowParams}}`);
	const all = await harness.waitForResponses(4);
	assert.deepEqual(all[3], { id: 4, ok: true, result: {} });
});

test("a step error becomes exactly one error response", async (t) => {
	const harness = startHarness();
	t.after(harness.close);
	harness.driver.stepGate = deferred();
	harness.send(`{"id": 1, "cmd": "startFlow", "params": ${startFlowParams}}`);
	harness.send(
		'{"id": 2, "cmd": "assert", "params": {"timeoutMs": 100, "title": "t", "spec": {}}}',
	);
	harness.driver.stepGate.reject(
		new ShimError("assert", "check failed", {
			expected: "visible",
			actual: "hidden",
		}),
	);
	const responses = await harness.waitForResponses(2);
	assert.deepEqual(responses[1], {
		id: 2,
		ok: false,
		error: {
			kind: "assert",
			message: "check failed",
			expected: "visible",
			actual: "hidden",
		},
	});
});

test("malformed JSON answers kind internal with the recovered id", async (t) => {
	const harness = startHarness();
	t.after(harness.close);
	harness.send('{"id": 7, "cmd": "hello", params-junk');
	const [response] = await harness.waitForResponses(1);
	assert.ok(response !== undefined);
	assert.equal(response["id"], 7);
	assert.equal(response["ok"], false);
	const error = response["error"] as Record<string, unknown>;
	assert.equal(error["kind"], "internal");
});

test("an unknown command answers kind internal", async (t) => {
	const harness = startHarness();
	t.after(harness.close);
	harness.send('{"id": 5, "cmd": "teleport", "params": {}}');
	const [response] = await harness.waitForResponses(1);
	assert.ok(response !== undefined);
	assert.equal(response["ok"], false);
	const error = response["error"] as Record<string, unknown>;
	assert.equal(error["kind"], "internal");
	assert.match(String(error["message"]), /unknown command/);
});

test("malformed startFlow params answer kind internal", async (t) => {
	const harness = startHarness();
	t.after(harness.close);
	harness.send(
		'{"id": 6, "cmd": "startFlow", "params": {"browser": "netscape"}}',
	);
	const [response] = await harness.waitForResponses(1);
	assert.ok(response !== undefined);
	assert.equal(response["ok"], false);
	const error = response["error"] as Record<string, unknown>;
	assert.equal(error["kind"], "internal");
});

test("shutdown replies, disposes the driver, and exits 0", async (t) => {
	const harness = startHarness();
	t.after(harness.close);
	harness.send('{"id": 9, "cmd": "shutdown", "params": {}}');
	const [response] = await harness.waitForResponses(1);
	assert.deepEqual(response, { id: 9, ok: true, result: {} });
	const deadline = Date.now() + 1000;
	while (harness.exitCodes.length === 0 && Date.now() < deadline) {
		await new Promise((resolve) => setTimeout(resolve, 5));
	}
	assert.deepEqual(harness.exitCodes, [0]);
	assert.ok(harness.driver.log.includes("dispose"));
});
