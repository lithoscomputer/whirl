// Names for the iframes in an ACT snapshot (SPEC 7.4). Playwright's AI
// snapshot gives an iframe no name, even one with a title, so a planner
// cannot tell "the incident history" from any other frame.

import type { Page } from "@playwright/test";
import type { Deadline } from "./step-util.js";
import { normalizeWhitespace } from "./step-util.js";

/** Playwright leaves a longer name out of a snapshot line. */
const maxNameLength = 900;

/**
 * The most one name read waits. It waits only when its element leaves the
 * page between the count and the read.
 */
const readTimeoutMs = 1000;

/**
 * An iframe line as Playwright writes it: `- iframe [ref=e4]:`, with
 * `[active]` before the ref when the iframe has focus. Groups: the text
 * before the name's place, the text after it, and the ref.
 */
const iframeLine = /^( *- iframe )((?:\[active\] )?\[ref=([^\]\s]+)\].*)$/gm;

/** The refs of the snapshot's iframe lines, in document order. */
export function iframeRefs(snapshot: string): readonly string[] {
	return [...snapshot.matchAll(iframeLine)].flatMap((match) =>
		match[3] === undefined ? [] : [match[3]],
	);
}

/**
 * An iframe's accessible name: its `aria-label`, or else its `title`, with
 * whitespace collapsed. An empty attribute does not count. A name that
 * Playwright would not show is no name.
 */
export function iframeName(
	label: string | null,
	title: string | null,
): string | undefined {
	const name = [label, title]
		.map((attribute) => normalizeWhitespace(attribute ?? ""))
		.find((attribute) => attribute !== "");
	return name !== undefined && name.length <= maxNameLength ? name : undefined;
}

/**
 * The snapshot with each name in `names` after its iframe's role, in
 * double quotes with JSON escapes, as Playwright writes a name:
 * `- iframe "Incident history" [ref=e4]:`. Other lines stay as they are.
 *
 * Playwright also wraps a whole line in YAML single quotes when its name
 * holds text such as `: ` or ` #`. These lines stay unwrapped, because
 * Rust's snapshot parsers read the role from the start of the line.
 */
export function withIframeNames(
	snapshot: string,
	names: ReadonlyMap<string, string>,
): string {
	return snapshot.replace(
		iframeLine,
		(line, before: string, after: string, ref: string) => {
			const name = names.get(ref);
			return name === undefined
				? line
				: `${before}${JSON.stringify(name)} ${after}`;
		},
	);
}

/**
 * Adds each iframe's name to its line in a snapshot of `page` that was just
 * taken. The refs resolve through `aria-ref=` in any frame, across origins.
 * An iframe without a name, a ref that no longer resolves, a failed read,
 * and a spent budget each leave the line as it was, so the names never fail
 * the step.
 */
export async function nameIframes(
	page: Page,
	snapshot: string,
	deadline: Deadline,
): Promise<string> {
	const refs = iframeRefs(snapshot);
	if (refs.length === 0) {
		return snapshot;
	}
	const names = new Map<string, string>();
	const read = await Promise.all(
		refs.map(async (ref) => ({
			ref,
			name: await readName(page, ref, deadline),
		})),
	);
	for (const { ref, name } of read) {
		if (name !== undefined) {
			names.set(ref, name);
		}
	}
	return withIframeNames(snapshot, names);
}

async function readName(page: Page, ref: string, deadline: Deadline) {
	if (deadline.expired()) {
		return undefined;
	}
	const locator = page.locator(`aria-ref=${ref}`);
	try {
		// A replaced element never matches its ref again. The count says so
		// at once, where the read would wait for the element.
		if ((await locator.count()) === 0) {
			return undefined;
		}
		const { label, title } = await locator.evaluate(
			(element) => ({
				label: element.getAttribute("aria-label"),
				title: element.getAttribute("title"),
			}),
			undefined,
			{ timeout: Math.min(deadline.remainingMs(), readTimeoutMs) },
		);
		return iframeName(label, title);
	} catch {
		// A navigation or a closed page; the line keeps no name.
		return undefined;
	}
}
