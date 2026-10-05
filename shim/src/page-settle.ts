import type { BrowserContext, Page, Request } from "@playwright/test";

/** How long the network must stay quiet before a snapshot (SPEC 7.4). */
export const quietMs = 500;
/** The shortest wait, so a request that a click just started is seen. */
export const minimumWaitMs = 100;
/** The longest wait for a settled page. */
export const maximumWaitMs = 5_000;
/** A request open this long, such as a long poll, stops counting. */
export const staleRequestMs = 2_000;
const pollMs = 25;

/** Requests that never finish on their own, so they never block a settle. */
const streamingTypes = new Set(["websocket", "eventsource"]);

/**
 * The flow's open requests, so a model sees the page after the data it
 * waits for has arrived. The clock is injectable for tests.
 */
export class PageActivity {
	readonly #open = new Map<Request, number>();
	readonly #now: () => number;
	#lastActivity = Number.NEGATIVE_INFINITY;

	constructor(now: () => number = Date.now) {
		this.#now = now;
	}

	/** Starts listening to the context's requests. */
	watch(context: BrowserContext): void {
		context.on("request", this.started);
		context.on("requestfinished", this.ended);
		context.on("requestfailed", this.ended);
		context.once("close", () => {
			context.off("request", this.started);
			context.off("requestfinished", this.ended);
			context.off("requestfailed", this.ended);
			this.#open.clear();
		});
	}

	readonly started = (request: Pick<Request, "resourceType">): void => {
		if (streamingTypes.has(request.resourceType())) return;
		const now = this.#now();
		this.#open.set(request as Request, now);
		this.#lastActivity = now;
	};

	readonly ended = (request: Pick<Request, "resourceType">): void => {
		if (this.#open.delete(request as Request)) this.#lastActivity = this.#now();
	};

	/** True when no request counts and the network has been quiet long enough. */
	quiet(): boolean {
		const now = this.#now();
		for (const [request, since] of this.#open) {
			if (now - since >= staleRequestMs) this.#open.delete(request);
		}
		return this.#open.size === 0 && now - this.#lastActivity >= quietMs;
	}

	/**
	 * Waits until the document has loaded its DOM and the network has been
	 * quiet for `quietMs`, for at least `minimumWaitMs` and at most `capMs`.
	 * It never fails: a page that does not settle is snapshotted as it is.
	 */
	async settle(page: Page, capMs: number): Promise<void> {
		const cap = Math.min(capMs, maximumWaitMs);
		const start = this.#now();
		try {
			await page.waitForLoadState("domcontentloaded", {
				timeout: Math.max(cap, 1),
			});
		} catch {
			// The cap passed first, or the page closed; snapshot what is there.
		}
		for (;;) {
			const waited = this.#now() - start;
			if (waited >= cap) return;
			if (waited >= minimumWaitMs && this.quiet()) return;
			await new Promise((resolve) => setTimeout(resolve, pollMs));
		}
	}
}
