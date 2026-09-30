import assert from "node:assert/strict";
import {
	link,
	mkdir,
	mkdtemp,
	readdir,
	readFile,
	rm,
	stat,
	symlink,
	writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { assertStateOutput, writeState } from "./adapter.js";
import type { SharedState } from "./format.js";
import { parseState } from "./format.js";

const empty = () =>
	parseState({
		format: "whirl-state",
		version: 1,
		redacted: false,
		cookies: [],
		origins: [],
	});

test("atomic replacement uses owner-only permissions", async () => {
	const directory = await mkdtemp(join(tmpdir(), "whirl-state-"));
	try {
		const path = join(directory, "app.state.json");
		await writeFile(path, "old", { mode: 0o644 });
		await writeState(path, empty(), []);
		assert.equal((await stat(path)).mode & 0o777, 0o600);
		assert.deepEqual(JSON.parse(await readFile(path, "utf8")), empty());
		assert.deepEqual(await readdir(directory), ["app.state.json"]);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});

test("state output refuses symlink and hard-link aliases of its input", async () => {
	const directory = await mkdtemp(join(tmpdir(), "whirl-state-"));
	try {
		const input = join(directory, "input.state.json"),
			alias = join(directory, "alias.state.json"),
			hard = join(directory, "hard.state.json");
		await writeFile(input, "original");
		await symlink(input, alias);
		await link(input, hard);
		await assert.rejects(
			assertStateOutput(alias, [input]),
			/overwrite an input/,
		);
		await assert.rejects(
			assertStateOutput(hard, [input]),
			/overwrite an input/,
		);
		assert.equal(await readFile(input, "utf8"), "original");
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});

test("validation failure keeps the previous output and removes staging files", async () => {
	const directory = await mkdtemp(join(tmpdir(), "whirl-state-"));
	try {
		const path = join(directory, "app.state.json");
		await writeState(path, empty(), []);
		const before = await readFile(path, "utf8");
		const invalid = { ...empty(), version: 2 } as unknown as SharedState;
		await assert.rejects(writeState(path, invalid, []), /Invalid state/);
		assert.equal(await readFile(path, "utf8"), before);
		assert.deepEqual(await readdir(directory), ["app.state.json"]);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});

test("rename failure leaves its destination intact and removes staging files", async () => {
	const directory = await mkdtemp(join(tmpdir(), "whirl-state-"));
	try {
		const path = join(directory, "app.state.json");
		await mkdir(path);
		await writeFile(join(path, "marker"), "original");
		await assert.rejects(writeState(path, empty(), []));
		assert.equal(await readFile(join(path, "marker"), "utf8"), "original");
		assert.deepEqual(await readdir(directory), ["app.state.json"]);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});
