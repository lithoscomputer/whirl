// Raw reads for checks and captures that Rust evaluates (protocol 4.4,
// SPEC section 9.2, ADR evaluate-checks-in-rust). A read makes one attempt
// and never waits for its value to appear; Rust owns the retry loop.

import type { Locator, Page } from "@playwright/test";
import type { EvalClassification } from "./eval-support.js";
import { buildReadEvalExpression } from "./eval-support.js";
import { buildLocator, describeLocator, frameOwners } from "./locators.js";
import type {
	ElementReadSubject,
	JsonValue,
	LocatorSegment,
	ReadResult,
	ReadSubject,
} from "./protocol.js";
import { assertNever, ShimError } from "./protocol.js";
import {
	isStrictModeViolation,
	isTimeoutError,
	normalizeWhitespace,
	shortErrorMessage,
	strictnessError,
} from "./step-util.js";

/**
 * How long one element extraction may take once the element resolved. An
 * element that detaches in that window reads as missing, and Rust reads
 * again on its schedule.
 */
const extractionTimeoutMs = 1_000;

type ReadEvalOutcome = EvalClassification & { readonly string: boolean };

function present(value: JsonValue): ReadResult {
	return { type: "value", value };
}

const noElement: ReadResult = { type: "missing", reason: "no-element" };

/** Runs one read of a page subject. */
export async function runRead(
	page: Page,
	subject: ReadSubject,
	timeoutMs: number,
): Promise<ReadResult> {
	switch (subject.type) {
		case "element":
			return readElement(page, subject, timeoutMs);
		case "count":
			await failOnAmbiguousFrames(page, subject.locator);
			return present(await buildLocator(page, subject.locator).count());
		case "url":
			return present(page.url());
		case "title":
			return present(normalizeWhitespace(await page.title()));
		case "eval":
			return present(await readEval(page, subject.script, timeoutMs));
		default:
			return assertNever(subject);
	}
}

async function readElement(
	page: Page,
	subject: ElementReadSubject,
	timeoutMs: number,
): Promise<ReadResult> {
	await failOnAmbiguousFrames(page, subject.locator);
	const locator = buildLocator(page, subject.locator);
	const description = describeLocator(subject.locator);
	const count = await locator.count();
	if (count === 0) {
		return noElement;
	}
	if (count > 1) {
		throw await strictnessError(locator, description, count);
	}
	const timeout = Math.min(extractionTimeoutMs, timeoutMs);
	try {
		return await extract(locator, subject, timeout);
	} catch (error) {
		if (error instanceof ShimError) {
			throw error;
		}
		if (isTimeoutError(error)) {
			return noElement;
		}
		if (isStrictModeViolation(error)) {
			throw new ShimError("strictness", shortErrorMessage(error));
		}
		throw new ShimError(
			"read",
			`reading ${subject.extract.type} of ${description} failed: ${shortErrorMessage(error)}`,
		);
	}
}

async function extract(
	locator: Locator,
	subject: ElementReadSubject,
	timeout: number,
): Promise<ReadResult> {
	const extraction = subject.extract;
	switch (extraction.type) {
		case "text":
			return present(
				normalizeWhitespace(
					await locator.evaluate(elementText, undefined, { timeout }),
				),
			);
		case "value":
			return present(await locator.inputValue({ timeout }));
		case "attr": {
			const value = await locator.getAttribute(extraction.name, { timeout });
			return value === null
				? { type: "missing", reason: "absent-attribute" }
				: present(value);
		}
		default:
			return assertNever(extraction);
	}
}

/**
 * Fails at once when a `frame:` segment matches more than one iframe
 * (SPEC 6). Playwright would read the first frame silently.
 */
async function failOnAmbiguousFrames(
	page: Page,
	segments: readonly LocatorSegment[],
): Promise<void> {
	for (const frame of frameOwners(page, segments)) {
		const count = await frame.locator.count();
		if (count > 1) {
			throw await strictnessError(
				frame.locator,
				describeLocator(frame.segments),
				count,
			);
		}
	}
}

/**
 * An element's text as Playwright's text engine and `toHaveText` see it:
 * text nodes in order, then any shadow root, skipping SCRIPT, NOSCRIPT,
 * STYLE, and the document head; a submit or button input gives its value.
 * It runs in the page, so it must stay self-contained.
 */
function elementText(root: Node): string {
	const skipped = (node: Node): boolean =>
		node.nodeName === "SCRIPT" ||
		node.nodeName === "NOSCRIPT" ||
		node.nodeName === "STYLE" ||
		document.head?.contains(node) === true;
	const textOf = (node: Node): string => {
		if (skipped(node)) {
			return "";
		}
		if (
			node instanceof HTMLInputElement &&
			(node.type === "submit" || node.type === "button")
		) {
			return node.value;
		}
		let text = "";
		for (
			let child = node.firstChild;
			child !== null;
			child = child.nextSibling
		) {
			if (child.nodeType === Node.TEXT_NODE) {
				text += child.nodeValue ?? "";
			} else if (child.nodeType === Node.ELEMENT_NODE) {
				text += textOf(child);
			}
		}
		if (node instanceof Element && node.shadowRoot !== null) {
			text += textOf(node.shadowRoot);
		}
		return text;
	};
	return textOf(root);
}

async function readEval(
	page: Page,
	script: string,
	timeoutMs: number,
): Promise<JsonValue> {
	let timer: NodeJS.Timeout | undefined;
	const timedOut = new Promise<never>((_resolve, reject) => {
		timer = setTimeout(() => {
			reject(
				new ShimError(
					"eval",
					`script did not settle within ${String(timeoutMs)}ms`,
				),
			);
		}, timeoutMs);
	});
	try {
		const outcome = (await Promise.race([
			page.evaluate(buildReadEvalExpression(script)),
			timedOut,
		])) as ReadEvalOutcome;
		if (!outcome.ok) {
			throw new ShimError("eval-result", `eval result is ${outcome.reason}`);
		}
		return outcome.string
			? outcome.value
			: (JSON.parse(outcome.value) as JsonValue);
	} catch (error) {
		if (error instanceof ShimError) {
			throw error;
		}
		throw new ShimError("eval", shortErrorMessage(error));
	} finally {
		clearTimeout(timer);
	}
}
