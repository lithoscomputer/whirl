// Capture execution (protocol 4.4, SPEC section 10).

import type { Locator, Page } from "@playwright/test";
import type { EvalClassification } from "./eval-support.js";
import { buildCaptureEvalExpression } from "./eval-support.js";
import { buildLocator, describeLocator } from "./locators.js";
import type {
	CaptureFilter,
	CaptureSource,
	ElementExtract,
} from "./protocol.js";
import { assertNever, ShimError } from "./protocol.js";
import {
	Deadline,
	normalizeWhitespace,
	pollInterval,
	sleep,
	strictnessError,
} from "./step-util.js";

/**
 * Waits for the locator to resolve to exactly one element, up to the
 * deadline. More than one match fails immediately with candidates; never
 * resolving is a timeout.
 */
async function waitForSingleElement(
	locator: Locator,
	description: string,
	deadline: Deadline,
): Promise<void> {
	let iteration = 0;
	for (;;) {
		let count = 0;
		try {
			count = await locator.count();
		} catch (error) {
			if (error instanceof ShimError) {
				throw error;
			}
			// Transient page churn; retry below.
		}
		if (count === 1) {
			return;
		}
		if (count > 1) {
			throw await strictnessError(locator, description, count);
		}
		if (deadline.expired()) {
			throw new ShimError(
				"timeout",
				`no element matching ${description} within the step timeout`,
			);
		}
		await sleep(Math.min(pollInterval(iteration), deadline.remainingMs()));
		iteration += 1;
	}
}

async function extractFromElement(
	locator: Locator,
	description: string,
	extract: ElementExtract,
	deadline: Deadline,
): Promise<string> {
	switch (extract.type) {
		case "count":
			// count never waits: the current number of matches, zero included.
			return String(await locator.count());
		case "text": {
			await waitForSingleElement(locator, description, deadline);
			const text = await locator.textContent({
				timeout: deadline.remainingMs(),
			});
			return normalizeWhitespace(text ?? "");
		}
		case "value": {
			await waitForSingleElement(locator, description, deadline);
			try {
				return await locator.inputValue({ timeout: deadline.remainingMs() });
			} catch (error) {
				throw toCaptureError(error, description);
			}
		}
		case "attr": {
			await waitForSingleElement(locator, description, deadline);
			const value = await locator.getAttribute(extract.name, {
				timeout: deadline.remainingMs(),
			});
			if (value === null) {
				// The element resolved; an absent attribute fails immediately.
				throw new ShimError(
					"capture",
					`attribute "${extract.name}" is absent on ${description}`,
				);
			}
			return value;
		}
		default:
			return assertNever(extract);
	}
}

function toCaptureError(error: unknown, description: string): ShimError {
	if (error instanceof ShimError) {
		return error;
	}
	const message = error instanceof Error ? error.message : String(error);
	return new ShimError(
		"capture",
		`extraction from ${description} failed: ${message.split("\n")[0] ?? message}`,
	);
}

async function runEvalCapture(
	page: Page,
	script: string,
	timeoutMs: number,
): Promise<string> {
	const expression = buildCaptureEvalExpression(script);
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
			page.evaluate(expression),
			timedOut,
		])) as EvalClassification;
		if (!outcome.ok) {
			throw new ShimError("eval-result", `EVAL result is ${outcome.reason}`);
		}
		return outcome.value;
	} catch (error) {
		if (error instanceof ShimError) {
			throw error;
		}
		const message = error instanceof Error ? error.message : String(error);
		throw new ShimError("eval", message);
	} finally {
		clearTimeout(timer);
	}
}

export function applyCaptureFilter(
	value: string,
	filter: CaptureFilter,
): string {
	const pattern = new RegExp(filter.source, filter.flags);
	const match = pattern.exec(value);
	if (match === null) {
		throw new ShimError(
			"capture",
			`regex /${filter.source}/${filter.flags} did not match ${JSON.stringify(
				value,
			)}`,
		);
	}
	return match[1] ?? match[0];
}

/** Runs one `[Captures]` line and returns the captured string. */
export async function runCapture(
	page: Page,
	source: CaptureSource,
	filter: CaptureFilter | null,
	timeoutMs: number,
): Promise<string> {
	const extracted = await extractSource(page, source, timeoutMs);
	if (filter === null) {
		return extracted;
	}
	return applyCaptureFilter(extracted, filter);
}

async function extractSource(
	page: Page,
	source: CaptureSource,
	timeoutMs: number,
): Promise<string> {
	switch (source.type) {
		case "element": {
			const locator = buildLocator(page, source.locator);
			const description = describeLocator(source.locator);
			return extractFromElement(
				locator,
				description,
				source.extract,
				new Deadline(timeoutMs),
			);
		}
		case "url":
			return page.url();
		case "title":
			return page.title();
		case "eval":
			return runEvalCapture(page, source.script, timeoutMs);
		default:
			return assertNever(source);
	}
}
