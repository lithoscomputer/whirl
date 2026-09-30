import {
	mkdir,
	mkdtemp,
	readFile,
	realpath,
	rename,
	rm,
	stat,
	writeFile,
} from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import type { BrowserContext, Page, Route } from "@playwright/test";
import type { Database } from "./database.js";
import { databaseRecord, databaseState } from "./database.js";
import type { SharedState } from "./format.js";
import { checkStateCapabilities, parseState } from "./format.js";
import { storageEntries } from "./storage.js";

export async function readState(path: string): Promise<SharedState> {
	let value: unknown;
	try {
		value = JSON.parse(await readFile(path, "utf8"));
	} catch {
		throw new Error("Cannot read saved state JSON; check --load-state");
	}
	const state = parseState(value);
	checkStateCapabilities(state);
	return state;
}

async function bootstrap<T>(
	page: Page,
	origin: string,
	action: () => Promise<T>,
): Promise<T> {
	const url = `${origin}/__whirl_state_${crypto.randomUUID()}`;
	const handler = async (route: Route): Promise<void> => {
		await route.fulfill({
			status: 200,
			contentType: "text/html",
			body: "<!doctype html>",
		});
	};
	const cdp =
		page.context().browser()?.browserType().name() === "chromium"
			? await page.context().newCDPSession(page)
			: null;
	await page.route(url, handler);
	if (cdp !== null) {
		await cdp.send("Network.enable");
		await cdp.send("Network.setBypassServiceWorker", { bypass: true });
	}
	try {
		await page.goto(url);
		return await action();
	} finally {
		await page.unroute(url, handler);
		if (cdp !== null) {
			await cdp.send("Network.setBypassServiceWorker", { bypass: false });
			await cdp.detach();
		}
	}
}

const databaseScript = (seed?: readonly Database[]): string =>
	`(${databaseState.toString()})(${seed === undefined ? "undefined" : JSON.stringify(seed)},(${databaseRecord.toString()}))`;

/** A fresh context owns all state. Bootstrap pages never contact application servers. */
export async function restoreState(
	context: BrowserContext,
	main: Page,
	state: SharedState,
): Promise<number> {
	const now = Date.now() / 1000;
	const cookies = state.cookies.filter(
		(cookie) => cookie.expires === null || cookie.expires > now,
	);
	const expired = state.cookies.length - cookies.length;
	await context.addCookies(
		cookies.map((cookie) => ({
			name: cookie.name,
			value: cookie.value,
			domain: `${cookie.hostOnly ? "" : "."}${cookie.domain}`,
			path: cookie.path,
			expires: cookie.expires ?? -1,
			httpOnly: cookie.httpOnly,
			secure: cookie.secure,
			...(cookie.sameSite === null ? {} : { sameSite: cookie.sameSite }),
		})),
	);
	for (const scope of state.origins) {
		await bootstrap(main, scope.origin, async () => {
			if (scope.localStorage !== undefined)
				await main.evaluate((items) => {
					localStorage.clear();
					for (const item of items) localStorage.setItem(item.name, item.value);
				}, scope.localStorage);
			if (scope.indexedDB !== undefined)
				await main.evaluate(databaseScript(scope.indexedDB));
		});
	}
	for (const scope of state.pages?.find((page) => page.id === "main")
		?.origins ?? []) {
		if (scope.sessionStorage !== undefined)
			await bootstrap(main, scope.origin, async () => {
				await main.evaluate((items) => {
					sessionStorage.clear();
					for (const item of items ?? [])
						sessionStorage.setItem(item.name, item.value);
				}, scope.sessionStorage);
			});
	}
	await main.goto("about:blank");
	return expired;
}

/** Capture each local/IndexedDB origin plus the main page's visited session origins. */
export async function captureState(
	context: BrowserContext,
	main: Page,
	visited: ReadonlySet<string>,
	previous?: SharedState,
	storageOrigins: ReadonlySet<string> = visited,
): Promise<SharedState> {
	const native = { cookies: await context.cookies() };
	const origins: SharedState["origins"] = [];
	const page = await context.newPage();
	try {
		for (const origin of new Set([
			...storageOrigins,
			...(previous?.origins.map((scope) => scope.origin) ?? []),
		])) {
			origins.push(
				await bootstrap(page, origin, async () => ({
					origin,
					localStorage: await page.evaluate<
						{ name: string; value: string }[],
						"local" | "session"
					>(storageEntries, "local"),
					indexedDB: await page.evaluate<Database[]>(databaseScript()),
				})),
			);
		}
	} finally {
		await page.close();
	}
	const sessions: NonNullable<SharedState["pages"]>[number]["origins"] = [];
	// A new auxiliary page gets a native copy of its opener's session namespaces.
	// Navigate this copy, so export does not navigate the app or reset its state.
	const current =
		!main.isClosed() && /^https?:/.test(main.url())
			? new URL(main.url()).origin
			: null;
	if (
		visited.size === 1 &&
		current !== null &&
		visited.has(current) &&
		context.browser()?.browserType().name() === "chromium"
	) {
		const cdp = await context.newCDPSession(main);
		try {
			const data = await cdp.send("DOMStorage.getDOMStorageItems", {
				storageId: { securityOrigin: current, isLocalStorage: false },
			});
			sessions.push({
				origin: current,
				sessionStorage: data.entries.map(([name, value]) => ({
					name: name ?? "",
					value: value ?? "",
				})),
			});
		} finally {
			await cdp.detach();
		}
	} else if (!main.isClosed() && visited.size > 0) {
		const open = async (): Promise<void> => {
			if (context.browser()?.browserType().name() === "chromium") {
				const cdp = await context.newCDPSession(main);
				try {
					const { frameTree } = await cdp.send("Page.getFrameTree");
					const world = await cdp.send("Page.createIsolatedWorld", {
						frameId: frameTree.frame.id,
						worldName: "whirl-state-export",
					});
					await cdp.send("Runtime.evaluate", {
						expression: "window.open('about:blank', '_blank'); void 0",
						contextId: world.executionContextId,
						userGesture: true,
					});
				} finally {
					await cdp.detach();
				}
			} else
				await main.evaluate(() => {
					window.open("about:blank", "_blank");
				});
		};
		const [copy] = await Promise.all([
			main.waitForEvent("popup", { timeout: 5000 }),
			open(),
		]);
		try {
			for (const origin of visited)
				sessions.push({
					origin,
					sessionStorage: await bootstrap(
						copy,
						origin,
						async () =>
							await copy.evaluate<
								{ name: string; value: string }[],
								"local" | "session"
							>(storageEntries, "session"),
					),
				});
		} finally {
			await copy.close();
		}
	}
	let cookies: SharedState["cookies"] = native.cookies.map((cookie) => ({
		name: cookie.name,
		value: cookie.value,
		domain: cookie.domain.replace(/^\./, "").toLowerCase(),
		hostOnly: !cookie.domain.startsWith("."),
		path: cookie.path,
		expires: cookie.expires < 0 ? null : cookie.expires,
		httpOnly: cookie.httpOnly,
		secure: cookie.secure,
		sameSite: cookie.sameSite,
	}));
	if (context.browser()?.browserType().name() === "chromium") {
		const cdp = await context.newCDPSession(
			page.isClosed() ? await context.newPage() : page,
		);
		try {
			const result = await cdp.send("Network.getAllCookies");
			if (
				result.cookies.some(
					(cookie) =>
						cookie.partitionKey !== undefined || cookie.partitionKeyOpaque,
				)
			)
				throw new Error(
					"Partitioned cookies are not supported by this state adapter",
				);
			cookies = result.cookies.map((cookie) => ({
				name: cookie.name,
				value: cookie.value,
				domain: cookie.domain.replace(/^\./, "").toLowerCase(),
				hostOnly: !cookie.domain.startsWith("."),
				path: cookie.path,
				expires: cookie.session ? null : cookie.expires,
				httpOnly: cookie.httpOnly,
				secure: cookie.secure,
				sameSite: cookie.sameSite ?? null,
			}));
		} finally {
			await cdp.detach();
		}
	} else if (native.cookies.some((cookie) => "partitionKey" in cookie))
		throw new Error(
			"Partitioned cookies are not supported by this state adapter",
		);
	return parseState({
		format: "whirl-state",
		version: 1,
		redacted: previous?.redacted ?? false,
		cookies,
		origins,
		pages: [{ id: "main", origins: sessions }],
		...(previous?.metadata === undefined
			? {}
			: { metadata: previous.metadata }),
	});
}

/** Publish only complete files, with owner-only permissions. Refuse input aliases. */
export async function writeState(
	path: string,
	state: SharedState,
	inputs: readonly string[],
): Promise<void> {
	await assertStateOutput(path, inputs);
	await mkdir(dirname(path), { recursive: true });
	const directory = await mkdtemp(join(dirname(path), ".whirl-state-"));
	try {
		const staged = join(directory, "state.json");
		await writeFile(staged, `${JSON.stringify(parseState(state), null, 2)}\n`, {
			mode: 0o600,
		});
		await rename(staged, path);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
}

export async function assertStateOutput(
	path: string,
	inputs: readonly string[],
): Promise<void> {
	const destination = await realpath(path).catch(() => resolve(path));
	const info = await stat(path).catch(() => null);
	for (const input of inputs) {
		const source = await realpath(input).catch(() => resolve(input));
		const sourceInfo = await stat(input).catch(() => null);
		if (
			destination === source ||
			(info !== null &&
				sourceInfo !== null &&
				info.dev === sourceInfo.dev &&
				info.ino === sourceInfo.ino)
		)
			throw new Error("Saved state output would overwrite an input");
	}
}
