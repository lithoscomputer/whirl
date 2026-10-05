// The Playwright-backed driver: one browser per process, one context and
// named pages per flow, and the step implementations (protocol sections 3-6).

import { randomUUID } from "node:crypto";
import { mkdir, stat } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname } from "node:path";
import type {
	APIResponse,
	Browser,
	BrowserContext,
	BrowserContextOptions,
	Locator,
	Page,
} from "@playwright/test";
import { chromium, devices, expect, firefox, webkit } from "@playwright/test";
import { runAssert, runPage } from "./assertions.js";
import type { ShimDriver } from "./driver.js";
import { buildEvalExpression } from "./eval-support.js";
import { FlowNetwork } from "./flow-network.js";
import { FlowTabs } from "./flow-tabs.js";
import { createHostAllowlist } from "./host-glob.js";
import { nameIframes } from "./iframe-names.js";
import { buildLocator, describeLocator, frameOwners } from "./locators.js";
import type { Params } from "./params.js";
import {
	decodeHttpParams,
	decodeScrollMotion,
	decodeSnapshotComparison,
	fieldArray,
	fieldArrayOrNull,
	fieldBoolean,
	fieldEnum,
	fieldNumber,
	fieldObject,
	fieldString,
	fieldStringOrNull,
} from "./params.js";
import type {
	AssertSpec,
	BrowserEngine,
	EndFlowParams,
	EndFlowResult,
	ErrorKind,
	LocatorSegment,
	MouseButton,
	PageExpectation,
	ReadSubject,
	ScrollMotion,
	StartFlowParams,
	StepCommand,
} from "./protocol.js";
import { assertNever, ShimError } from "./protocol.js";
import { runRead } from "./reads.js";
import { runSnapshot } from "./snapshots.js";
import {
	actionErrorMessage,
	Deadline,
	isDropRejected,
	isStrictModeViolation,
	isTargetClosedError,
	isTimeoutError,
	shortErrorMessage,
	sleep,
	strictnessError,
} from "./step-util.js";
import type { RecordingOutcome } from "./video-recorder.js";
import {
	PageScreencast,
	PLAYWRIGHT_VIDEO_FPS,
	resolveFfmpegPath,
	ScreencastRecorder,
} from "./video-recorder.js";

const require = createRequire(import.meta.url);

const playwrightCoreVersion = (
	require("playwright-core/package.json") as { version: string }
).version;

/** Bound for force-closing a wedged context or browser (protocol 6). */
const closeWatchdogMs = 3000;

function resolveUserAgent(value: string | null) {
	let deviceName: string;
	switch (value) {
		case "chrome":
			deviceName = "Desktop Chrome";
			break;
		case "firefox":
			deviceName = "Desktop Firefox";
			break;
		case "safari":
			deviceName = "Desktop Safari";
			break;
		default:
			return value ?? undefined;
	}
	const device = devices[deviceName];
	if (device === undefined) {
		throw new ShimError("internal", `missing Playwright device: ${deviceName}`);
	}
	return device.userAgent;
}

interface FlowState {
	readonly context: BrowserContext;
	readonly page: Page;
	readonly tabs: FlowTabs;
	readonly network: FlowNetwork;
	readonly blockedHosts: Set<string>;
	readonly traceActive: boolean;
	readonly video: {
		readonly tempDir: string;
		readonly finalPath: string;
	} | null;
	/** The screencast recorder; null when Playwright's recorder or no video. */
	readonly recorder: ScreencastRecorder | null;
	/**
	 * The page function a clicked element calls to confirm a trusted click.
	 * It is bound once for the whole context at flow start, so a click step
	 * does not spend its own timeout on the setup round trip.
	 */
	readonly clickBinding: string;
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

const redirectStatuses = new Set([301, 302, 303, 307, 308]);

/** The browser's own redirect-hop limit. */
const maxRedirectHops = 20;

const mouseButtons: readonly MouseButton[] = ["left", "right", "middle"];

/**
 * How long DRAG holds the button before it moves (SPEC section 7). Some
 * drag libraries start a drag only after a press delay, commonly 100 to
 * 300 ms, and cancel it when the pointer moves sooner.
 */
const dragHoldMs = 500;

/** DRAG's pointer moves; libraries that start after a distance need several. */
const dragSteps = 10;

/**
 * The trusted event that shows a click reached its target. Only the left
 * button fires `click`: a right click fires `contextmenu` in every engine,
 * and a middle click fires `auxclick`.
 */
const clickEvents: Readonly<Record<MouseButton, string>> = {
	left: "click",
	right: "contextmenu",
	middle: "auxclick",
};

/** The absolute target of a redirect response, or null when it is not one. */
function redirectTarget(
	status: number,
	location: string | undefined,
	base: URL,
): URL | null {
	if (!redirectStatuses.has(status) || location === undefined) {
		return null;
	}
	try {
		return new URL(location, base);
	} catch {
		return null;
	}
}

/**
 * True for requests whose response may stream forever (server-sent
 * events, media). Resolving those through `route.fetch` would buffer the
 * whole body and hang, so they skip redirect vetting.
 */
function streamsIndefinitely(request: {
	resourceType: () => string;
	headers: () => Record<string, string>;
}): boolean {
	const type = request.resourceType();
	if (type === "eventsource" || type === "media") {
		return true;
	}
	const accept = request.headers()["accept"];
	return accept?.includes("text/event-stream") ?? false;
}

async function installHostFiltering(
	context: BrowserContext,
	allowHosts: readonly string[],
	blockedHosts: Set<string>,
): Promise<void> {
	const isAllowed = createHostAllowlist(allowHosts);
	await context.route("**/*", async (route) => {
		try {
			const request = route.request();
			const url = new URL(request.url());
			// data: and blob: URLs have no host and are always allowed.
			if (url.protocol !== "http:" && url.protocol !== "https:") {
				await route.continue();
				return;
			}
			if (!isAllowed(url.hostname)) {
				blockedHosts.add(url.hostname.toLowerCase());
				await route.abort("blockedbyclient");
				return;
			}
			if (streamsIndefinitely(request)) {
				await route.continue();
				return;
			}
			// The browser follows server redirects without re-entering
			// route handlers (a fulfilled 3xx included), so a redirect to
			// a disallowed host would bypass the check. Resolve the
			// response here and vet the redirect chain hop by hop before
			// the browser may follow it.
			let response: APIResponse;
			try {
				response = await route.fetch({ maxRedirects: 0 });
			} catch {
				await route.abort("failed");
				return;
			}
			let hopUrl = url;
			let hopStatus = response.status();
			let hopLocation = response.headers()["location"];
			for (let hop = 0; hop < maxRedirectHops; hop += 1) {
				const target = redirectTarget(hopStatus, hopLocation, hopUrl);
				if (
					target === null ||
					(target.protocol !== "http:" && target.protocol !== "https:")
				) {
					break;
				}
				if (!isAllowed(target.hostname)) {
					blockedHosts.add(target.hostname.toLowerCase());
					await route.abort("blockedbyclient");
					return;
				}
				const method = request.method();
				if (method !== "GET" && method !== "HEAD") {
					// Refetching would replay a non-idempotent request, so
					// the chain is vetted one hop deep only.
					break;
				}
				hopUrl = target;
				let hopResponse: APIResponse;
				try {
					hopResponse = await route.fetch({
						maxRedirects: 0,
						url: target.toString(),
					});
				} catch {
					// The browser will surface its own error for this hop.
					break;
				}
				hopStatus = hopResponse.status();
				hopLocation = hopResponse.headers()["location"];
			}
			// Every reachable hop is allowed: hand the original response
			// to the browser, which follows the chain with its own
			// method, history, and URL semantics.
			await route.fulfill({ response });
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

interface Point {
	readonly x: number;
	readonly y: number;
}

/**
 * Runs in every frame before page scripts (SPEC 7.4). Playwright's AI
 * snapshot, like page scripts, cannot see inside a closed shadow root, so a
 * flow that uses ACT opens every root a page script attaches, which gives
 * ACT the view that a browser extension's API has. The page can
 * notice: `host.shadowRoot` is no longer null. Declarative closed roots in
 * HTML are created by the parser and stay closed.
 */
function openShadowRoots(): void {
	const attachShadow = Element.prototype.attachShadow;
	Element.prototype.attachShadow = function (
		this: Element,
		init: ShadowRootInit,
	): ShadowRoot {
		return attachShadow.call(this, { ...init, mode: "open" });
	};
}

/** A step's locator as the shim reads it from the command's params. */
interface ReadLocator {
	readonly locator: Locator;
	readonly description: string;
	readonly fromSnapshot: boolean;
}

/**
 * The page point DRAG releases at: the target's center, or for an `ACT`
 * snapshot ref the point `textTargetPosition` picks. Scrolling the target
 * into view waits until it is visible and stable. A trial hover would also
 * check that nothing covers it, but in Chromium it stops a native HTML5
 * drag that is under way.
 */
async function dropPoint(
	target: ReadLocator,
	remaining: () => number,
): Promise<Point> {
	const { locator, description, fromSnapshot } = target;
	try {
		await locator.scrollIntoViewIfNeeded({ timeout: remaining() });
		const position = fromSnapshot
			? await textTargetPosition(locator, remaining())
			: undefined;
		const box = await locator.boundingBox({ timeout: remaining() });
		if (box === null) {
			throw new ShimError(
				"action",
				`the drop target ${description} is not visible`,
			);
		}
		return {
			x: box.x + (position?.x ?? box.width / 2),
			y: box.y + (position?.y ?? box.height / 2),
		};
	} catch (error) {
		if (isStrictModeViolation(error)) {
			throw await strictnessError(locator, description, await locator.count());
		}
		throw error;
	}
}

/**
 * SCROLL's chunk and position motions (SPEC section 7), run in the page.
 * The scroll box is the element when it can scroll on the motion's axis,
 * else the largest such box inside it, else its nearest such ancestor,
 * else its document; `html` and `body` are the document. Looking inside
 * serves a locator that names a container, such as a dialog whose list
 * scrolls. It scrolls at once, whatever the page's `scroll-behavior`, and
 * resolves when the position holds for two frames. A hidden page runs no
 * frames, so a timer also drives the check there.
 */
function scrollBox(
	element: Element,
	motion: Exclude<ScrollMotion, { readonly type: "intoView" }>,
): Promise<void> {
	const doc = element.ownerDocument;
	const view = doc.defaultView ?? window;
	const vertical =
		motion.type === "position" ||
		motion.direction === "down" ||
		motion.direction === "up";
	const isDocument = (node: Element): boolean =>
		node === doc.documentElement || node === doc.body;
	const canScroll = (node: Element): boolean => {
		const style = view.getComputedStyle(node);
		const overflow = vertical ? style.overflowY : style.overflowX;
		const room = vertical
			? node.scrollHeight > node.clientHeight
			: node.scrollWidth > node.clientWidth;
		return room && ["auto", "scroll", "overlay"].includes(overflow);
	};
	const area = (node: Element): number => node.clientWidth * node.clientHeight;
	const inside =
		isDocument(element) || canScroll(element)
			? []
			: Array.from(element.querySelectorAll("*")).filter(canScroll);
	let box: Element | null =
		inside.reduce<Element | null>(
			(largest, node) =>
				largest === null || area(node) > area(largest) ? node : largest,
			null,
		) ?? element;
	while (box !== null && !isDocument(box) && !canScroll(box)) {
		const root = box.getRootNode();
		box = box.parentElement ?? (root instanceof ShadowRoot ? root.host : null);
	}
	const scroller =
		box === null || isDocument(box)
			? (doc.scrollingElement ?? doc.documentElement)
			: box;
	let top = scroller.scrollTop;
	let left = scroller.scrollLeft;
	if (motion.type === "position") {
		top =
			((scroller.scrollHeight - scroller.clientHeight) * motion.percent) / 100;
	} else {
		const sign =
			motion.direction === "down" || motion.direction === "right" ? 1 : -1;
		if (vertical) {
			top += sign * scroller.clientHeight;
		} else {
			left += sign * scroller.clientWidth;
		}
	}
	scroller.scrollTo({ top, left, behavior: "instant" });
	return new Promise((resolve) => {
		const position = (): string =>
			`${scroller.scrollTop},${scroller.scrollLeft}`;
		let last = position();
		let steady = 0;
		const check = (): void => {
			const now = position();
			steady = now === last ? steady + 1 : 0;
			last = now;
			if (steady >= 2) {
				resolve();
			} else {
				next();
			}
		};
		const next = (): void => {
			let ran = false;
			const once = (): void => {
				if (!ran) {
					ran = true;
					check();
				}
			};
			requestAnimationFrame(once);
			setTimeout(once, 50);
		};
		next();
	});
}

/**
 * Waits two animation frames, so a page that handles pointer moves once per
 * frame sees the last one before the release. A hidden page runs no frames,
 * so a short timer ends the wait there.
 */
async function nextFrames(page: Page): Promise<void> {
	await page
		.evaluate(
			() =>
				new Promise<void>((resolve) => {
					requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
					setTimeout(resolve, 100);
				}),
		)
		.catch(() => {
			// A drop that navigates destroys the context; nothing to wait for.
		});
}

/**
 * Fails DROP at once when its file is missing. Without this check the
 * step fails with Node's `ENOENT ... stat` text from inside Playwright.
 */
async function requireFile(path: string): Promise<void> {
	try {
		await stat(path);
	} catch (error) {
		if (
			error instanceof Error &&
			"code" in error &&
			(error as NodeJS.ErrnoException).code === "ENOENT"
		) {
			throw new ShimError("action", `the file ${path} does not exist`);
		}
		throw error;
	}
}

/**
 * DROP (SPEC section 7): drops one file on the element, with Playwright's
 * actionability checks. Playwright reads the file and gives it its name
 * and a type from its extension. A page accepts a drop only when its
 * `dragover` handler calls `preventDefault()`. Otherwise Playwright fires
 * `dragleave` and throws at once, and the step fails with that reason.
 */
async function dropFile(
	locator: Locator,
	description: string,
	path: string,
	timeoutMs: number,
): Promise<void> {
	try {
		await locator.drop({ files: path }, { timeout: timeoutMs });
	} catch (error) {
		if (isDropRejected(error)) {
			throw new ShimError(
				"action",
				`the drop target ${description} did not accept the drop (its dragover did not call preventDefault)`,
			);
		}
		throw error;
	}
}

/**
 * Where to point at an element that an `ACT` snapshot names (SPEC 7.4).
 * Playwright's AI snapshot folds a wrapper with one visible child into the
 * wrapper's line, so the ref can name a wide container whose center misses
 * the clickable child, as in a custom dropdown. Aim at the deepest
 * descendant that shows the same text instead; the click still bubbles to
 * the element the model chose. Undefined keeps Playwright's center.
 */
async function textTargetPosition(
	locator: Locator,
	timeoutMs: number,
): Promise<Point | undefined> {
	const position = await locator.evaluate(
		(element) => {
			const normalize = (text: string): string =>
				text.replace(/\s+/g, " ").trim();
			const text = normalize((element as HTMLElement).innerText ?? "");
			if (text === "") {
				return null;
			}
			let target: Element = element;
			for (;;) {
				const next = Array.from(target.children).find((child) => {
					if (!(child instanceof HTMLElement)) {
						return false;
					}
					const box = child.getBoundingClientRect();
					return (
						box.width > 0 &&
						box.height > 0 &&
						normalize(child.innerText) === text
					);
				});
				if (next === undefined) {
					break;
				}
				target = next;
			}
			if (target === element) {
				return null;
			}
			const outer = element.getBoundingClientRect();
			const inner = target.getBoundingClientRect();
			return {
				x: inner.left - outer.left + inner.width / 2,
				y: inner.top - outer.top + inner.height / 2,
			};
		},
		undefined,
		{ timeout: timeoutMs },
	);
	return position ?? undefined;
}

/** True for a native checkbox or radio input. */
async function isNativeToggle(
	locator: Locator,
	timeoutMs: number,
): Promise<boolean> {
	return locator.evaluate(
		(element) =>
			element instanceof HTMLInputElement &&
			(element.type === "checkbox" || element.type === "radio"),
		undefined,
		{ timeout: timeoutMs },
	);
}

export class PlaywrightDriver implements ShimDriver {
	readonly #clickReceipts = new WeakMap<
		Page,
		{
			next: number;
			received: number;
		}
	>();
	readonly playwrightVersion: string = playwrightCoreVersion;

	#browser: Browser | null = null;
	#browserKey: string | null = null;
	#flow: FlowState | null = null;
	#cancelRequested = false;

	ffmpegPath(): Promise<string | null> {
		return resolveFfmpegPath();
	}

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

	async startFlow(params: StartFlowParams): Promise<Params> {
		if (this.#flow !== null) {
			throw new ShimError("internal", "a flow is already active");
		}
		this.#cancelRequested = false;
		// An explicit rate needs the Chromium screencast recorder; Rust sends
		// null for the other engines, which keep Playwright's recorder.
		const screencastFps = params.video?.fps ?? null;
		if (screencastFps !== null && params.browser !== "chromium") {
			throw new ShimError(
				"internal",
				`a video frame rate needs chromium; ${params.browser} records at ${PLAYWRIGHT_VIDEO_FPS} fps`,
			);
		}
		const ffmpegPath =
			screencastFps === null ? null : await resolveFfmpegPath();
		if (screencastFps !== null && ffmpegPath === null) {
			throw new ShimError(
				"internal",
				"video recording needs Playwright's ffmpeg, which is missing; run `whirl install chromium`",
			);
		}
		const browser = await this.#ensureBrowser(params.browser, params.headed);
		const userAgent = resolveUserAgent(params.userAgent);
		const contextOptions: BrowserContextOptions = {
			viewport: {
				width: params.viewport.width,
				height: params.viewport.height,
			},
			...(params.storageStatePath === null
				? {}
				: { storageState: params.storageStatePath }),
			...(userAgent === undefined ? {} : { userAgent }),
			...(params.reducedMotion === null
				? {}
				: { reducedMotion: params.reducedMotion }),
			...(params.video === null || screencastFps !== null
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
		if (params.openShadowRoots) {
			await context.addInitScript(openShadowRoots);
		}
		// Bound before any page exists, so the main page and every popup
		// have it without a round trip inside a step's timeout.
		const clickBinding = `__whirlClick_${randomUUID().replaceAll("-", "")}`;
		await context.exposeBinding(clickBinding, ({ page }, token: unknown) => {
			const receipt = this.#clickReceipts.get(page);
			if (receipt !== undefined && typeof token === "number") {
				receipt.received = token;
			}
		});
		const blockedHosts = new Set<string>();
		if (params.allowHosts !== null) {
			await installHostFiltering(context, params.allowHosts, blockedHosts);
		}
		if (params.trace) {
			await context.tracing.start({ screenshots: true, snapshots: true });
		}
		const network = new FlowNetwork(context, params.allowHosts, blockedHosts);
		const page = await context.newPage();
		const tabs = new FlowTabs(context, page, params.dialogs);
		let recorder: ScreencastRecorder | null = null;
		if (
			params.video !== null &&
			screencastFps !== null &&
			ffmpegPath !== null
		) {
			const { width, height } = params.viewport;
			try {
				recorder = await ScreencastRecorder.start(
					await PageScreencast.open(page, width, height),
					{
						fps: screencastFps,
						width,
						height,
						tempDir: params.video.tempDir,
						ffmpegPath,
					},
				);
			} catch (error) {
				await settlesWithin(context.close(), closeWatchdogMs);
				throw error;
			}
		}
		this.#flow = {
			context,
			page,
			tabs,
			network,
			blockedHosts,
			traceActive: params.trace,
			video: params.video,
			recorder,
			clickBinding,
		};
		let videoFps: number | null = null;
		if (params.video !== null) {
			videoFps = screencastFps ?? PLAYWRIGHT_VIDEO_FPS;
		}
		return {
			browserVersion: browser.version(),
			nodeVersion: process.versions.node,
			playwrightVersion: this.playwrightVersion,
			userAgent: await page.evaluate(() => navigator.userAgent),
			videoFps,
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
		// The screencast recorder finalizes while the page is still alive; a
		// failure surfaces after the context is closed so nothing leaks.
		let recording: RecordingOutcome | null = null;
		let recorderFailure: unknown = null;
		if (flow.recorder !== null && flow.video !== null) {
			try {
				recording = await flow.recorder.stop(flow.video.finalPath);
			} catch (error) {
				recorderFailure = error;
			}
		}
		const video =
			flow.video === null || flow.recorder !== null ? null : flow.page.video();
		// The context close finalizes the video recording and the HAR file.
		await flow.context.close();
		if (recorderFailure !== null) {
			throw recorderFailure;
		}
		let videoPath: string | null = null;
		let videoSkipped: string | null = null;
		let videoBlank: string | null = null;
		if (flow.video !== null && recording !== null) {
			// A recording is evidence, not a result, so Rust reports a
			// skipped or blank one as a warning (SPEC section 13).
			switch (recording.type) {
				case "saved":
					videoPath = flow.video.finalPath;
					break;
				case "blank":
					videoPath = flow.video.finalPath;
					videoBlank = recording.reason;
					break;
				case "skipped":
					videoSkipped = recording.reason;
					break;
				default:
					return assertNever(recording);
			}
		} else if (flow.video !== null && video !== null) {
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
			videoSkipped,
			videoBlank,
		};
	}

	async cancelFlow(): Promise<void> {
		this.#cancelRequested = true;
		const flow = this.#flow;
		this.#flow = null;
		if (flow === null) {
			return;
		}
		if (flow.recorder !== null) {
			await flow.recorder.abort();
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
		if (params["entryStart"] === true) {
			flow.tabs.beginEntry();
			flow.network.beginEntry();
		}
		this.#cancelRequested = false;
		const timeoutMs = fieldNumber(params, "timeoutMs");
		const title = fieldStringOrNull(params, "title");
		if (cmd === "traceGroup" || cmd === "traceGroupEnd") {
			// Rust groups the reads of one check under one trace step.
			if (flow.traceActive) {
				const change =
					cmd === "traceGroup"
						? flow.context.tracing.group(title ?? "check")
						: flow.context.tracing.groupEnd();
				await change.catch(() => {
					// Tracing hiccups never fail a step.
				});
			}
			return {};
		}
		const grouped = flow.traceActive && title !== null;
		if (grouped) {
			await flow.context.tracing.group(title).catch(() => {
				// Tracing hiccups never fail a step.
			});
		}
		try {
			return await this.#dispatchStep(flow, cmd, params, timeoutMs);
		} catch (error) {
			throw this.#mapStepError(cmd, error);
		} finally {
			if (grouped) {
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
		if (cmd === "http") {
			await flow.network.http(decodeHttpParams(params), timeoutMs);
			return {};
		}
		if (cmd === "popup") {
			await flow.tabs.capture(fieldString(params, "name"), timeoutMs);
			return {};
		}
		if (cmd === "readResponse") {
			return {
				...(await flow.network.readResponse(
					fieldString(params, "name"),
					fieldBoolean(params, "body"),
					timeoutMs,
				)),
			};
		}
		if (cmd === "tab") {
			flow.tabs.select(fieldString(params, "name"));
			return {};
		}
		if (cmd === "close") {
			await flow.tabs.close(fieldString(params, "name"));
			return {};
		}
		if (cmd === "assert") {
			const subject = fieldObject(fieldObject(params, "spec"), "subject");
			if (subject["type"] === "tab") {
				await flow.tabs.assertClosed(fieldString(subject, "name"), timeoutMs);
				return {};
			}
		}
		const page = flow.tabs.current();
		switch (cmd) {
			case "response":
				await flow.network.capture(
					fieldString(params, "name"),
					fieldString(params, "method"),
					fieldString(params, "url"),
					page,
					timeoutMs,
				);
				return {};
			case "visit":
				// The flow's later lines wait for what they need (SPEC section 12),
				// so VISIT only needs a parsed document, not the `load` event that
				// images, fonts, and media can hold open.
				await page.goto(fieldString(params, "url"), {
					timeout: timeoutMs,
					waitUntil: "domcontentloaded",
				});
				return {};
			case "click": {
				const button = fieldEnum(params, "button", mouseButtons);
				await this.#locatorAction(
					page,
					params,
					async (locator, fromSnapshot) => {
						// Styled checkboxes and radios often cover the native input,
						// which makes a click wait out the step. Focus and Space have
						// the click's effect, as CHECK relies on (SPEC 7 and 7.4).
						if (
							button === "left" &&
							fromSnapshot &&
							(await isNativeToggle(locator, timeoutMs))
						) {
							await locator.press("Space", { timeout: timeoutMs });
							return;
						}
						await this.#click(
							page,
							locator,
							timeoutMs,
							button,
							fromSnapshot
								? await textTargetPosition(locator, timeoutMs)
								: undefined,
						);
					},
				);
				return {};
			}
			case "dblclick":
				await this.#locatorAction(
					page,
					params,
					async (locator, fromSnapshot) => {
						const position = fromSnapshot
							? await textTargetPosition(locator, timeoutMs)
							: undefined;
						await locator.dblclick({
							timeout: timeoutMs,
							...(position === undefined ? {} : { position }),
						});
					},
				);
				return {};
			case "fill": {
				const value = fieldString(params, "value");
				await this.#locatorAction(page, params, (locator) =>
					locator.fill(value, { timeout: timeoutMs }),
				);
				return {};
			}
			case "type": {
				const text = fieldString(params, "text");
				await this.#locatorAction(page, params, (locator) =>
					locator.pressSequentially(text, { timeout: timeoutMs }),
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
					this.#setChecked(page, locator, checked, timeoutMs),
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
				await this.#locatorAction(
					page,
					params,
					async (locator, fromSnapshot) => {
						const position = fromSnapshot
							? await textTargetPosition(locator, timeoutMs)
							: undefined;
						await locator.hover({
							timeout: timeoutMs,
							...(position === undefined ? {} : { position }),
						});
					},
				);
				return {};
			case "drag": {
				const target = this.#readLocator(page, params, "target");
				await this.#locatorAction(
					page,
					params,
					async (source, fromSnapshot) => {
						if (target.fromSnapshot && (await target.locator.count()) === 0) {
							throw new ShimError(
								"stale-ref",
								`the snapshot element ${target.description} is no longer on the page`,
							);
						}
						await this.#drag(page, source, fromSnapshot, target, timeoutMs);
					},
				);
				return {};
			}
			case "scroll": {
				const motion = decodeScrollMotion(params);
				if (motion.type === "intoView") {
					await this.#locatorAction(page, params, (locator) =>
						locator.scrollIntoViewIfNeeded({ timeout: timeoutMs }),
					);
					return {};
				}
				// Without a locator the page scrolls, which `scrollBox` reads
				// from the document element.
				if (fieldArrayOrNull(params, "locator") === null) {
					await page
						.locator(":root")
						.evaluate(scrollBox, motion, { timeout: timeoutMs });
					return {};
				}
				await this.#locatorAction(page, params, async (locator) => {
					// An iframe scrolls the page inside it. Entering the frame
					// works across origins, where the parent's script cannot.
					const isFrame = await locator.evaluate(
						(element) =>
							element.tagName === "IFRAME" || element.tagName === "FRAME",
						undefined,
						{ timeout: timeoutMs },
					);
					const box = isFrame
						? locator.contentFrame().locator(":root")
						: locator;
					await box.evaluate(scrollBox, motion, { timeout: timeoutMs });
				});
				return {};
			}
			case "upload": {
				const path = fieldString(params, "path");
				await this.#locatorAction(page, params, (locator) =>
					locator.setInputFiles(path, { timeout: timeoutMs }),
				);
				return {};
			}
			case "drop": {
				const path = fieldString(params, "path");
				await requireFile(path);
				await this.#locatorAction(
					page,
					params,
					(locator, _fromSnapshot, description) =>
						dropFile(locator, description, path, timeoutMs),
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
				const target = fieldArrayOrNull(params, "target") as
					| readonly LocatorSegment[]
					| null;
				const result = await runSnapshot(
					page,
					{
						...decodeSnapshotComparison(params),
						capture:
							target === null
								? { type: "page" }
								: {
										type: "element",
										locator: buildLocator(page, target),
										description: describeLocator(target),
										frames: frameOwners(page, target),
									},
						masks: fieldArray(params, "masks").map((segments) => {
							if (!Array.isArray(segments)) {
								throw new ShimError(
									"internal",
									"snapshot mask is not a locator array",
								);
							}
							return {
								locator: buildLocator(
									page,
									segments as readonly LocatorSegment[],
								),
								frames: frameOwners(
									page,
									segments as readonly LocatorSegment[],
								),
							};
						}),
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
			case "store": {
				const scope = fieldEnum(params, "scope", [
					"local",
					"session",
					"cookie",
				] as const);
				const key = fieldString(params, "key");
				const value = fieldString(params, "value");
				if (scope === "cookie") {
					const url = page.url();
					if (!/^https?:/.test(url)) {
						throw new ShimError(
							"action",
							`STORE cookie needs an http or https page; the current page is ${url}`,
						);
					}
					await page.context().addCookies([{ name: key, value, url }]);
					return {};
				}
				await page.evaluate(
					([storageScope, storageKey, storageValue]) => {
						const storage =
							storageScope === "session"
								? window.sessionStorage
								: window.localStorage;
						storage.setItem(storageKey, storageValue);
					},
					[scope, key, value] as const,
				);
				return {};
			}
			case "ariaSnapshot": {
				// ACT's view of the page (SPEC 7.4): element refs such as
				// [ref=e12] that a later `ref` locator segment resolves.
				const deadline = new Deadline(timeoutMs);
				let snapshot = "";
				if (fieldArrayOrNull(params, "locator") === null) {
					snapshot = await page.ariaSnapshot({
						mode: "ai",
						timeout: timeoutMs,
					});
				} else {
					// ACT limited to one element (SPEC 7.4): the scope waits
					// like any locator and must match exactly one element.
					await this.#locatorAction(page, params, async (locator) => {
						snapshot = await locator.ariaSnapshot({
							mode: "ai",
							timeout: timeoutMs,
						});
					});
				}
				return { snapshot: await nameIframes(page, snapshot, deadline) };
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
			case "read": {
				const subject = fieldObject(
					params,
					"subject",
				) as unknown as ReadSubject;
				return { ...(await runRead(page, subject, timeoutMs)) };
			}
			case "traceGroup":
			case "traceGroupEnd":
				// runStep handles these before dispatch.
				return {};
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

	#readLocator(page: Page, params: Params, key = "locator"): ReadLocator {
		const segments = fieldArray(params, key) as readonly LocatorSegment[];
		return {
			locator: buildLocator(page, segments),
			description: describeLocator(segments),
			fromSnapshot: segments.some((segment) => segment.type === "ref"),
		};
	}

	/**
	 * CHECK / UNCHECK (SPEC section 7). A native checkbox or radio input is
	 * focused and toggled with Space, which works whether the input is
	 * visible or hidden behind a styled switch (Chakra, Radix, and Headless
	 * UI all hide the input and would make a click time out). Any other
	 * control, such as a role=switch button, is clicked. Both paths verify
	 * the resulting state and are idempotent.
	 */
	async #setChecked(
		page: Page,
		locator: Locator,
		checked: boolean,
		timeoutMs: number,
	): Promise<void> {
		await locator.waitFor({ state: "attached", timeout: timeoutMs });
		const control = await locator.evaluate((element) => {
			const input = element as HTMLInputElement;
			const native =
				element.tagName === "INPUT" &&
				(input.type === "checkbox" || input.type === "radio");
			return {
				native,
				type: native ? input.type : null,
				checked: native ? input.checked : null,
				disabled: native ? input.disabled : false,
			};
		});
		if (!control.native) {
			await locator.setChecked(checked, { timeout: timeoutMs });
			return;
		}
		if (control.checked === checked) {
			return;
		}
		if (control.disabled) {
			throw new ShimError("action", "the control is disabled");
		}
		if (control.type === "radio" && !checked) {
			throw new ShimError(
				"action",
				"a radio button cannot be unchecked; check another one in its group",
			);
		}
		await locator.focus({ timeout: timeoutMs });
		const focused = await locator.evaluate(
			(element) => document.activeElement === element,
		);
		if (!focused) {
			throw new ShimError(
				"action",
				"the control cannot take focus (is it display: none?); locate the visible control instead",
			);
		}
		await page.keyboard.press("Space");
		try {
			await expect(locator).toBeChecked({ checked, timeout: timeoutMs });
		} catch {
			throw new ShimError(
				"action",
				`the control did not become ${checked ? "checked" : "unchecked"} after pressing Space`,
				{
					expected: checked ? "checked" : "unchecked",
					actual: checked ? "unchecked" : "checked",
				},
			);
		}
	}

	async #click(
		page: Page,
		locator: Locator,
		timeoutMs: number,
		button: MouseButton,
		position?: Point,
	): Promise<void> {
		const deadline = performance.now() + timeoutMs;
		const binding = this.#flow?.clickBinding;
		if (binding === undefined) {
			throw new ShimError("internal", "click without an open flow");
		}
		let receipt = this.#clickReceipts.get(page);
		if (receipt === undefined) {
			receipt = { next: 0, received: 0 };
			this.#clickReceipts.set(page, receipt);
		}
		const token = ++receipt.next;
		const eventType = clickEvents[button];
		const listener = await locator.evaluateHandle(
			(element, { binding, token, eventType }) => {
				const view = element.ownerDocument.defaultView as unknown as Record<
					string,
					(token: number) => Promise<void>
				>;
				const onClick = (event: Event): void => {
					if (!event.isTrusted) return;
					void view[binding]?.(token).catch(() => {
						// The click handler can destroy the document immediately.
					});
				};
				element.addEventListener(eventType, onClick, { capture: true });
				return () => element.removeEventListener(eventType, onClick, true);
			},
			{ binding, token, eventType },
			{ timeout: Math.max(1, deadline - performance.now()) },
		);
		try {
			await locator.click({
				button,
				timeout: Math.max(1, deadline - performance.now()),
				...(position === undefined ? {} : { position }),
			});
		} catch (error: unknown) {
			// Chromium can close a popup before acknowledging the mouse event.
			// Only accept that closure when the target received a trusted click.
			if (
				!(
					receipt.received === token &&
					page.isClosed() &&
					!this.#cancelRequested &&
					this.#browser?.isConnected() &&
					this.#flow?.context.pages().some((other) => !other.isClosed()) &&
					isTargetClosedError(error)
				)
			) {
				throw error;
			}
		} finally {
			try {
				await listener.evaluate((remove) => remove());
			} catch {
				// Navigation or closure can destroy the listener's document.
			}
			await listener.dispose();
		}
	}

	/**
	 * DRAG (SPEC section 7): press on the source, hold, wait for the target,
	 * move to it in steps, let the page see the last move, and release.
	 * Playwright's `dragTo` cannot hold. The button is released even when a
	 * step fails, so no later screenshot runs with it pressed.
	 */
	async #drag(
		page: Page,
		source: Locator,
		fromSnapshot: boolean,
		target: ReadLocator,
		timeoutMs: number,
	): Promise<void> {
		const deadline = performance.now() + timeoutMs;
		const remaining = (): number => Math.max(1, deadline - performance.now());
		const sourcePosition = fromSnapshot
			? await textTargetPosition(source, remaining())
			: undefined;
		await source.hover({
			timeout: remaining(),
			...(sourcePosition === undefined ? {} : { position: sourcePosition }),
		});
		await page.mouse.down();
		let released = false;
		try {
			await sleep(Math.min(dragHoldMs, remaining()));
			const point = await dropPoint(target, remaining);
			await page.mouse.move(point.x, point.y, { steps: dragSteps });
			await nextFrames(page);
			released = true;
			await page.mouse.up();
		} finally {
			if (!released) {
				await page.mouse.up().catch(() => {
					// The page can be gone after a failed step.
				});
			}
		}
	}

	async #locatorAction(
		page: Page,
		params: Params,
		action: (
			locator: Locator,
			fromSnapshot: boolean,
			description: string,
		) => Promise<unknown>,
	): Promise<void> {
		const { locator, description, fromSnapshot } = this.#readLocator(
			page,
			params,
		);
		// A snapshot ref names one element. When the page has replaced that
		// element since the snapshot, the ref never matches again, so waiting
		// for it would only spend the step's timeout (SPEC section 7.4).
		if (fromSnapshot && (await locator.count()) === 0) {
			throw new ShimError(
				"stale-ref",
				`the snapshot element ${description} is no longer on the page`,
			);
		}
		try {
			await action(locator, fromSnapshot, description);
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
			return new ShimError("timeout", actionErrorMessage(error));
		}
		const fallback: ErrorKind = defaultErrorKind(cmd);
		return new ShimError(fallback, actionErrorMessage(error));
	}

	async dispose(): Promise<void> {
		if (this.#flow !== null) {
			if (this.#flow.recorder !== null) {
				await this.#flow.recorder.abort();
			}
			await settlesWithin(this.#flow.context.close(), closeWatchdogMs);
			this.#flow = null;
		}
		// No graceful browser.close() here: the process exits right after
		// dispose, and Playwright's exit hooks kill the browser child.
		// Closing an idle browser hangs for seconds on macOS headless
		// shell, and its watchdog expiry ended in the same exit-hook kill.
		this.#browser = null;
		this.#browserKey = null;
	}
}

function defaultErrorKind(cmd: StepCommand): ErrorKind {
	switch (cmd) {
		case "http":
		case "response":
		case "popup":
		case "tab":
		case "close":
		case "visit":
		case "click":
		case "dblclick":
		case "fill":
		case "type":
		case "press":
		case "checkbox":
		case "selectOption":
		case "hover":
		case "drag":
		case "scroll":
		case "upload":
		case "drop":
		case "screenshot":
			return "action";
		case "evalAction":
			return "eval";
		case "store":
			return "action";
		case "ariaSnapshot":
			return "internal";
		case "snapshot":
		case "page":
		case "assert":
		case "traceGroup":
		case "traceGroupEnd":
			return "internal";
		case "read":
			// Page churn such as a navigation mid-read; Rust reads again.
			return "read";
		case "readResponse":
			return "action";
		default:
			return assertNever(cmd);
	}
}
