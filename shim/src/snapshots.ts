// SNAPSHOT execution (protocol section 4, SPEC section 7).
//
// A shim-owned poll loop captures full-page frames until two consecutive
// captures are byte-identical, compares the settled frame against the
// baseline with Playwright's image comparator (identical dimensions;
// per-pixel color distance threshold 0.2 on a 0-1 scale), and keeps
// recapturing and recomparing on mismatch until the deadline.

import { mkdir, readFile, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname } from "node:path";
import type { Page } from "@playwright/test";
import { ShimError } from "./protocol.js";
import { Deadline, sleep } from "./step-util.js";

interface ComparatorResult {
	readonly errorMessage: string;
	readonly diff?: Buffer;
}

type Comparator = (
	actual: Buffer,
	expected: Buffer,
	options?: { readonly threshold?: number },
) => ComparatorResult | null;

interface CoreBundle {
	readonly utils: {
		readonly getComparator: (mimeType: string) => Comparator;
	};
}

const require = createRequire(import.meta.url);

// Playwright's own comparator lives in playwright-core's bundled internals.
// The Playwright version is pinned (1.62.1), so this private import is
// stable for the life of the pin.
const coreBundle = require("playwright-core/lib/coreBundle") as CoreBundle;
const comparePng: Comparator = coreBundle.utils.getComparator("image/png");

const defaultThreshold = 0.2;
const interFrameDelayMs = 100;

export interface SnapshotParams {
	readonly baselinePath: string;
	readonly actualPath: string;
	readonly diffPath: string;
	readonly update: boolean;
}

export interface SnapshotResult {
	readonly updated?: boolean;
}

async function captureFrame(page: Page, deadline: Deadline): Promise<Buffer> {
	return page.screenshot({
		fullPage: true,
		timeout: deadline.remainingMs(),
	});
}

interface SettledFrame {
	readonly frame: Buffer;
	readonly settled: boolean;
}

/** Captures until two consecutive frames are byte-identical. */
async function settleFrame(
	page: Page,
	deadline: Deadline,
): Promise<SettledFrame> {
	let previous = await captureFrame(page, deadline);
	for (;;) {
		if (deadline.expired()) {
			return { frame: previous, settled: false };
		}
		await sleep(Math.min(interFrameDelayMs, deadline.remainingMs()));
		const current = await captureFrame(page, deadline);
		if (current.equals(previous)) {
			return { frame: current, settled: true };
		}
		previous = current;
	}
}

async function writeImage(path: string, data: Buffer): Promise<void> {
	await mkdir(dirname(path), { recursive: true });
	await writeFile(path, data);
}

async function readBaseline(path: string): Promise<Buffer | null> {
	try {
		return await readFile(path);
	} catch (error) {
		if (
			error instanceof Error &&
			"code" in error &&
			(error as NodeJS.ErrnoException).code === "ENOENT"
		) {
			return null;
		}
		throw error;
	}
}

export async function runSnapshot(
	page: Page,
	params: SnapshotParams,
	timeoutMs: number,
): Promise<SnapshotResult> {
	const deadline = new Deadline(timeoutMs);

	if (params.update) {
		const settled = await settleFrame(page, deadline);
		if (!settled.settled) {
			throw new ShimError(
				"timeout",
				`page did not produce a stable frame within ${String(timeoutMs)}ms`,
			);
		}
		await writeImage(params.baselinePath, settled.frame);
		return { updated: true };
	}

	const baseline = await readBaseline(params.baselinePath);
	if (baseline === null) {
		throw new ShimError(
			"snapshot-missing-baseline",
			`no baseline image at ${params.baselinePath}`,
		);
	}

	let lastFrame: Buffer | null = null;
	let lastMismatch: ComparatorResult | null = null;
	for (;;) {
		const { frame, settled } = await settleFrame(page, deadline);
		const mismatch = comparePng(frame, baseline, {
			threshold: defaultThreshold,
		});
		if (mismatch === null && settled) {
			return {};
		}
		lastFrame = frame;
		lastMismatch = mismatch;
		if (deadline.expired()) {
			break;
		}
		await sleep(Math.min(interFrameDelayMs, deadline.remainingMs()));
	}

	if (lastMismatch === null) {
		// The final frame matched but the page never settled twice in a row;
		// accept the match rather than fail a visually identical page.
		return {};
	}
	if (lastFrame !== null) {
		await writeImage(params.actualPath, lastFrame);
		if (lastMismatch.diff !== undefined) {
			await writeImage(params.diffPath, lastMismatch.diff);
		}
	}
	throw new ShimError("snapshot-mismatch", lastMismatch.errorMessage);
}
