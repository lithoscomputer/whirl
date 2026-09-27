import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import {
	access,
	mkdir,
	mkdtemp,
	readdir,
	readFile,
	rm,
	writeFile,
} from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { TestContext } from "node:test";
import { test } from "node:test";
import { setTimeout as sleep } from "node:timers/promises";
import { promisify } from "node:util";
import type {
	FfmpegLocation,
	FrameCapture,
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

/** The color of `frameJpeg`. */
const frameColor = [31, 64, 95] as const;

const white = [255, 255, 255] as const;

const unpainted: FrameCapture = { type: "unpainted" };
/** What Chrome answers when it captures a crashed page. */
const crashError =
	"cdpSession.send: Protocol error (Page.captureScreenshot): Internal error";
const failed: FrameCapture = { type: "failed", reason: crashError };
const captured: FrameCapture = { type: "frame", frame: frameJpeg };

interface FakeSource extends FrameSource {
	readonly calls: readonly string[];
}

/**
 * A page that sends `frames` when its screencast starts. `capture`
 * answers each capture, and can send a screencast frame with `send`.
 */
function fakeSource(
	frames: readonly Buffer[],
	capture: (send: (frame: Buffer) => void) => FrameCapture,
): FakeSource {
	const calls: string[] = [];
	let send: (frame: Buffer) => void = () => {};
	return {
		calls,
		// Short, so a page that never paints keeps the tests fast.
		paintWaitMs: 200,
		async start(onFrame) {
			calls.push("start");
			send = onFrame;
			for (const frame of frames) {
				onFrame(frame);
			}
		},
		async capture() {
			calls.push("capture");
			return capture(send);
		},
		async stop() {
			calls.push("stop");
		},
	};
}

/** Answers each capture with the next reply, then repeats the last. */
function answers(
	...replies: readonly [FrameCapture, ...FrameCapture[]]
): () => FrameCapture {
	let next = 0;
	return () => {
		const reply = replies[Math.min(next, replies.length - 1)] ?? replies[0];
		next += 1;
		return reply;
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

interface PngImage {
	readonly data: Buffer;
}

interface UtilsBundle {
	readonly PNG: { readonly sync: { readonly read: (png: Buffer) => PngImage } };
}

const require = createRequire(import.meta.url);
const pngReader = (require("playwright-core/lib/utilsBundle") as UtilsBundle)
	.PNG.sync;

/**
 * Decodes a recording with ffmpeg into 160 by 90 frames, and returns the
 * RGBA pixels of each frame.
 */
async function decodeFrames(
	ffmpegPath: string,
	video: string,
): Promise<readonly Buffer[]> {
	const frames = join(video, "..", "frames");
	await mkdir(frames);
	await promisify(execFile)(ffmpegPath, [
		...["-hide_banner", "-loglevel", "error", "-i", video],
		...["-vf", "scale=160:90", "-f", "image2", join(frames, "f-%05d.png")],
	]);
	const names = (await readdir(frames)).sort();
	return Promise.all(
		names.map(
			async (name) => pngReader.read(await readFile(join(frames, name))).data,
		),
	);
}

/** Checks that every pixel of every frame is within `tolerance` of `rgb`. */
function assertColor(
	frames: readonly Buffer[],
	rgb: readonly [number, number, number],
	tolerance: number,
): void {
	assert.ok(frames.length >= 1, "no frames");
	let distance = 0;
	for (const pixels of frames) {
		for (let index = 0; index < pixels.length; index += 4) {
			for (const channel of [0, 1, 2] as const) {
				distance = Math.max(
					distance,
					Math.abs((pixels[index + channel] ?? 0) - rgb[channel]),
				);
			}
		}
	}
	assert.ok(
		distance <= tolerance,
		`a pixel is ${distance} from ${rgb.join(",")}`,
	);
}

test("a page that sends no frame is captured, and the frame fills the recording", async (t) => {
	const dir = await testDir(t);
	const options = await recordingOptions(dir);
	const source = fakeSource([], answers(captured));
	const recorder = await ScreencastRecorder.start(source, options);
	await sleep(500);
	const video = join(dir, "video.webm");

	const outcome = await recorder.stop(video);

	assert.deepEqual(outcome, { type: "saved" });
	assert.deepEqual(source.calls, ["start", "capture", "stop"]);
	// At 10 fps, 500 ms or more of recording is at least 5 frames.
	const frames = await decodeFrames(options.ffmpegPath, video);
	assert.ok(
		frames.length >= 5,
		`expected at least 5 frames, got ${frames.length}`,
	);
	assertColor(frames, frameColor, 4);
});

test("a capture of a page that has not painted is tried again", async (t) => {
	const dir = await testDir(t);
	const options = await recordingOptions(dir);
	const source = fakeSource([], answers(unpainted, captured));
	const recorder = await ScreencastRecorder.start(source, options);
	const video = join(dir, "video.webm");

	const outcome = await recorder.stop(video);

	assert.deepEqual(outcome, { type: "saved" });
	assert.deepEqual(source.calls, ["start", "capture", "capture", "stop"]);
	const frames = await decodeFrames(options.ffmpegPath, video);
	assert.ok(frames.length >= 1);
	assertColor(frames, frameColor, 4);
});

test("a page that never paints is recorded as a white frame, and the outcome says why", async (t) => {
	const dir = await testDir(t);
	const options = await recordingOptions(dir);
	const source = fakeSource([], answers(unpainted));
	const recorder = await ScreencastRecorder.start(source, options);
	await sleep(300);
	const video = join(dir, "video.webm");

	const outcome = await recorder.stop(video);

	assert.deepEqual(outcome, {
		type: "blank",
		reason: "the page did not paint within 200ms",
	});
	// The page gets a capture every 50 ms for its 200 ms paint wait.
	const captures = source.calls.filter((call) => call === "capture").length;
	assert.ok(captures >= 3, `expected at least 3 captures, got ${captures}`);
	assert.equal(source.calls.at(-1), "stop");
	// The white frame fills the 300 ms before the stop and the 200 ms wait.
	const frames = await decodeFrames(options.ffmpegPath, video);
	assert.ok(
		frames.length >= 5,
		`expected at least 5 frames, got ${frames.length}`,
	);
	assertColor(frames, white, 4);
});

test("a page that cannot be captured, as a crashed one, gets a white frame at once, and the outcome says why", async (t) => {
	const dir = await testDir(t);
	const options = await recordingOptions(dir);
	const source = fakeSource([], answers(failed));
	const recorder = await ScreencastRecorder.start(source, options);
	const video = join(dir, "video.webm");

	const outcome = await recorder.stop(video);

	assert.deepEqual(outcome, {
		type: "blank",
		reason: `capturing the page failed: ${crashError}`,
	});
	assert.deepEqual(source.calls, ["start", "capture", "stop"]);
	const frames = await decodeFrames(options.ffmpegPath, video);
	assertColor(frames, white, 4);
});

test("a screencast frame that arrives while the page is tried again is used", async (t) => {
	const dir = await testDir(t);
	const options = await recordingOptions(dir);
	// The frame arrives 10 ms into the 50 ms wait after the first capture.
	const source = fakeSource([], (send) => {
		setTimeout(() => send(frameJpeg), 10);
		return unpainted;
	});
	const recorder = await ScreencastRecorder.start(source, options);
	const video = join(dir, "video.webm");

	const outcome = await recorder.stop(video);

	assert.deepEqual(outcome, { type: "saved" });
	assert.deepEqual(source.calls, ["start", "capture", "stop"]);
	const frames = await decodeFrames(options.ffmpegPath, video);
	assertColor(frames, frameColor, 4);
});

test("a page that sends a frame is not captured", async (t) => {
	const dir = await testDir(t);
	const options = await recordingOptions(dir);
	const source = fakeSource([frameJpeg], () => {
		throw new Error("capture should not run");
	});
	const recorder = await ScreencastRecorder.start(source, options);
	const video = join(dir, "video.webm");

	const outcome = await recorder.stop(video);

	assert.deepEqual(outcome, { type: "saved" });
	assert.deepEqual(source.calls, ["start", "stop"]);
	assert.ok((await decodeFrames(options.ffmpegPath, video)).length >= 1);
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
	const source = fakeSource([frameJpeg], answers(failed));
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
