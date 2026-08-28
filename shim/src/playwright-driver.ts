// The Playwright-backed driver: one browser per process, one context and
// page per flow, and the step implementations (protocol sections 3-6).

import { mkdir } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname } from "node:path";
import type {
	Browser,
	BrowserContext,
	BrowserContextOptions,
	Locator,
	Page,
} from "@playwright/test";
import { chromium, firefox, webkit } from "@playwright/test";
import { runAssert, runPage } from "./assertions.js";
import { runCapture } from "./captures.js";
import type { ShimDriver } from "./driver.js";
import { buildEvalExpression } from "./eval-support.js";
import { createHostAllowlist } from "./host-glob.js";
import { buildLocator, describeLocator } from "./locators.js";
import type { Params } from "./params.js";
import {
	fieldArray,
	fieldArrayOrNull,
	fieldBoolean,
	fieldNumber,
	fieldObject,
	fieldObjectOrNull,
	fieldString,
} from "./params.js";
import type {
	AssertSpec,
	BrowserEngine,
	CaptureFilter,
	CaptureSource,
	EndFlowParams,
	EndFlowResult,
	ErrorKind,
	LocatorSegment,
	PageExpectation,
	StartFlowParams,
	StepCommand,
} from "./protocol.js";
import { assertNever, ShimError } from "./protocol.js";
import { runSnapshot } from "./snapshots.js";
import {
	isStrictModeViolation,
	isTargetClosedError,
	isTimeoutError,
	shortErrorMessage,
	strictnessError,
} from "./step-util.js";

const require = createRequire(import.meta.url);

const playwrightCoreVersion = (
	require("playwright-core/package.json") as { version: string }
).version;

/** Bound for force-closing a wedged context or browser (protocol 6). */
const closeWatchdogMs = 3000;

interface FlowState {
	readonly context: BrowserContext;
	readonly page: Page;
	readonly blockedHosts: Set<string>;
	readonly traceActive: boolean;
	readonly video: {
		readonly tempDir: string;
		readonly finalPath: string;
	} | null;
}

/** Resolves true when the promise settles in time, false on the deadline. */
async function settlesWithin(
	promise: Promise<unknown>,
	timeoutMs: number,
): Promise<boolean> {
	let timer: NodeJS.Timeout | undefined;
	try {
		return await Promise.race([
			promise.then(
				() => true,
				() => true,
			),
			new Promise<false>((resolve) => {
				timer = setTimeout(() => resolve(false), timeoutMs);
			}),
		]);
	} finally {
		clearTimeout(timer);
	}
}

function browserType(engine: BrowserEngine) {
	switch (engine) {
		case "chromium":
			return chromium;
		case "firefox":
			return firefox;
		case "webkit":
			return webkit;
		default:
			return assertNever(engine);
	}
}

async function installHostFiltering(
	context: BrowserContext,
	allowHosts: readonly string[],
	blockedHosts: Set<string>,
): Promise<void> {
	const isAllowed = createHostAllowlist(allowHosts);
	await context.route("**/*", async (route) => {
		try {
			const url = new URL(route.request().url());
			// data: and blob: URLs have no host and are always allowed.
			if (url.protocol !== "http:" && url.protocol !== "https:") {
				await route.continue();
				return;
			}
			if (isAllowed(url.hostname)) {
				await route.continue();
			} else {
				blockedHosts.add(url.hostname.toLowerCase());
				await route.abort("blockedbyclient");
			}
		} catch {
			// The page or request is gone; nothing to do.
		}
	});
	await context.routeWebSocket(
		(_url) => true,
		(webSocketRoute) => {
			const url = new URL(webSocketRoute.url());
			if (isAllowed(url.hostname)) {
				// Connect through; unhandled messages forward automatically.
				webSocketRoute.connectToServer();
			} else {
				blockedHosts.add(url.hostname.toLowerCase());
				webSocketRoute.close();
			}
		},
	);
}

export class PlaywrightDriver implements ShimDriver {
	readonly playwrightVersion: string = playwrightCoreVersion;

	#browser: Browser | null = null;
	#browserKey: string | null = null;
	#flow: FlowState | null = null;
	#cancelRequested = false;

	async #ensureBrowser(
		engine: BrowserEngine,
		headed: boolean,
	): Promise<Browser> {
		const key = `${engine}:${headed ? "headed" : "headless"}`;
		if (
			this.#browser !== null &&
			this.#browserKey === key &&
			this.#browser.isConnected()
		) {
			return this.#browser;
		}
		if (this.#browser !== null) {
			await settlesWithin(this.#browser.close(), closeWatchdogMs);
			this.#browser = null;
			this.#browserKey = null;
		}
		const browser = await browserType(engine).launch({ headless: !headed });
		this.#browser = browser;
		this.#browserKey = key;
		return browser;
	}

	#requireFlow(): FlowState {
		if (this.#flow === null) {
			throw new ShimError("internal", "no active flow");
		}
		return this.#flow;
	}

	async startFlow(params: StartFlowParams): Promise<void> {
		if (this.#flow !== null) {
			throw new ShimError("internal", "a flow is already active");
		}
		this.#cancelRequested = false;
		const browser = await this.#ensureBrowser(params.browser, params.headed);
		const contextOptions: BrowserContextOptions = {
			viewport: {
				width: params.viewport.width,
				height: params.viewport.height,
			},
			...(params.storageStatePath === null
				? {}
				: { storageState: params.storageStatePath }),
			...(params.video === null
				? {}
				: { recordVideo: { dir: params.video.tempDir } }),
			...(params.harPath === null
				? {}
				: { recordHar: { path: params.harPath } }),
			// Service workers can bypass request routing (SPEC section 5).
			...(params.allowHosts === null
				? {}
				: { serviceWorkers: "block" as const }),
		};
		const context = await browser.newContext(contextOptions);
		context.setDefaultNavigationTimeout(params.navTimeoutMs);
		const blockedHosts = new Set<string>();
		if (params.allowHosts !== null) {
			await installHostFiltering(context, params.allowHosts, blockedHosts);
		}
		if (params.trace) {
			await context.tracing.start({ screenshots: true, snapshots: true });
		}
		const page = await context.newPage();
		page.on("dialog", (dialog) => {
			const settle =
				params.dialogs === "accept" ? dialog.accept() : dialog.dismiss();
			settle.catch(() => {
				// The dialog is already gone; nothing to do.
			});
		});
		this.#flow = {
			context,
			page,
			blockedHosts,
			traceActive: params.trace,
			video: params.video,
		};
	}

	async endFlow(params: EndFlowParams): Promise<EndFlowResult> {
		const flow = this.#requireFlow();
		this.#flow = null;
		if (flow.traceActive) {
			// A null tracePath discards the running trace.
			if (params.tracePath === null) {
				await flow.context.tracing.stop();
			} else {
				await mkdir(dirname(params.tracePath), { recursive: true });
				await flow.context.tracing.stop({ path: params.tracePath });
			}
		}
		if (params.saveStoragePath !== null) {
			await mkdir(dirname(params.saveStoragePath), { recursive: true });
			await flow.context.storageState({ path: params.saveStoragePath });
		}
		const video = flow.video === null ? null : flow.page.video();
		// The context close finalizes the video recording and the HAR file.
		await flow.context.close();
		let videoPath: string | null = null;
		if (flow.video !== null && video !== null) {
			await mkdir(dirname(flow.video.finalPath), { recursive: true });
			await video.saveAs(flow.video.finalPath);
			await video.delete().catch(() => {
				// Leaving the temp recording behind is harmless.
			});
			videoPath = flow.video.finalPath;
		}
		return {
			blockedHosts: [...flow.blockedHosts].sort(),
			videoPath,
		};
	}

	async cancelFlow(): Promise<void> {
		this.#cancelRequested = true;
		const flow = this.#flow;
		this.#flow = null;
		if (flow === null) {
			return;
		}
		const closed = await settlesWithin(flow.context.close(), closeWatchdogMs);
		if (!closed) {
			// The context is wedged; drop the whole browser so the next
			// startFlow relaunches cleanly.
			const browser = this.#browser;
			this.#browser = null;
			this.#browserKey = null;
			if (browser !== null) {
				await settlesWithin(browser.close(), closeWatchdogMs);
			}
		}
	}

	async runStep(cmd: StepCommand, params: Params): Promise<Params> {
		const flow = this.#requireFlow();
		this.#cancelRequested = false;
		const timeoutMs = fieldNumber(params, "timeoutMs");
		const title = fieldString(params, "title");
		if (flow.traceActive) {
			await flow.context.tracing.group(title).catch(() => {
				// Tracing hiccups never fail a step.
			});
		}
		try {
			return await this.#dispatchStep(flow, cmd, params, timeoutMs);
		} catch (error) {
			throw this.#mapStepError(cmd, error);
		} finally {
			if (flow.traceActive) {
				await flow.context.tracing.groupEnd().catch(() => {
					// The context may already be closed after a cancel.
				});
			}
		}
	}

	async #dispatchStep(
		flow: FlowState,
		cmd: StepCommand,
		params: Params,
		timeoutMs: number,
	): Promise<Params> {
		const page = flow.page;
		switch (cmd) {
			case "visit":
				await page.goto(fieldString(params, "url"), { timeout: timeoutMs });
				return {};
			case "click":
				await this.#locatorAction(page, params, (locator) =>
					locator.click({ timeout: timeoutMs }),
				);
				return {};
			case "dblclick":
				await this.#locatorAction(page, params, (locator) =>
					locator.dblclick({ timeout: timeoutMs }),
				);
				return {};
			case "fill": {
				const value = fieldString(params, "value");
				await this.#locatorAction(page, params, (locator) =>
					locator.fill(value, { timeout: timeoutMs }),
				);
				return {};
			}
			case "press": {
				const key = fieldString(params, "key");
				if (fieldArrayOrNull(params, "locator") === null) {
					await page.keyboard.press(key);
					return {};
				}
				await this.#locatorAction(page, params, (locator) =>
					locator.press(key, { timeout: timeoutMs }),
				);
				return {};
			}
			case "checkbox": {
				const checked = fieldBoolean(params, "checked");
				await this.#locatorAction(page, params, (locator) =>
					locator.setChecked(checked, { timeout: timeoutMs }),
				);
				return {};
			}
			case "selectOption": {
				const label = fieldString(params, "label");
				await this.#locatorAction(page, params, (locator) =>
					locator.selectOption({ label }, { timeout: timeoutMs }),
				);
				return {};
			}
			case "hover":
				await this.#locatorAction(page, params, (locator) =>
					locator.hover({ timeout: timeoutMs }),
				);
				return {};
			case "upload": {
				const path = fieldString(params, "path");
				await this.#locatorAction(page, params, (locator) =>
					locator.setInputFiles(path, { timeout: timeoutMs }),
				);
				return {};
			}
			case "screenshot": {
				const path = fieldString(params, "path");
				await mkdir(dirname(path), { recursive: true });
				await page.screenshot({ path, fullPage: true, timeout: timeoutMs });
				return {};
			}
			case "snapshot": {
				const result = await runSnapshot(
					page,
					{
						baselinePath: fieldString(params, "baselinePath"),
						actualPath: fieldString(params, "actualPath"),
						diffPath: fieldString(params, "diffPath"),
						update: fieldBoolean(params, "update"),
					},
					timeoutMs,
				);
				return { ...result };
			}
			case "evalAction": {
				await this.#runEvalAction(
					page,
					fieldString(params, "script"),
					timeoutMs,
				);
				return {};
			}
			case "page": {
				const expectation = fieldObject(
					params,
					"expect",
				) as unknown as PageExpectation;
				await runPage(page, expectation, timeoutMs);
				return {};
			}
			case "assert": {
				const spec = fieldObject(params, "spec") as unknown as AssertSpec;
				await runAssert(page, spec, timeoutMs);
				return {};
			}
			case "capture": {
				const source = fieldObject(
					params,
					"source",
				) as unknown as CaptureSource;
				const filter = fieldObjectOrNull(
					params,
					"filter",
				) as CaptureFilter | null;
				const value = await runCapture(page, source, filter, timeoutMs);
				return { value };
			}
			default:
				return assertNever(cmd);
		}
	}

	async #runEvalAction(
		page: Page,
		script: string,
		timeoutMs: number,
	): Promise<void> {
		const expression = buildEvalExpression(script);
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
			// The action form discards the result.
			await Promise.race([page.evaluate(expression), timedOut]);
		} catch (error) {
			if (error instanceof ShimError) {
				throw error;
			}
			throw new ShimError("eval", shortErrorMessage(error));
		} finally {
			clearTimeout(timer);
		}
	}

	#readLocator(
		page: Page,
		params: Params,
	): {
		readonly locator: Locator;
		readonly description: string;
	} {
		const segments = fieldArray(params, "locator") as readonly LocatorSegment[];
		return {
			locator: buildLocator(page, segments),
			description: describeLocator(segments),
		};
	}

	async #locatorAction(
		page: Page,
		params: Params,
		action: (locator: Locator) => Promise<unknown>,
	): Promise<void> {
		const { locator, description } = this.#readLocator(page, params);
		try {
			await action(locator);
		} catch (error) {
			if (isStrictModeViolation(error)) {
				let count = 0;
				try {
					count = await locator.count();
				} catch {
					// The page is gone; report without a count.
				}
				throw await strictnessError(locator, description, count);
			}
			throw error;
		}
	}

	#mapStepError(cmd: StepCommand, error: unknown): ShimError {
		if (error instanceof ShimError) {
			return error;
		}
		if (
			this.#cancelRequested ||
			(this.#flow === null && isTargetClosedError(error))
		) {
			return new ShimError("cancelled", "step aborted by cancelFlow");
		}
		if (isTimeoutError(error)) {
			return new ShimError("timeout", shortErrorMessage(error));
		}
		const fallback: ErrorKind = defaultErrorKind(cmd);
		return new ShimError(fallback, shortErrorMessage(error));
	}

	async dispose(): Promise<void> {
		if (this.#flow !== null) {
			await settlesWithin(this.#flow.context.close(), closeWatchdogMs);
			this.#flow = null;
		}
		if (this.#browser !== null) {
			await settlesWithin(this.#browser.close(), closeWatchdogMs);
			this.#browser = null;
			this.#browserKey = null;
		}
	}
}

function defaultErrorKind(cmd: StepCommand): ErrorKind {
	switch (cmd) {
		case "visit":
		case "click":
		case "dblclick":
		case "fill":
		case "press":
		case "checkbox":
		case "selectOption":
		case "hover":
		case "upload":
		case "screenshot":
			return "action";
		case "evalAction":
			return "eval";
		case "snapshot":
		case "page":
		case "assert":
		case "capture":
			return "internal";
		default:
			return assertNever(cmd);
	}
}
