import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import {
	access,
	mkdir,
	mkdtemp,
	readdir,
	rm,
	writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { TestContext } from "node:test";
import { test } from "node:test";
import { setTimeout as sleep } from "node:timers/promises";
import { promisify } from "node:util";
import type {
	FfmpegLocation,
	FrameSource,
	ScreencastOptions,
} from "./video-recorder.js";
import {
	FrameClock,
	ffmpegArgs,
	ffmpegExecutablePath,
	resolveFfmpegPath,
	ScreencastRecorder,
} from "./video-recorder.js";

function location(overrides: Partial<FfmpegLocation>): FfmpegLocation {
	return {
		platform: "darwin",
		env: {},
		homeDir: "/Users/tester",
		cwd: "/work",
		packageRoot: "/work/node_modules/playwright-core",
		revision: "1011",
		...overrides,
	};
}

test("macOS uses the Library cache and the mac executable", () => {
	assert.equal(
		ffmpegExecutablePath(location({})),
		"/Users/tester/Library/Caches/ms-playwright/ffmpeg-1011/ffmpeg-mac",
	);
});

test("Linux uses XDG_CACHE_HOME when set and ~/.cache otherwise", () => {
	assert.equal(
		ffmpegExecutablePath(location({ platform: "linux", homeDir: "/home/t" })),
		"/home/t/.cache/ms-playwright/ffmpeg-1011/ffmpeg-linux",
	);
	assert.equal(
		ffmpegExecutablePath(
			location({
				platform: "linux",
				homeDir: "/home/t",
				env: { XDG_CACHE_HOME: "/var/cache/t" },
			}),
		),
		"/var/cache/t/ms-playwright/ffmpeg-1011/ffmpeg-linux",
	);
});

test("PLAYWRIGHT_BROWSERS_PATH replaces the registry directory", () => {
	assert.equal(
		ffmpegExecutablePath(
			location({ env: { PLAYWRIGHT_BROWSERS_PATH: "/opt/browsers" } }),
		),
		"/opt/browsers/ffmpeg-1011/ffmpeg-mac",
	);
	// A relative value resolves against INIT_CWD, then the working directory.
	assert.equal(
		ffmpegExecutablePath(
			location({ env: { PLAYWRIGHT_BROWSERS_PATH: "browsers" } }),
		),
		"/work/browsers/ffmpeg-1011/ffmpeg-mac",
	);
	assert.equal(
		ffmpegExecutablePath(
			location({
				env: { PLAYWRIGHT_BROWSERS_PATH: "browsers", INIT_CWD: "/init" },
			}),
		),
		"/init/browsers/ffmpeg-1011/ffmpeg-mac",
	);
	assert.equal(
		ffmpegExecutablePath(location({ env: { PLAYWRIGHT_BROWSERS_PATH: "0" } })),
		"/work/node_modules/playwright-core/.local-browsers/ffmpeg-1011/ffmpeg-mac",
	);
});

test("Windows uses LOCALAPPDATA and the win64 executable", () => {
	assert.equal(
		ffmpegExecutablePath(
			location({
				platform: "win32",
				homeDir: "C:\\Users\\t",
				env: { LOCALAPPDATA: "C:\\Users\\t\\AppData\\Local" },
			}),
		),
		join(
			"C:\\Users\\t\\AppData\\Local",
			"ms-playwright",
			"ffmpeg-1011",
			"ffmpeg-win64.exe",
		),
	);
});

test("the first frame starts the clock and owes nothing", () => {
	const clock = new FrameClock(60);
	assert.equal(clock.due(1000), 0);
	assert.equal(clock.written, 0);
});

test("slots are counted from the start so rounding never drifts", () => {
	const clock = new FrameClock(60);
	clock.due(0);
	// 1 second in, 60 slots are due regardless of how the gaps fell.
	assert.equal(clock.due(500), 30);
	clock.advance(30);
	assert.equal(clock.due(1000), 30);
	clock.advance(30);
	assert.equal(clock.written, 60);
	// Ten uneven gaps over the next second still sum to 60 slots.
	let total = 0;
	for (const at of [
		1017, 1033, 1051, 1066, 1084, 1100, 1117, 1134, 1151, 2000,
	]) {
		const due = clock.due(at);
		clock.advance(due);
		total += due;
	}
	assert.equal(total, 60);
});

test("frames faster than the rate owe zero slots and are dropped", () => {
	const clock = new FrameClock(60);
	clock.due(0);
	assert.equal(clock.due(5), 0);
	assert.equal(clock.due(10), 1);
});

test("a long static gap owes the whole gap to the held frame", () => {
	const clock = new FrameClock(30);
	clock.due(0);
	assert.equal(clock.due(4000), 120);
});

test("a clock started at the recording's start owes the time before it", () => {
	const clock = new FrameClock(10);
	clock.startAt(1000);
	assert.equal(clock.due(1500), 5);
});

test("startAt does not move a clock that a frame started", () => {
	const clock = new FrameClock(10);
	clock.due(1000);
	clock.startAt(0);
	assert.equal(clock.due(1500), 5);
});

test("ffmpeg receives a constant-rate MJPEG stream and scales the bitrate", () => {
	const args = ffmpegArgs(
		{
			fps: 60,
			width: 1280,
			height: 720,
			tempDir: "/tmp/v",
			ffmpegPath: "/ffmpeg",
		},
		"/tmp/v/out.webm",
	);
	assert.deepEqual(args.slice(0, 9), [
		"-loglevel",
		"error",
		"-f",
		"image2pipe",
		"-framerate",
		"60",
		"-c:v",
		"mjpeg",
		"-i",
	]);
	assert.ok(args.includes("2400k"));
	assert.ok(args.includes("pad=1280:720:0:0:gray,crop=1280:720:0:0"));
	assert.equal(args.at(-1), "/tmp/v/out.webm");
	const slow = ffmpegArgs(
		{ fps: 10, width: 8, height: 8, tempDir: "/tmp/v", ffmpegPath: "/f" },
		"/tmp/v/o.webm",
	);
	assert.ok(slow.includes("1000k"), "the bitrate never drops below 1000k");
});

// --- recordings through Playwright's ffmpeg ---

/** A 16 by 16 JPEG of one color, from Chrome's screenshot encoder. */
const frameJpeg = Buffer.from(
	"/9j/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsNDhIQDQ4RDgsLEBYQERMUFRUVDA8XGBYUGBIUFRT/2wBDAQMEBAUEBQkFBQkUDQsNFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBT/wAARCAAQABADASIAAhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAAAAj/xAAUEAEAAAAAAAAAAAAAAAAAAAAA/8QAFAEBAAAAAAAAAAAAAAAAAAAABv/EABQRAQAAAAAAAAAAAAAAAAAAAAD/2gAMAwEAAhEDEQA/AJEAOAx//9k=",
	"base64",
);

interface FakeSource extends FrameSource {
	readonly calls: readonly string[];
}

/** A page that sends `frames` when its screencast starts, and no more. */
function fakeSource(
	frames: readonly Buffer[],
	capture: () => Promise<Buffer>,
): FakeSource {
	const calls: string[] = [];
	return {
		calls,
		async start(onFrame) {
			calls.push("start");
			for (const frame of frames) {
				onFrame(frame);
			}
		},
		async capture() {
			calls.push("capture");
			return capture();
		},
		async stop() {
			calls.push("stop");
		},
	};
}

/** A temporary directory that the test removes when it ends. */
async function testDir(t: TestContext): Promise<string> {
	const dir = await mkdtemp(join(tmpdir(), "whirl-recorder-"));
	t.after(() => rm(dir, { recursive: true, force: true }));
	return dir;
}

async function recordingOptions(dir: string): Promise<ScreencastOptions> {
	const ffmpegPath = await resolveFfmpegPath();
	assert.ok(
		ffmpegPath !== null,
		"Playwright's ffmpeg is missing; run `mise run setup`",
	);
	return {
		fps: 10,
		width: 16,
		height: 16,
		tempDir: join(dir, "temp"),
		ffmpegPath,
	};
}

/** Decodes a recording with ffmpeg and counts its frames. */
async function frameCount(ffmpegPath: string, video: string): Promise<number> {
	const frames = join(video, "..", "frames");
	await mkdir(frames);
	await promisify(execFile)(ffmpegPath, [
		...["-hide_banner", "-loglevel", "error", "-i", video],
		...["-f", "image2", join(frames, "f-%05d.png")],
	]);
	return (await readdir(frames)).length;
}

test("a page that sends no frame is captured once, and the frame fills the recording", async (t) => {
	const dir = await testDir(t);
	const options = await recordingOptions(dir);
	const source = fakeSource([], async () => frameJpeg);
	const recorder = await ScreencastRecorder.start(source, options);
	await sleep(500);
	const video = join(dir, "video.webm");

	const outcome = await recorder.stop(video);

	assert.deepEqual(outcome, { type: "saved" });
	assert.deepEqual(source.calls, ["start", "capture", "stop"]);
	// At 10 fps, 500 ms or more of recording is at least 5 frames.
	const frames = await frameCount(options.ffmpegPath, video);
	assert.ok(frames >= 5, `expected at least 5 frames, got ${frames}`);
});

test("a page that sends no frame and cannot be captured skips the recording", async (t) => {
	const dir = await testDir(t);
	const options = await recordingOptions(dir);
	const source = fakeSource([], async () => {
		throw new Error("Target crashed");
	});
	const recorder = await ScreencastRecorder.start(source, options);
	const video = join(dir, "video.webm");

	const outcome = await recorder.stop(video);

	assert.deepEqual(outcome, {
		type: "skipped",
		reason:
			"the page produced no frames, and capturing it failed: Target crashed",
	});
	assert.deepEqual(source.calls, ["start", "capture", "stop"]);
	await assert.rejects(access(video));
	assert.deepEqual(await readdir(options.tempDir), []);
});

test("a page that sends a frame is not captured", async (t) => {
	const dir = await testDir(t);
	const options = await recordingOptions(dir);
	const source = fakeSource([frameJpeg], async () => {
		throw new Error("capture should not run");
	});
	const recorder = await ScreencastRecorder.start(source, options);
	const video = join(dir, "video.webm");

	const outcome = await recorder.stop(video);

	assert.deepEqual(outcome, { type: "saved" });
	assert.deepEqual(source.calls, ["start", "stop"]);
	assert.ok((await frameCount(options.ffmpegPath, video)) >= 1);
});

test("an ffmpeg that fails while it finishes skips the recording", async (t) => {
	const dir = await testDir(t);
	// It writes part of its output, as ffmpeg does, then fails.
	const ffmpegPath = join(dir, "ffmpeg");
	await writeFile(
		ffmpegPath,
		[
			"#!/bin/sh",
			'for output; do :; done; echo partial > "$output"',
			"echo 'pipe:0: Invalid data found when processing input' >&2",
			"exit 1",
		].join("\n"),
		{ mode: 0o755 },
	);
	const options = { ...(await recordingOptions(dir)), ffmpegPath };
	const source = fakeSource([frameJpeg], async () => {
		throw new Error("capture should not run");
	});
	const recorder = await ScreencastRecorder.start(source, options);
	const video = join(dir, "video.webm");

	const outcome = await recorder.stop(video);

	assert.deepEqual(outcome, {
		type: "skipped",
		reason:
			"ffmpeg exited with status 1: pipe:0: Invalid data found when processing input",
	});
	assert.deepEqual(source.calls, ["start", "stop"]);
	await assert.rejects(access(video));
	assert.deepEqual(await readdir(options.tempDir), []);
});
