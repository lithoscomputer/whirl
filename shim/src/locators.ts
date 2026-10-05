// Locator building from protocol JSON segments (protocol 4.1, SPEC 6.1).

import type { Locator, Page } from "@playwright/test";
import type { LocatorSegment } from "./protocol.js";
import { assertNever, ShimError } from "./protocol.js";

type AriaRole = Parameters<Page["getByRole"]>[0];

interface LocatorScope {
	getByRole: Page["getByRole"];
	getByLabel: Page["getByLabel"];
	getByPlaceholder: Page["getByPlaceholder"];
	getByText: Page["getByText"];
	getByAltText: Page["getByAltText"];
	getByTitle: Page["getByTitle"];
	getByTestId: Page["getByTestId"];
	locator: (selector: string) => Locator;
}

function applySegment(scope: LocatorScope, segment: LocatorSegment): Locator {
	switch (segment.type) {
		case "role": {
			if (segment.name === null) {
				return scope.getByRole(segment.role as AriaRole);
			}
			return scope.getByRole(segment.role as AriaRole, {
				name: segment.name,
				exact: segment.exact,
			});
		}
		case "label":
			return scope.getByLabel(segment.text, { exact: segment.exact });
		case "placeholder":
			return scope.getByPlaceholder(segment.text, { exact: segment.exact });
		case "text":
			return scope.getByText(segment.text, { exact: segment.exact });
		case "alt":
			return scope.getByAltText(segment.text, { exact: segment.exact });
		case "title":
			return scope.getByTitle(segment.text, { exact: segment.exact });
		case "testid":
			return scope.getByTestId(segment.id);
		case "frame":
		case "css":
			return scope.locator(segment.selector);
		case "nth":
			throw new ShimError("internal", "nth may not be the first segment");
		case "ref":
			return scope.locator(`aria-ref=${segment.ref}`);
		default:
			return assertNever(segment);
	}
}

/** The iframe element that a `frame:` segment enters. */
export interface FrameOwner {
	readonly locator: Locator;
	/** The segments up to and including the frame and its `nth`. */
	readonly segments: readonly LocatorSegment[];
}

interface Chain {
	readonly locator: Locator;
	readonly frames: readonly FrameOwner[];
}

function buildChain(page: Page, segments: readonly LocatorSegment[]): Chain {
	const [first, ...rest] = segments;
	if (first === undefined) {
		throw new ShimError("internal", "locator has no segments");
	}
	const frames: FrameOwner[] = [];
	let chain = applySegment(page, first);
	let enteringFrame = first.type === "frame";
	for (const [offset, segment] of rest.entries()) {
		if (segment.type === "nth") {
			// 0-based; a negative index counts from the end (SPEC 6).
			chain = chain.nth(segment.index);
			continue;
		}
		if (enteringFrame) {
			frames.push({ locator: chain, segments: segments.slice(0, offset + 1) });
			chain = applySegment(chain.contentFrame(), segment);
		} else {
			chain = applySegment(chain, segment);
		}
		enteringFrame = segment.type === "frame";
	}
	if (enteringFrame) {
		throw new ShimError(
			"internal",
			"a frame locator needs an element segment inside the frame",
		);
	}
	return { locator: chain, frames };
}

export function buildLocator(
	page: Page,
	segments: readonly LocatorSegment[],
): Locator {
	return buildChain(page, segments).locator;
}

/** The iframe elements that the locator's `frame:` segments enter, in order. */
export function frameOwners(
	page: Page,
	segments: readonly LocatorSegment[],
): readonly FrameOwner[] {
	return buildChain(page, segments).frames;
}

function quote(text: string): string {
	return JSON.stringify(text);
}

function describeSegment(segment: LocatorSegment): string {
	switch (segment.type) {
		case "role": {
			if (segment.name === null) {
				return `getByRole(${quote(segment.role)})`;
			}
			return `getByRole(${quote(segment.role)}, { name: ${quote(
				segment.name,
			)}, exact: ${String(segment.exact)} })`;
		}
		case "label":
			return `getByLabel(${quote(segment.text)}, { exact: ${String(
				segment.exact,
			)} })`;
		case "placeholder":
			return `getByPlaceholder(${quote(segment.text)}, { exact: ${String(
				segment.exact,
			)} })`;
		case "text":
			return `getByText(${quote(segment.text)}, { exact: ${String(
				segment.exact,
			)} })`;
		case "alt":
			return `getByAltText(${quote(segment.text)}, { exact: ${String(
				segment.exact,
			)} })`;
		case "title":
			return `getByTitle(${quote(segment.text)}, { exact: ${String(
				segment.exact,
			)} })`;
		case "testid":
			return `getByTestId(${quote(segment.id)})`;
		case "css":
			return `locator(${quote(segment.selector)})`;
		case "frame":
			return `frameLocator(${quote(segment.selector)})`;
		case "nth":
			return `nth(${String(segment.index)})`;
		case "ref":
			return `locator(${quote(`aria-ref=${segment.ref}`)})`;
		default:
			return assertNever(segment);
	}
}

/** Human-readable chain description for error messages and tests. */
export function describeLocator(segments: readonly LocatorSegment[]): string {
	return segments.map(describeSegment).join(" >> ");
}

/**
 * Short in-page descriptions of the elements a locator resolves to, for
 * strictness-failure candidate lists. Never throws; a broken page yields [].
 */
export async function candidateDescriptions(
	locator: Locator,
): Promise<string[]> {
	try {
		return await locator.evaluateAll((elements) =>
			elements.slice(0, 10).map((element) => {
				const tag = element.tagName.toLowerCase();
				const id = element.id === "" ? "" : `#${element.id}`;
				const classAttr = element.getAttribute("class");
				const classes =
					classAttr === null || classAttr.trim() === ""
						? ""
						: `.${classAttr.trim().split(/\s+/).slice(0, 3).join(".")}`;
				const text = (element.textContent ?? "")
					.replace(/\s+/g, " ")
					.trim()
					.slice(0, 40);
				const label = text === "" ? "" : ` "${text}"`;
				return `<${tag}${id}${classes}>${label}`;
			}),
		);
	} catch {
		return [];
	}
}
