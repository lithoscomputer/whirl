// The locator generator of the AI cache (SPEC 12.1, protocol 4.8): turns
// the element behind a snapshot ref into a strict Whirl locator.

import type { ElementHandle, Frame, Page } from "@playwright/test";
import { buildLocator } from "./locators.js";
import type { LocatorSegment } from "./protocol.js";

/** What the generator found: a locator, or why there is none. */
export type GeneratedLocator =
	| { readonly type: "locator"; readonly locator: readonly LocatorSegment[] }
	| { readonly type: "unstable"; readonly reason: string };

/** What the page says about the element, for the candidate locators. */
interface ElementFacts {
	readonly testId: string | null;
	readonly labels: readonly string[];
	readonly placeholder: string | null;
	readonly text: string | null;
	/** The nearest named landmark, dialog, or region around the element. */
	readonly scope: { readonly role: string; readonly name: string } | null;
}

/** Text longer than this makes a poor `text:` locator. */
const maxTextLength = 80;

/** Runs in the page: the facts the candidates need. */
function readFacts(element: Element): ElementFacts {
	const normalize = (text: string | null | undefined): string =>
		(text ?? "").replace(/[​­]/g, "").replace(/\s+/g, " ").trim();
	const nameOf = (node: Element): string => {
		const label = normalize(node.getAttribute("aria-label"));
		if (label !== "") return label;
		const ids = normalize(node.getAttribute("aria-labelledby"));
		if (ids === "") return "";
		return normalize(
			ids
				.split(" ")
				.map((id) => node.ownerDocument.getElementById(id)?.textContent ?? "")
				.join(" "),
		);
	};
	const implicitRoles: Record<string, string> = {
		DIALOG: "dialog",
		NAV: "navigation",
		MAIN: "main",
		FORM: "form",
		SECTION: "region",
		ASIDE: "complementary",
		ARTICLE: "article",
	};
	const scopeRoles = new Set([
		"dialog",
		"alertdialog",
		"navigation",
		"main",
		"form",
		"region",
		"complementary",
		"search",
		"article",
		"banner",
		"contentinfo",
	]);
	let scope: { role: string; name: string } | null = null;
	for (
		let node = element.parentElement;
		node !== null && scope === null;
		node = node.parentElement
	) {
		const role = node.getAttribute("role") ?? implicitRoles[node.tagName];
		if (role === undefined || !scopeRoles.has(role)) continue;
		const name = nameOf(node);
		if (name !== "") scope = { role, name };
	}
	const labels =
		"labels" in element && element.labels instanceof NodeList
			? [...(element.labels as NodeListOf<HTMLLabelElement>)]
					.map((label) => normalize(label.textContent))
					.filter((label) => label !== "")
			: [];
	const ownLabel = nameOf(element);
	const text = normalize(element.textContent);
	return {
		testId: element.getAttribute("data-testid"),
		labels: ownLabel === "" ? labels : [...labels, ownLabel],
		placeholder: element.getAttribute("placeholder"),
		text: text === "" ? null : text,
		scope,
	};
}

/** A CSS string in single quotes. */
function cssString(text: string): string {
	return `'${text.replace(/\\/g, "\\\\").replace(/'/g, "\\'")}'`;
}

/**
 * The `frame:` segments that reach the element's frame from the page, each
 * naming its iframe by `title`, `name`, or `id` (SPEC 12.1). `null` when an
 * iframe has no attribute that names it alone.
 */
async function framePath(frame: Frame): Promise<LocatorSegment[] | null> {
	const segments: LocatorSegment[] = [];
	for (
		let current = frame, parent = frame.parentFrame();
		parent !== null;
		current = parent, parent = parent.parentFrame()
	) {
		const owner = await current.frameElement();
		const attributes = await owner.evaluate((element) => {
			const iframe = element as Element;
			return {
				title: iframe.getAttribute("title"),
				name: iframe.getAttribute("name"),
				id: iframe.getAttribute("id"),
			};
		});
		const selectors = [
			attributes.title === null
				? null
				: `iframe[title=${cssString(attributes.title)}]`,
			attributes.name === null
				? null
				: `iframe[name=${cssString(attributes.name)}]`,
			attributes.id === null ? null : `iframe[id=${cssString(attributes.id)}]`,
		].filter((selector): selector is string => selector !== null);
		let found: string | null = null;
		for (const selector of selectors) {
			if ((await parent.locator(selector).count()) === 1) {
				found = selector;
				break;
			}
		}
		if (found === null) return null;
		segments.unshift({ type: "frame", selector: found });
	}
	return segments;
}

/** The candidate segments, best first (SPEC 12.1). */
function candidates(
	facts: ElementFacts,
	role: string,
	name: string | null,
): LocatorSegment[] {
	const list: LocatorSegment[] = [];
	if (facts.testId !== null && facts.testId !== "")
		list.push({ type: "testid", id: facts.testId });
	if (name !== null && name !== "" && role !== "generic")
		list.push({ type: "role", role, name, exact: true });
	for (const label of facts.labels)
		list.push({ type: "label", text: label, exact: true });
	if (facts.placeholder !== null && facts.placeholder !== "")
		list.push({ type: "placeholder", text: facts.placeholder, exact: true });
	if (facts.text !== null && facts.text.length <= maxTextLength)
		list.push({ type: "text", text: facts.text, exact: true });
	return list;
}

/** Counts a locator's matches and checks that one of them is the element. */
async function matches(
	page: Page,
	segments: readonly LocatorSegment[],
	element: ElementHandle,
): Promise<{ readonly count: number; readonly index: number }> {
	const locator = buildLocator(page, segments);
	const all = await locator.elementHandles();
	let index = -1;
	for (const [position, handle] of all.entries()) {
		if (index === -1 && (await handle.evaluate((a, b) => a === b, element)))
			index = position;
		await handle.dispose();
	}
	return { count: all.length, index };
}

/**
 * Generates a strict locator for the element behind `ref`, from the latest
 * AI snapshot. `role` and `name` are the element's, from that snapshot.
 */
export async function generateLocator(
	page: Page,
	ref: string,
	role: string,
	name: string | null,
): Promise<GeneratedLocator> {
	// A replaced element never comes back, so the generator does not wait.
	const target = page.locator(`aria-ref=${ref}`);
	const handle =
		(await target.count()) === 1
			? await target.elementHandle({ timeout: 1_000 })
			: null;
	if (handle === null)
		return { type: "unstable", reason: "the element is gone" };
	try {
		const frame = await handle.ownerFrame();
		const prefix = frame === null ? [] : await framePath(frame);
		if (prefix === null)
			return {
				type: "unstable",
				reason: "no title, name, or id names the element's iframe alone",
			};
		const facts = await handle.evaluate(readFacts);
		const options = candidates(facts, role, name);
		let fallback: readonly LocatorSegment[] | null = null;
		for (const option of options) {
			const segments = [...prefix, option];
			const { count, index } = await matches(page, segments, handle);
			if (index === -1) continue;
			if (count === 1) return { type: "locator", locator: segments };
			fallback ??= segments;
		}
		if (facts.scope !== null) {
			const scope: LocatorSegment = {
				type: "role",
				role: facts.scope.role,
				name: facts.scope.name,
				exact: true,
			};
			for (const option of options) {
				const segments = [...prefix, scope, option];
				const { count, index } = await matches(page, segments, handle);
				if (index !== -1 && count === 1)
					return { type: "locator", locator: segments };
			}
		}
		if (fallback !== null) {
			const { index } = await matches(page, fallback, handle);
			if (index !== -1)
				return {
					type: "locator",
					locator: [...fallback, { type: "nth", index }],
				};
		}
		return { type: "unstable", reason: "no locator finds the element alone" };
	} finally {
		await handle.dispose();
	}
}
