import assert from "node:assert/strict";
import { join } from "node:path";
import { test } from "node:test";
import type { FfmpegLocation } from "./video-recorder.js";
import {
	FrameClock,
	ffmpegArgs,
	ffmpegExecutablePath,
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
