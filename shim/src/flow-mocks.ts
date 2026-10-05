import type { BrowserContext, Route } from "@playwright/test";
import type { MockHits, MockParams, MockResponse } from "./protocol.js";
import { assertNever, ShimError } from "./protocol.js";

interface MockRecord {
	readonly id: number;
	readonly method: string;
	readonly pattern: string;
	readonly regex: RegExp;
	readonly response: MockResponse;
	hits: number;
	/** False once a later mock with the same method and pattern replaced it. */
	active: boolean;
}

/** A request URL as a mock sees it: without its fragment (SPEC 7.5). */
export function mockUrl(url: string): string {
	const hash = url.indexOf("#");
	return hash === -1 ? url : url.slice(0, hash);
}

/**
 * Serves the flow's `MOCK` lines (SPEC 7.5, protocol 4.6). One context
 * route sees every request; the mock registered last that matches serves
 * it, and any other request falls back to the routes registered before,
 * such as host filtering.
 */
export class FlowMocks {
	readonly #records: MockRecord[] = [];

	/**
	 * Routes the context's requests through the mocks. Call it after every
	 * other context route, because the route registered last runs first.
	 */
	async install(context: BrowserContext): Promise<void> {
		await context.route("**/*", (route) => this.#handle(route));
	}

	register(params: MockParams): void {
		let regex: RegExp;
		try {
			regex = new RegExp(params.pattern);
		} catch {
			throw new ShimError(
				"internal",
				`malformed request: invalid mock pattern ${params.pattern}`,
			);
		}
		for (const record of this.#records) {
			if (record.method === params.method && record.pattern === params.pattern)
				record.active = false;
		}
		this.#records.push({
			id: params.id,
			method: params.method,
			pattern: params.pattern,
			regex,
			response: params.response,
			hits: 0,
			active: true,
		});
	}

	/** Every mock's served-request count, in registration order. */
	hits(): readonly MockHits[] {
		return this.#records.map(({ id, hits }) => ({ id, hits }));
	}

	/** The active mock registered last that matches, if any. */
	match(method: string, url: string): MockRecord | undefined {
		const target = mockUrl(url);
		return this.#records.findLast(
			(record) =>
				record.active && record.method === method && record.regex.test(target),
		);
	}

	async #handle(route: Route): Promise<void> {
		const request = route.request();
		const record = this.match(request.method(), request.url());
		try {
			if (record === undefined) {
				await route.fallback();
				return;
			}
			record.hits += 1;
			const response = record.response;
			switch (response.type) {
				case "fulfill":
					await route.fulfill({
						status: response.status,
						headers: Object.fromEntries(response.headers),
						body: response.body ?? "",
					});
					return;
				case "failed":
					await route.abort("failed");
					return;
				default:
					assertNever(response);
			}
		} catch {
			// The page or request is gone; nothing to do.
		}
	}
}
