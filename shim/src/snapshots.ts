// SNAPSHOT execution (protocol section 4, SPEC section 7).
//
// A shim-owned poll loop captures full-page or element frames until two
// consecutive captures are byte-identical, compares the settled frame against the
// baseline with Playwright's image comparator (identical dimensions;
// configurable pixel threshold and difference allowance), and keeps
// recapturing and recomparing on mismatch until the deadline.

import { mkdir, readFile, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname } from "node:path";
import type { Locator, Page } from "@playwright/test";
import { describeLocator, type FrameOwner } from "./locators.js";
import type { SnapshotComparison } from "./protocol.js";
import { assertNever, ShimError } from "./protocol.js";
import {
	Deadline,
	failOnMultipleMatches,
	isStrictModeViolation,
	isTimeoutError,
	shortErrorMessage,
	sleep,
	strictnessError,
} from "./step-util.js";

interface ComparatorResult {
	readonly errorMessage: string;
	readonly diff?: Buffer;
}

type Comparator = (
	actual: Buffer,
	expected: Buffer,
	options?: {
		readonly threshold?: number;
		readonly maxDiffPixels?: number;
		readonly maxDiffPixelRatio?: number;
	},
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

const interFrameDelayMs = 100;

export interface SnapshotMask {
	readonly locator: Locator;
	readonly frames: readonly FrameOwner[];
}

interface PageCapture {
	readonly type: "page";
}

/** The visible part of the page, for JUDGE (SPEC 9.8). */
interface ViewportCapture {
	readonly type: "viewport";
}

interface ElementCapture {
	readonly type: "element";
	/** Lazy, so every capture finds a replaced element again. */
	readonly locator: Locator;
	readonly description: string;
	readonly frames: readonly FrameOwner[];
}

/** What one SNAPSHOT captures: the full page or one element (SPEC 7). */
export type SnapshotCapture = PageCapture | ElementCapture;

/** What JUDGE shows the model: the viewport or one element (SPEC 9.8). */
export type JudgeCapture = ViewportCapture | ElementCapture;

export interface SnapshotParams extends SnapshotComparison {
	readonly capture: SnapshotCapture;
	readonly baselinePath: string;
	readonly actualPath: string;
	readonly diffPath: string;
	readonly update: boolean;
	readonly masks: readonly SnapshotMask[];
}

export interface SnapshotResult {
	readonly updated?: boolean;
}

async function failOnAmbiguousFrames(
	frames: readonly FrameOwner[],
): Promise<void> {
	for (const frame of frames) {
		await failOnMultipleMatches(frame.locator, describeLocator(frame.segments));
	}
}

/**
 * Playwright 1.62.1 resolves the element once per `locator.screenshot()`
 * and then captures that handle. These errors mean the element changed
 * during the capture, so resolving it again can succeed.
 */
const transientCaptureErrors = [
	"Element is not attached to the DOM",
	"Node has 0 width",
	"Node has 0 height",
	"Node is either not visible or not an HTMLElement",
] as const;

function isTransientCaptureError(error: unknown): boolean {
	return (
		error instanceof Error &&
		transientCaptureErrors.some((text) => error.message.includes(text))
	);
}

async function captureElement(
	target: ElementCapture,
	deadline: Deadline,
	mask: Locator[],
): Promise<Buffer> {
	for (;;) {
		await failOnAmbiguousFrames(target.frames);
		await failOnMultipleMatches(target.locator, target.description);
		try {
			return await target.locator.screenshot({
				mask,
				timeout: deadline.remainingMs(),
			});
		} catch (error) {
			if (isStrictModeViolation(error)) {
				throw await strictnessError(
					target.locator,
					target.description,
					await target.locator.count(),
				);
			}
			if (!isTransientCaptureError(error)) {
				throw error;
			}
			if (deadline.expired()) {
				throw new ShimError(
					"timeout",
					`snapshot target ${target.description} could not be captured: ${shortErrorMessage(error)}`,
				);
			}
			await sleep(Math.min(interFrameDelayMs, deadline.remainingMs()));
		}
	}
}

async function captureFrame(
	page: Page,
	capture: SnapshotCapture | JudgeCapture,
	deadline: Deadline,
	masks: readonly SnapshotMask[],
): Promise<Buffer> {
	// Screenshot masks allow many element matches, but frame owners must
	// remain unambiguous. Check each capture because the DOM can change.
	for (const mask of masks) {
		await failOnAmbiguousFrames(mask.frames);
	}
	const maskLocators = masks.map((mask) => mask.locator);
	switch (capture.type) {
		case "page":
			return page.screenshot({
				fullPage: true,
				mask: maskLocators,
				timeout: deadline.remainingMs(),
			});
		case "viewport":
			return page.screenshot({ timeout: deadline.remainingMs() });
		case "element":
			return captureElement(capture, deadline, maskLocators);
		default:
			return assertNever(capture);
	}
}

function isCaptureTimeout(error: unknown): boolean {
	return (
		isTimeoutError(error) ||
		(error instanceof ShimError && error.kind === "timeout")
	);
}

/**
 * A capture timeout cannot tell a target that is gone from a deadline that
 * ran out while capturing a present one. Check once without waiting.
 */
async function isTargetStillVisible(target: ElementCapture): Promise<boolean> {
	try {
		await failOnAmbiguousFrames(target.frames);
		return await target.locator.isVisible();
	} catch (error) {
		if (error instanceof ShimError) {
			throw error;
		}
		if (isStrictModeViolation(error)) {
			throw await strictnessError(
				target.locator,
				target.description,
				await target.locator.count(),
			);
		}
		return false;
	}
}

interface SettledFrame {
	readonly frame: Buffer;
	readonly settled: boolean;
}

/** Captures until two consecutive frames are byte-identical. */
async function settleFrame(
	page: Page,
	capture: SnapshotCapture | JudgeCapture,
	deadline: Deadline,
	masks: readonly SnapshotMask[],
): Promise<SettledFrame> {
	let previous = await captureFrame(page, capture, deadline, masks);
	for (;;) {
		if (deadline.expired()) {
			return { frame: previous, settled: false };
		}
		await sleep(Math.min(interFrameDelayMs, deadline.remainingMs()));
		const current = await captureFrame(page, capture, deadline, masks);
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

/** The pinned comparator enforces equal dimensions even at a 100% allowance. */
export function compareSnapshot(
	actual: Buffer,
	expected: Buffer,
	settings: SnapshotComparison,
): ComparatorResult | null {
	return comparePng(actual, expected, {
		threshold: settings.pixelThreshold,
		...(settings.maxDiff.type === "pixels"
			? { maxDiffPixels: settings.maxDiff.value }
			: { maxDiffPixelRatio: settings.maxDiff.value / 100 }),
	});
}

/**
 * A PNG for JUDGE (SPEC 9.8): frames until two in a row are identical,
 * or the last frame when `timeoutMs` runs out first.
 */
export async function settledScreenshot(
	page: Page,
	capture: JudgeCapture,
	timeoutMs: number,
): Promise<Buffer> {
	const settled = await settleFrame(page, capture, new Deadline(timeoutMs), []);
	return settled.frame;
}

export async function runSnapshot(
	page: Page,
	params: SnapshotParams,
	timeoutMs: number,
): Promise<SnapshotResult> {
	const deadline = new Deadline(timeoutMs);

	if (params.update) {
		const settled = await settleFrame(
			page,
			params.capture,
			deadline,
			params.masks,
		);
		if (!settled.settled) {
			const subject =
				params.capture.type === "element"
					? `snapshot target ${params.capture.description}`
					: "page";
			throw new ShimError(
				"timeout",
				`${subject} did not produce a stable frame within ${String(timeoutMs)}ms`,
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
		let attempt: SettledFrame;
		try {
			attempt = await settleFrame(page, params.capture, deadline, params.masks);
		} catch (error) {
			// A capture near the deadline can outlive its sliver of budget
			// and throw Playwright's timeout. The comparison already has a
			// frame to report; failing on it keeps the mismatch artifacts.
			if (!isCaptureTimeout(error) || lastFrame === null) {
				throw error;
			}
			// An element that is gone must not pass, or fail as a mismatch,
			// on pixels from before it disappeared.
			if (
				params.capture.type === "element" &&
				!(await isTargetStillVisible(params.capture))
			) {
				throw new ShimError(
					"timeout",
					`snapshot target ${params.capture.description} was missing or hidden when the step timed out`,
				);
			}
			break;
		}
		const { frame, settled } = attempt;
		const mismatch = compareSnapshot(frame, baseline, params);
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
