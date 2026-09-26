// State check, tab closure, and PAGE execution (protocol 4.2 and 4.3,
// SPEC sections 8, 9.1, 15).
//
// Each compiles to a Playwright web-first assertion. Checks with a subject
// never reach the shim as checks: Rust evaluates them from raw reads (see
// reads.ts and ADR evaluate-checks-in-rust).

import type { Locator, Page } from "@playwright/test";
import { expect } from "@playwright/test";
import { buildLocator, describeLocator } from "./locators.js";
import type { AssertSpec, ElementState, PageExpectation } from "./protocol.js";
import { assertNever, ShimError } from "./protocol.js";
import {
	failOnMultipleMatches,
	isStrictModeViolation,
	shortErrorMessage,
	strictnessError,
} from "./step-util.js";

const actualReadTimeoutMs = 1000;

async function readCount(locator: Locator): Promise<number> {
	try {
		return await locator.count();
	} catch {
		return 0;
	}
}

/** Best-effort current state of an element, for `actual` reporting. */
async function readActual(
	locator: Locator,
	state: ElementState,
): Promise<string> {
	try {
		if ((await locator.count()) === 0) {
			return "no matching element";
		}
		return await readStateActual(locator, state);
	} catch (error) {
		return shortErrorMessage(error);
	}
}

async function readStateActual(
	locator: Locator,
	state: ElementState,
): Promise<string> {
	switch (state) {
		case "visible":
		case "hidden":
			return (await locator.isVisible()) ? "visible" : "hidden";
		case "enabled":
		case "disabled":
			return (await locator.isEnabled({ timeout: actualReadTimeoutMs }))
				? "enabled"
				: "disabled";
		case "checked":
		case "unchecked":
			return (await locator.isChecked({ timeout: actualReadTimeoutMs }))
				? "checked"
				: "unchecked";
		case "focused":
			return (await locator.evaluate(
				(element) => element === element.ownerDocument.activeElement,
			))
				? "focused"
				: "not focused";
		default:
			return assertNever(state);
	}
}

interface WebFirstFailure {
	readonly expected: string;
	readonly actual: () => Promise<string>;
	readonly locator?: Locator;
	readonly description?: string;
}

/**
 * Runs one web-first assertion. A failure at the timeout reports as kind
 * "assert" with expected/actual; a strict-mode violation reports as
 * "strictness" with candidates.
 */
async function webFirst(
	assertion: () => Promise<void>,
	failure: WebFirstFailure,
): Promise<void> {
	try {
		await assertion();
	} catch (error) {
		if (error instanceof ShimError) {
			throw error;
		}
		if (
			isStrictModeViolation(error) &&
			failure.locator !== undefined &&
			failure.description !== undefined
		) {
			throw await strictnessError(
				failure.locator,
				failure.description,
				await readCount(failure.locator),
			);
		}
		let actual = "unavailable";
		try {
			actual = await failure.actual();
		} catch {
			// keep the fallback
		}
		throw new ShimError("assert", shortErrorMessage(error), {
			expected: failure.expected,
			actual,
		});
	}
}

async function assertState(
	locator: Locator,
	state: ElementState,
	timeout: number,
	failure: WebFirstFailure,
): Promise<void> {
	switch (state) {
		case "visible":
			return webFirst(() => expect(locator).toBeVisible({ timeout }), failure);
		case "hidden":
			return webFirst(() => expect(locator).toBeHidden({ timeout }), failure);
		case "enabled":
			return webFirst(() => expect(locator).toBeEnabled({ timeout }), failure);
		case "disabled":
			return webFirst(() => expect(locator).toBeDisabled({ timeout }), failure);
		case "checked":
			return webFirst(
				() => expect(locator).toBeChecked({ checked: true, timeout }),
				failure,
			);
		case "unchecked":
			return webFirst(
				() => expect(locator).toBeChecked({ checked: false, timeout }),
				failure,
			);
		case "focused":
			return webFirst(() => expect(locator).toBeFocused({ timeout }), failure);
		default:
			return assertNever(state);
	}
}

/** Runs one state check (protocol 4.3). */
export async function runAssert(
	page: Page,
	spec: AssertSpec,
	timeoutMs: number,
): Promise<void> {
	const locator = buildLocator(page, spec.subject.locator);
	const description = describeLocator(spec.subject.locator);
	// More than one match fails immediately, for `hidden` as well (SPEC 6.2).
	await failOnMultipleMatches(locator, description);
	const state = spec.check.state;
	return assertState(locator, state, timeoutMs, {
		expected: state,
		actual: () => readActual(locator, state),
		locator,
		description,
	});
}

function describePageExpectation(expectation: PageExpectation): string {
	switch (expectation.kind) {
		case "path":
			return `path == ${expectation.value}`;
		case "pathQuery":
			return `path?query == ${expectation.value}`;
		case "url":
			return `url == ${expectation.value}`;
		case "regex":
			return `url matches /${expectation.source}/${expectation.flags}`;
		default:
			return assertNever(expectation);
	}
}

/** Runs one PAGE line: retried like an assert (protocol 4.2). */
export async function runPage(
	page: Page,
	expectation: PageExpectation,
	timeoutMs: number,
): Promise<void> {
	const failure: WebFirstFailure = {
		expected: describePageExpectation(expectation),
		actual: async () => page.url(),
	};
	switch (expectation.kind) {
		case "path":
			return webFirst(
				() =>
					expect(page).toHaveURL((url) => url.pathname === expectation.value, {
						timeout: timeoutMs,
					}),
				failure,
			);
		case "pathQuery":
			return webFirst(
				() =>
					expect(page).toHaveURL(
						(url) => url.pathname + url.search === expectation.value,
						{ timeout: timeoutMs },
					),
				failure,
			);
		case "url":
			return webFirst(
				() =>
					expect(page).toHaveURL((_url) => page.url() === expectation.value, {
						timeout: timeoutMs,
					}),
				failure,
			);
		case "regex":
			return webFirst(
				() =>
					// Whirl regexes run in Unicode mode (SPEC 3.1).
					expect(page).toHaveURL(
						new RegExp(expectation.source, `${expectation.flags}u`),
						{ timeout: timeoutMs },
					),
				failure,
			);
		default:
			return assertNever(expectation);
	}
}
