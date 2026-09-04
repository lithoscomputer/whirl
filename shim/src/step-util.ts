// Shared helpers for step execution: deadlines, poll loops, error shaping.

import type { Locator } from "@playwright/test";
import { candidateDescriptions } from "./locators.js";
import type { ErrorKind } from "./protocol.js";
import { ShimError } from "./protocol.js";

export function sleep(ms: number): Promise<void> {
	return new Promise((resolve) => {
		setTimeout(resolve, ms);
	});
}

/** Playwright's web-first assertion polling ladder. */
const pollIntervalsMs = [100, 250, 500, 1000] as const;

export function pollInterval(iteration: number): number {
	const index = Math.min(iteration, pollIntervalsMs.length - 1);
	return pollIntervalsMs[index] ?? 1000;
}

export class Deadline {
	readonly #expiresAt: number;

	constructor(timeoutMs: number) {
		this.#expiresAt = Date.now() + timeoutMs;
	}

	expired(): boolean {
		return Date.now() >= this.#expiresAt;
	}

	/** Remaining budget, clamped to at least 1ms for Playwright calls. */
	remainingMs(): number {
		return Math.max(1, this.#expiresAt - Date.now());
	}
}

export interface PollAttempt {
	readonly pass: boolean;
	readonly actual: string;
}

export interface PollFailure {
	readonly kind: ErrorKind;
	readonly message: string;
	readonly expected?: string;
}

/**
 * Shim-owned retry loop with Playwright's polling ladder. `attempt` runs
 * until it passes or `timeoutMs` expires; a thrown ShimError (strictness,
 * cancellation) aborts immediately. Other exceptions count as a failing
 * attempt, because a page mid-navigation throws transient errors.
 */
export async function pollUntilPass(
	timeoutMs: number,
	attempt: () => Promise<PollAttempt>,
	failure: PollFailure,
): Promise<void> {
	const deadline = new Deadline(timeoutMs);
	let last: PollAttempt | null = null;
	let iteration = 0;
	for (;;) {
		try {
			last = await attempt();
			if (last.pass) {
				return;
			}
		} catch (error) {
			if (error instanceof ShimError) {
				throw error;
			}
			last = { pass: false, actual: shortErrorMessage(error) };
		}
		if (deadline.expired()) {
			break;
		}
		await sleep(Math.min(pollInterval(iteration), deadline.remainingMs()));
		iteration += 1;
	}
	throw new ShimError(failure.kind, failure.message, {
		...(failure.expected === undefined ? {} : { expected: failure.expected }),
		...(last === null ? {} : { actual: last.actual }),
	});
}

/** First line of an error message, without Playwright's call log. */
export function shortErrorMessage(error: unknown): string {
	const message = error instanceof Error ? error.message : String(error);
	const callLogIndex = message.indexOf("Call log:");
	const trimmed =
		callLogIndex === -1 ? message : message.slice(0, callLogIndex);
	return trimmed.trim().replace(/\n+/g, " ");
}

export function isTimeoutError(error: unknown): boolean {
	return error instanceof Error && error.name === "TimeoutError";
}

export function isStrictModeViolation(error: unknown): boolean {
	return (
		error instanceof Error && error.message.includes("strict mode violation")
	);
}

export function isTargetClosedError(error: unknown): boolean {
	return (
		error instanceof Error &&
		(error.message.includes("has been closed") ||
			error.name === "TargetClosedError")
	);
}

/** Immediate strictness failure with the candidate list (SPEC 6.2). */
export async function strictnessError(
	locator: Locator,
	description: string,
	matchCount: number,
): Promise<ShimError> {
	const candidates = await candidateDescriptions(locator);
	return new ShimError(
		"strictness",
		`locator ${description} matched ${String(matchCount)} elements`,
		{ candidates },
	);
}

/**
 * Fails immediately when the locator currently resolves to more than one
 * element. Zero matches is fine here; the caller's own wait handles that.
 */
export async function failOnMultipleMatches(
	locator: Locator,
	description: string,
): Promise<void> {
	const count = await locator.count();
	if (count > 1) {
		throw await strictnessError(locator, description, count);
	}
}

/** Playwright-style whitespace normalization for text comparisons. */
export function normalizeWhitespace(text: string): string {
	return text
		.replace(/[\u200b\u00ad]/g, "")
		.replace(/\s+/g, " ")
		.trim();
}

export function escapeRegExp(text: string): string {
	return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Keep Playwright's actionability log: it explains what prevented the action. */
export function actionErrorMessage(error: unknown): string {
	return (error instanceof Error ? error.message : String(error)).trim();
}
