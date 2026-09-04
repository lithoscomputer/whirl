import type { BrowserContext, Dialog, Page } from "@playwright/test";
import { ShimError } from "./protocol.js";
import { pollUntilPass } from "./step-util.js";

interface Popup {
	readonly opener: Page;
	readonly page: Page;
}

/** Owns named tabs and popup events for one flow. */
export class FlowTabs {
	readonly #context: BrowserContext;
	readonly #names = new Map<string, Page>();
	readonly #listeners = new Map<Page, (page: Page) => void>();
	readonly #dialogPolicy: "accept" | "dismiss";
	#activeName = "main";
	#pending: Popup[] = [];

	constructor(
		context: BrowserContext,
		main: Page,
		dialogs: "accept" | "dismiss",
	) {
		this.#context = context;
		this.#dialogPolicy = dialogs;
		this.#names.set("main", main);
		this.#trackPage(main);
		context.on("page", this.#trackPage);
		context.once("close", () => this.#dispose());
	}

	readonly #handleDialog = (dialog: Dialog): void => {
		const settle =
			this.#dialogPolicy === "accept" ? dialog.accept() : dialog.dismiss();
		void settle.catch(() => {
			// Closing a tab can dismiss its pending dialog.
		});
	};

	readonly #trackPage = (page: Page): void => {
		const onPopup = (popup: Page): void => {
			this.#pending.push({ opener: page, page: popup });
		};
		this.#listeners.set(page, onPopup);
		page.on("popup", onPopup);
		page.on("dialog", this.#handleDialog);
	};

	#dispose(): void {
		this.#context.off("page", this.#trackPage);
		for (const [page, onPopup] of this.#listeners) {
			page.off("popup", onPopup);
			page.off("dialog", this.#handleDialog);
		}
		this.#listeners.clear();
		this.#pending = [];
		this.#names.clear();
	}

	beginEntry(): void {
		this.#pending = [];
	}

	#named(name: string): Page {
		const page = this.#names.get(name);
		if (page === undefined)
			throw new ShimError("action", `unknown tab ${name}`);
		return page;
	}

	current(): Page {
		const page = this.#named(this.#activeName);
		if (page.isClosed()) {
			throw new ShimError(
				"action",
				`tab ${this.#activeName} is closed; select an open tab with TAB`,
			);
		}
		return page;
	}

	select(name: string): void {
		const page = this.#named(name);
		if (page.isClosed()) throw new ShimError("action", `tab ${name} is closed`);
		this.#activeName = name;
	}

	async capture(name: string, timeoutMs: number): Promise<void> {
		if (this.#names.has(name))
			throw new ShimError("action", `tab ${name} is already named`);
		const opener = this.current();
		await pollUntilPass(
			timeoutMs,
			async () => {
				const candidates = this.#pending.filter(
					(popup) => popup.opener === opener,
				);
				if (candidates.length > 1) {
					throw new ShimError(
						"strictness",
						"multiple unnamed popups opened in this entry",
						{
							candidates: candidates.map((popup) => popup.page.url()),
						},
					);
				}
				const popup = candidates[0];
				if (popup === undefined) return { pass: false, actual: "no new popup" };
				this.#names.set(name, popup.page);
				this.#pending = this.#pending.filter(
					(candidate) => candidate !== popup,
				);
				return { pass: true, actual: popup.page.url() };
			},
			{
				kind: "timeout",
				message: `no popup from the current tab within ${String(timeoutMs)}ms`,
			},
		);
	}

	async close(name: string): Promise<void> {
		await this.#named(name).close();
	}

	async assertClosed(name: string, timeoutMs: number): Promise<void> {
		const page = this.#named(name);
		await pollUntilPass(
			timeoutMs,
			async () => ({
				pass: page.isClosed(),
				actual: page.isClosed() ? "closed" : "open",
			}),
			{
				kind: "assert",
				message: `tab ${name} did not close within ${String(timeoutMs)}ms`,
				expected: "closed",
			},
		);
	}
}
