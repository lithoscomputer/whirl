// The screencast video recorder: Chrome DevTools Protocol screencast frames
// piped into Playwright's bundled ffmpeg as a constant-rate MJPEG stream,
// encoded to VP8 WebM at the requested frame rate (SPEC section 13).
//
// Playwright's own recorder is the same design with the rate fixed at 25.
// This one never runs a timer: frame arrival times decide how many copies
// of the held frame fill the constant-rate slots, so a static page holds
// its last frame and a fast page drops frames, without clock drift.

import type { ChildProcess } from "node:child_process";
import { spawn } from "node:child_process";
import { access, mkdir, readFile, rename, rm } from "node:fs/promises";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import type { CDPSession, Page } from "@playwright/test";
import { ShimError } from "./protocol.js";

const require = createRequire(import.meta.url);

/** The rate of Playwright's own recorder, used for Firefox and WebKit. */
export const PLAYWRIGHT_VIDEO_FPS = 25;

/** Bounds for an explicit rate (SPEC section 13). */
export const MIN_VIDEO_FPS = 1;
export const MAX_VIDEO_FPS = 60;

/** JPEG quality of screencast frames, as Playwright's recorder uses. */
const screencastQuality = 90;

/** How long `stop` waits for ffmpeg to flush and exit before killing it. */
const ffmpegStopTimeoutMs = 10_000;

/** How long `abort` waits for a killed ffmpeg to exit. */
const ffmpegAbortTimeoutMs = 3000;

/** Kept bytes of ffmpeg's most recent stderr output. */
const stderrTailLimit = 4096;

// --- ffmpeg location ---

/** The inputs Playwright's registry uses to place its ffmpeg build. */
export interface FfmpegLocation {
	readonly platform: NodeJS.Platform;
	readonly env: Readonly<Record<string, string | undefined>>;
	readonly homeDir: string;
	readonly cwd: string;
	/** The `playwright-core` package directory. */
	readonly packageRoot: string;
	/** The ffmpeg revision from `playwright-core/browsers.json`. */
	readonly revision: string;
}

/**
 * The ffmpeg executable path Playwright's registry would use. Mirrors the
 * registry directory rules (`PLAYWRIGHT_BROWSERS_PATH`, else the platform
 * cache directory plus `ms-playwright`) and the per-platform file name.
 */
export function ffmpegExecutablePath(location: FfmpegLocation): string {
	const { platform, env, homeDir } = location;
	const executable = (() => {
		switch (platform) {
			case "darwin":
				return "ffmpeg-mac";
			case "linux":
				return "ffmpeg-linux";
			case "win32":
				return "ffmpeg-win64.exe";
			default:
				throw new ShimError(
					"internal",
					`video recording is not supported on ${platform}`,
				);
		}
	})();
	const registryDirectory = (() => {
		const configured = env["PLAYWRIGHT_BROWSERS_PATH"];
		if (configured === "0") {
			return join(location.packageRoot, ".local-browsers");
		}
		if (configured !== undefined && configured !== "") {
			return resolve(env["INIT_CWD"] ?? location.cwd, configured);
		}
		const cacheDirectory = (() => {
			switch (platform) {
				case "darwin":
					return join(homeDir, "Library", "Caches");
				case "linux":
					return env["XDG_CACHE_HOME"] || join(homeDir, ".cache");
				default:
					return env["LOCALAPPDATA"] || join(homeDir, "AppData", "Local");
			}
		})();
		return join(cacheDirectory, "ms-playwright");
	})();
	return join(registryDirectory, `ffmpeg-${location.revision}`, executable);
}

interface BrowsersJson {
	readonly browsers: readonly {
		readonly name: string;
		readonly revision: string;
	}[];
}

/** Playwright's bundled ffmpeg, or null when it is not installed. */
export async function resolveFfmpegPath(): Promise<string | null> {
	// Only package.json is in the package's export map; browsers.json is
	// read from the same directory.
	const packageRoot = dirname(require.resolve("playwright-core/package.json"));
	let browsers: BrowsersJson;
	try {
		browsers = JSON.parse(
			await readFile(join(packageRoot, "browsers.json"), "utf8"),
		) as BrowsersJson;
	} catch {
		return null;
	}
	const ffmpeg = browsers.browsers.find((entry) => entry.name === "ffmpeg");
	if (ffmpeg === undefined) {
		return null;
	}
	const path = ffmpegExecutablePath({
		platform: process.platform,
		env: process.env,
		homeDir: homedir(),
		cwd: process.cwd(),
		packageRoot,
		revision: ffmpeg.revision,
	});
	try {
		await access(path);
		return path;
	} catch {
		return null;
	}
}

// --- constant-rate slot accounting ---

/**
 * Decides how many constant-rate frame slots are due at a point in time.
 * Slots are counted from the first frame's arrival, so rounding never
 * accumulates across gaps.
 */
export class FrameClock {
	readonly #fps: number;
	#startMs: number | null = null;
	#written = 0;

	constructor(fps: number) {
		this.#fps = fps;
	}

	/** Frames written so far. */
	get written(): number {
		return this.#written;
	}

	/**
	 * Slots due at `nowMs` that are not written yet. The first call starts
	 * the clock and returns 0.
	 */
	due(nowMs: number): number {
		if (this.#startMs === null) {
			this.#startMs = nowMs;
			return 0;
		}
		const target = Math.round(((nowMs - this.#startMs) * this.#fps) / 1000);
		return Math.max(0, target - this.#written);
	}

	/** Records `count` more written frames. */
	advance(count: number): void {
		this.#written += count;
	}
}

// --- ffmpeg process ---

export interface ScreencastOptions {
	readonly fps: number;
	readonly width: number;
	readonly height: number;
	/** Where the recording is written while in progress. */
	readonly tempDir: string;
	readonly ffmpegPath: string;
}

/** ffmpeg arguments: Playwright's proven VP8 settings at the given rate. */
export function ffmpegArgs(
	options: ScreencastOptions,
	output: string,
): string[] {
	const { fps, width, height } = options;
	const bitrateKbps = Math.max(1000, Math.round((fps / 25) * 1000));
	return [
		...["-loglevel", "error"],
		...["-f", "image2pipe", "-framerate", String(fps), "-c:v", "mjpeg"],
		...["-i", "pipe:0"],
		...["-y", "-an", "-r", String(fps)],
		...["-c:v", "vp8", "-qmin", "0", "-qmax", "50", "-crf", "8"],
		...["-deadline", "realtime", "-speed", "8"],
		...["-b:v", `${bitrateKbps}k`, "-threads", "1"],
		...["-vf", `pad=${width}:${height}:0:0:gray,crop=${width}:${height}:0:0`],
		output,
	];
}

interface ExitStatus {
	readonly code: number | null;
	readonly signal: NodeJS.Signals | null;
}

/** Resolves with the result, or null when the deadline passes first. */
async function withinMs<T>(
	promise: Promise<T>,
	timeoutMs: number,
): Promise<T | null> {
	let timer: NodeJS.Timeout | undefined;
	try {
		return await Promise.race([
			promise,
			new Promise<null>((resolveNull) => {
				timer = setTimeout(() => resolveNull(null), timeoutMs);
			}),
		]);
	} finally {
		clearTimeout(timer);
	}
}

interface ScreencastFrameEvent {
	readonly data: string;
	readonly sessionId: number;
}

/**
 * One recording of one page. `start` opens the screencast and ffmpeg;
 * `stop` finalizes the WebM and moves it into place; `abort` discards it.
 */
export class ScreencastRecorder {
	readonly #session: CDPSession;
	readonly #process: ChildProcess;
	readonly #stdin: NodeJS.WritableStream & { writableNeedDrain: boolean };
	readonly #exit: Promise<ExitStatus>;
	readonly #clock: FrameClock;
	readonly #output: string;
	readonly #onFrame: (event: ScreencastFrameEvent) => void;
	#stderrTail = "";
	#held: Buffer | null = null;
	#stopped = false;

	private constructor(
		session: CDPSession,
		process: ChildProcess,
		options: ScreencastOptions,
		output: string,
	) {
		this.#session = session;
		this.#process = process;
		const stdin = process.stdin;
		if (stdin === null) {
			throw new ShimError("internal", "ffmpeg has no stdin pipe");
		}
		this.#stdin = stdin;
		this.#clock = new FrameClock(options.fps);
		this.#output = output;
		this.#exit = new Promise<ExitStatus>((resolveExit) => {
			process.once("exit", (code, signal) => resolveExit({ code, signal }));
		});
		// A crashed ffmpeg closes the pipe; the exit status reports it.
		stdin.on("error", () => {});
		stdin.on("drain", () => this.#flush(performance.now()));
		process.stderr?.on("data", (chunk: Buffer) => {
			this.#stderrTail = (this.#stderrTail + chunk.toString()).slice(
				-stderrTailLimit,
			);
		});
		this.#onFrame = (event) => {
			// Acknowledge first so Chrome keeps producing frames.
			void session
				.send("Page.screencastFrameAck", { sessionId: event.sessionId })
				.catch(() => {
					// The page may be closing; the recording still finalizes.
				});
			this.#receive(Buffer.from(event.data, "base64"));
		};
		session.on("Page.screencastFrame", this.#onFrame);
	}

	static async start(
		page: Page,
		options: ScreencastOptions,
	): Promise<ScreencastRecorder> {
		await mkdir(options.tempDir, { recursive: true });
		const output = join(
			options.tempDir,
			`screencast-${process.pid}-${Date.now()}.webm`,
		);
		const session = await page.context().newCDPSession(page);
		const child = spawn(options.ffmpegPath, ffmpegArgs(options, output), {
			stdio: ["pipe", "ignore", "pipe"],
		});
		const recorder = new ScreencastRecorder(session, child, options, output);
		try {
			await session.send("Page.startScreencast", {
				format: "jpeg",
				quality: screencastQuality,
				maxWidth: options.width,
				maxHeight: options.height,
				everyNthFrame: 1,
			});
		} catch (error) {
			await recorder.abort();
			throw error;
		}
		return recorder;
	}

	#receive(frame: Buffer): void {
		if (this.#stopped) {
			return;
		}
		this.#flush(performance.now());
		this.#held = frame;
	}

	/** Writes the held frame into every slot due at `nowMs`. */
	#flush(nowMs: number): void {
		const held = this.#held;
		const due = this.#clock.due(nowMs);
		if (held === null || this.#stdin.writableNeedDrain) {
			return;
		}
		for (let index = 0; index < due; index += 1) {
			const accepted = this.#stdin.write(held);
			this.#clock.advance(1);
			if (!accepted) {
				// The rest of the slots are written on `drain`.
				return;
			}
		}
	}

	async #stopScreencast(): Promise<void> {
		this.#stopped = true;
		this.#session.off("Page.screencastFrame", this.#onFrame);
		await this.#session.send("Page.stopScreencast").catch(() => {
			// The page is already gone.
		});
		await this.#session.detach().catch(() => {
			// Detaching a closed session is harmless.
		});
	}

	/**
	 * Finalizes the recording at `finalPath`. Throws an `internal` error when
	 * ffmpeg fails, stalls, or received no frames.
	 */
	async stop(finalPath: string): Promise<void> {
		await this.#stopScreencast();
		const held = this.#held;
		if (held === null) {
			await this.abort();
			throw new ShimError(
				"internal",
				"video recording failed: the page produced no frames",
			);
		}
		// Hold the last frame up to now, and never end on an empty stream.
		const remaining = this.#clock.due(performance.now());
		const copies =
			this.#clock.written === 0 ? Math.max(1, remaining) : remaining;
		for (let index = 0; index < copies; index += 1) {
			this.#stdin.write(held);
		}
		this.#clock.advance(copies);
		this.#stdin.end();
		const status = await withinMs(this.#exit, ffmpegStopTimeoutMs);
		if (status === null) {
			this.#process.kill("SIGKILL");
			await withinMs(this.#exit, ffmpegAbortTimeoutMs);
			await this.#discard();
			throw new ShimError(
				"internal",
				`video recording failed: ffmpeg did not finish within ${ffmpegStopTimeoutMs}ms`,
			);
		}
		if (status.code !== 0) {
			await this.#discard();
			const detail = this.#stderrTail.trim();
			throw new ShimError(
				"internal",
				`video recording failed: ffmpeg exited with ${
					status.code === null
						? `signal ${status.signal}`
						: `status ${status.code}`
				}${detail === "" ? "" : `: ${detail}`}`,
			);
		}
		await mkdir(dirname(finalPath), { recursive: true });
		await rename(this.#output, finalPath);
	}

	/** Stops recording and discards the output. Never throws. */
	async abort(): Promise<void> {
		await this.#stopScreencast();
		if (this.#process.exitCode === null && this.#process.signalCode === null) {
			this.#process.kill("SIGKILL");
			await withinMs(this.#exit, ffmpegAbortTimeoutMs);
		}
		await this.#discard();
	}

	async #discard(): Promise<void> {
		await rm(this.#output, { force: true }).catch(() => {
			// A missing partial file needs no cleanup.
		});
	}
}
