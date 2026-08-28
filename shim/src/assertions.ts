// Assert and PAGE execution (protocol 4.2 and 4.3, SPEC sections 8, 9, 15).
//
// Every check compiles to a Playwright web-first assertion where a usable
// one exists; the rest run as shim-owned poll loops with the same timeout.

import type { Locator, Page } from "@playwright/test";
import { expect } from "@playwright/test";
import { buildLocator, describeLocator } from "./locators.js";
import type {
	AssertCheck,
	AssertSpec,
	CountOp,
	ElementState,
	PageExpectation,
	StringOp,
} from "./protocol.js";
import { assertNever, ShimError } from "./protocol.js";
import {
	escapeRegExp,
	failOnMultipleMatches,
	isStrictModeViolation,
	normalizeWhitespace,
	pollUntilPass,
	shortErrorMessage,
	strictnessError,
} from "./step-util.js";

function stringOpRegExp(op: StringOp): RegExp {
	switch (op.op) {
		case "matches":
			return new RegExp(op.source, op.flags);
		case "contains":
			return new RegExp(escapeRegExp(op.value));
		case "==":
		case "!=":
			throw new ShimError("internal", `no regex form for op ${op.op}`);
		default:
			return assertNever(op);
	}
}

function describeStringOp(op: StringOp): string {
	switch (op.op) {
		case "matches":
			return `matches /${op.source}/${op.flags}`;
		default:
			return `${op.op} ${JSON.stringify(op.value)}`;
	}
}

const actualReadTimeoutMs = 1000;

async function readCount(locator: Locator): Promise<number> {
	try {
		return await locator.count();
	} catch {
		return 0;
	}
}

/** Best-effort current value of a check subject, for `actual` reporting. */
async function readElementActual(
	locator: Locator,
	check: AssertCheck,
): Promise<string> {
	try {
		const count = await locator.count();
		if (count === 0) {
			return "no matching element";
		}
		const options = { timeout: actualReadTimeoutMs };
		switch (check.type) {
			case "state":
				return await readStateActual(locator, check.state);
			case "text":
				return normalizeWhitespace((await locator.textContent(options)) ?? "");
			case "value":
				return await locator.inputValue(options);
			case "attr": {
				const value = await locator.getAttribute(check.name, options);
				return value === null ? "<absent>" : value;
			}
			case "count":
				return String(count);
			default:
				return assertNever(check);
		}
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

function compareCount(actual: number, op: CountOp, value: number): boolean {
	switch (op) {
		case "==":
			return actual === value;
		case "!=":
			return actual !== value;
		case "<":
			return actual < value;
		case "<=":
			return actual <= value;
		case ">":
			return actual > value;
		case ">=":
			return actual >= value;
		default:
			return assertNever(op);
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

async function assertText(
	locator: Locator,
	op: StringOp,
	timeout: number,
	failure: WebFirstFailure,
): Promise<void> {
	switch (op.op) {
		case "==":
			return webFirst(
				() => expect(locator).toHaveText(op.value, { timeout }),
				failure,
			);
		case "!=":
			return webFirst(
				() => expect(locator).not.toHaveText(op.value, { timeout }),
				failure,
			);
		case "contains":
			return webFirst(
				() => expect(locator).toContainText(op.value, { timeout }),
				failure,
			);
		case "matches": {
			// No web-first form: `toHaveText(RegExp)` matches the raw
			// text, but the subject of a `text` check is the normalized
			// text content (SPEC 9.2), so a shim-owned poll loop applies
			// the regex to the normalized text with the same timeout.
			const description = failure.description ?? "";
			return pollTextMatches(locator, stringOpRegExp(op), timeout, {
				expected: failure.expected,
				description,
			});
		}
		default:
			return assertNever(op);
	}
}

async function pollTextMatches(
	locator: Locator,
	pattern: RegExp,
	timeoutMs: number,
	info: { readonly expected: string; readonly description: string },
): Promise<void> {
	await pollUntilPass(
		timeoutMs,
		async () => {
			const count = await locator.count();
			if (count > 1) {
				throw await strictnessError(locator, info.description, count);
			}
			if (count === 0) {
				return { pass: false, actual: "no matching element" };
			}
			const text = normalizeWhitespace((await locator.textContent()) ?? "");
			return { pass: pattern.test(text), actual: text };
		},
		{
			kind: "assert",
			message: `text check did not pass within ${String(timeoutMs)}ms`,
			expected: info.expected,
		},
	);
}

async function assertValue(
	locator: Locator,
	op: StringOp,
	timeout: number,
	failure: WebFirstFailure,
): Promise<void> {
	switch (op.op) {
		case "==":
			return webFirst(
				() => expect(locator).toHaveValue(op.value, { timeout }),
				failure,
			);
		case "!=":
			return webFirst(
				() => expect(locator).not.toHaveValue(op.value, { timeout }),
				failure,
			);
		case "contains":
		case "matches":
			return webFirst(
				() => expect(locator).toHaveValue(stringOpRegExp(op), { timeout }),
				failure,
			);
		default:
			return assertNever(op);
	}
}

async function assertAttr(
	locator: Locator,
	name: string,
	op: StringOp,
	timeoutMs: number,
	failure: WebFirstFailure,
): Promise<void> {
	switch (op.op) {
		case "==":
			return webFirst(
				() =>
					expect(locator).toHaveAttribute(name, op.value, {
						timeout: timeoutMs,
					}),
				failure,
			);
		case "contains":
		case "matches":
			return webFirst(
				() =>
					expect(locator).toHaveAttribute(name, stringOpRegExp(op), {
						timeout: timeoutMs,
					}),
				failure,
			);
		case "!=": {
			// No web-first form: `!=` must also pass when the attribute is
			// absent, so a shim-owned poll loop runs with the same timeout.
			const description = failure.description ?? "";
			return pollAttrNotEquals(locator, name, op.value, timeoutMs, {
				expected: failure.expected,
				description,
			});
		}
		default:
			return assertNever(op);
	}
}

async function pollAttrNotEquals(
	locator: Locator,
	name: string,
	value: string,
	timeoutMs: number,
	info: { readonly expected: string; readonly description: string },
): Promise<void> {
	await pollUntilPass(
		timeoutMs,
		async () => {
			const count = await locator.count();
			if (count > 1) {
				throw await strictnessError(locator, info.description, count);
			}
			if (count === 0) {
				return { pass: false, actual: "no matching element" };
			}
			const actual = await locator.getAttribute(name);
			return {
				pass: actual !== value,
				actual: actual === null ? "<absent>" : actual,
			};
		},
		{
			kind: "assert",
			message: `attribute check did not pass within ${String(timeoutMs)}ms`,
			expected: info.expected,
		},
	);
}

async function assertLocatorCheck(
	locator: Locator,
	description: string,
	check: AssertCheck,
	timeoutMs: number,
): Promise<void> {
	// More than one match fails immediately, for `hidden` as well
	// (SPEC 6.2); `count` alone accepts any number of matches.
	if (check.type !== "count") {
		await failOnMultipleMatches(locator, description);
	}
	const failure: WebFirstFailure = {
		expected: describeCheck(check),
		actual: () => readElementActual(locator, check),
		locator,
		description,
	};
	switch (check.type) {
		case "state":
			return assertState(locator, check.state, timeoutMs, failure);
		case "text":
			return assertText(locator, check.op, timeoutMs, failure);
		case "value":
			return assertValue(locator, check.op, timeoutMs, failure);
		case "attr":
			return assertAttr(locator, check.name, check.op, timeoutMs, failure);
		case "count": {
			if (check.op === "==") {
				return webFirst(
					() =>
						expect(locator).toHaveCount(check.value, { timeout: timeoutMs }),
					failure,
				);
			}
			// Count comparators other than == have no web-first assertion.
			return pollUntilPass(
				timeoutMs,
				async () => {
					const count = await locator.count();
					return {
						pass: compareCount(count, check.op, check.value),
						actual: String(count),
					};
				},
				{
					kind: "assert",
					message: `count check did not pass within ${String(timeoutMs)}ms`,
					expected: describeCheck(check),
				},
			);
		}
		default:
			return assertNever(check);
	}
}

function describeCheck(check: AssertCheck): string {
	switch (check.type) {
		case "state":
			return check.state;
		case "text":
			return `text ${describeStringOp(check.op)}`;
		case "value":
			return `value ${describeStringOp(check.op)}`;
		case "attr":
			return `attr:${check.name} ${describeStringOp(check.op)}`;
		case "count":
			return `count ${check.op} ${String(check.value)}`;
		default:
			return assertNever(check);
	}
}

async function assertPageSubject(
	page: Page,
	subject: "url" | "title",
	check: AssertCheck,
	timeoutMs: number,
): Promise<void> {
	if (check.type !== "text") {
		throw new ShimError(
			"internal",
			`${subject} assert requires a string check`,
		);
	}
	const op = check.op;
	const failure: WebFirstFailure = {
		expected: `${subject} ${describeStringOp(op)}`,
		actual: async () => (subject === "url" ? page.url() : await page.title()),
	};
	if (subject === "url") {
		switch (op.op) {
			case "==":
				return webFirst(
					() =>
						expect(page).toHaveURL((_url) => page.url() === op.value, {
							timeout: timeoutMs,
						}),
					failure,
				);
			case "!=":
				return webFirst(
					() =>
						expect(page).not.toHaveURL((_url) => page.url() === op.value, {
							timeout: timeoutMs,
						}),
					failure,
				);
			case "contains":
				return webFirst(
					() =>
						expect(page).toHaveURL((_url) => page.url().includes(op.value), {
							timeout: timeoutMs,
						}),
					failure,
				);
			case "matches":
				return webFirst(
					() =>
						expect(page).toHaveURL(stringOpRegExp(op), { timeout: timeoutMs }),
					failure,
				);
			default:
				return assertNever(op);
		}
	}
	switch (op.op) {
		case "==":
			return webFirst(
				() => expect(page).toHaveTitle(op.value, { timeout: timeoutMs }),
				failure,
			);
		case "!=":
			return webFirst(
				() => expect(page).not.toHaveTitle(op.value, { timeout: timeoutMs }),
				failure,
			);
		case "contains":
		case "matches":
			return webFirst(
				() =>
					expect(page).toHaveTitle(stringOpRegExp(op), { timeout: timeoutMs }),
				failure,
			);
		default:
			return assertNever(op);
	}
}

/** Runs one `[Asserts]` line. */
export async function runAssert(
	page: Page,
	spec: AssertSpec,
	timeoutMs: number,
): Promise<void> {
	const subject = spec.subject;
	switch (subject.type) {
		case "locator": {
			const locator = buildLocator(page, subject.locator);
			const description = describeLocator(subject.locator);
			return assertLocatorCheck(locator, description, spec.check, timeoutMs);
		}
		case "url":
			return assertPageSubject(page, "url", spec.check, timeoutMs);
		case "title":
			return assertPageSubject(page, "title", spec.check, timeoutMs);
		default:
			return assertNever(subject);
	}
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
					expect(page).toHaveURL(
						new RegExp(expectation.source, expectation.flags),
						{ timeout: timeoutMs },
					),
				failure,
			);
		default:
			return assertNever(expectation);
	}
}
