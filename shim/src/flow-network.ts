import type { BrowserContext, Page, Request, Response } from "@playwright/test";
import type { BlockedHost, BlockedHostLog } from "./host-glob.js";
import { describeBlockedHost } from "./host-glob.js";
import type { HttpParams, RequestRead, ResponseRead } from "./protocol.js";
import { ShimError } from "./protocol.js";
import { Deadline, pollUntilPass, shortErrorMessage } from "./step-util.js";

const maxRequestsPerEntry = 10_000;
const maxBodyBytes = 1_048_576;
type NamedResponse = Pick<
	Response,
	"status" | "headerValue" | "body" | "url" | "headersArray"
>;

async function withinTimeout<T>(
	operation: Promise<T>,
	timeoutMs: number,
): Promise<T> {
	let timer: NodeJS.Timeout | undefined;
	const timeout = new Promise<never>((_resolve, reject) => {
		timer = setTimeout(
			() =>
				reject(
					new ShimError(
						"timeout",
						`response did not become available within ${String(timeoutMs)}ms`,
					),
				),
			timeoutMs,
		);
	});
	try {
		return await Promise.race([operation, timeout]);
	} finally {
		clearTimeout(timer);
	}
}

function belongsToPage(request: Request, page: Page): boolean {
	try {
		return request.frame().page() === page;
	} catch {
		// Service-worker requests have no frame; a popup's initial frame may not exist yet.
		return false;
	}
}

/** Records requests at context level, including popup navigation before the page event. */
export class FlowNetwork {
	readonly #context: BrowserContext;
	readonly #responses = new Map<string, NamedResponse>();
	/** The request each `RESPONSE` name selected (protocol 4.7). */
	readonly #selectedRequests = new Map<string, Request>();
	/** Names whose response came from an `HTTP` entry, with exact bytes. */
	readonly #httpNames = new Set<string>();
	readonly #bodies = new Map<NamedResponse, Promise<Buffer>>();
	private readonly httpRequests = new Set<AbortController>();
	private readonly hostPolicy: (hostname: string) => BlockedHost | null;
	private readonly blockedHosts: BlockedHostLog;
	#requests: Request[] = [];
	#overflow = false;

	constructor(
		context: BrowserContext,
		hostPolicy: (hostname: string) => BlockedHost | null,
		blockedHosts: BlockedHostLog,
	) {
		this.#context = context;
		this.hostPolicy = hostPolicy;
		this.blockedHosts = blockedHosts;
		context.on("request", this.#onRequest);
		context.once("close", () => {
			for (const controller of this.httpRequests) controller.abort();
			this.httpRequests.clear();
			context.off("request", this.#onRequest);
			this.#requests = [];
			this.#responses.clear();
			this.#selectedRequests.clear();
			this.#httpNames.clear();
			this.#bodies.clear();
		});
	}

	stopCollecting(): void {
		this.#context.off("request", this.#onRequest);
	}

	readonly #onRequest = (request: Request): void => {
		if (this.#requests.length >= maxRequestsPerEntry) {
			this.#overflow = true;
			return;
		}
		this.#requests.push(request);
	};

	beginEntry(): void {
		this.#requests = [];
		this.#overflow = false;
	}

	async http(
		{ name, method, url, headers, body }: HttpParams,
		timeoutMs: number,
	): Promise<void> {
		if (this.#responses.has(name))
			throw new ShimError("action", `response ${name} is already named`);
		let target: URL;
		try {
			target = new URL(url);
		} catch {
			throw new ShimError(
				"action",
				"HTTP needs an absolute HTTP URL or a path with app-url",
			);
		}
		if (
			!/^https?:$/.test(target.protocol) ||
			target.username !== "" ||
			target.password !== ""
		) {
			throw new ShimError(
				"action",
				"HTTP needs an HTTP or HTTPS URL without embedded credentials",
			);
		}
		const blocked = this.hostPolicy(target.hostname);
		if (blocked !== null) {
			this.blockedHosts.record(blocked);
			throw new ShimError(
				"action",
				`HTTP host ${target.hostname} is blocked by ${describeBlockedHost(blocked)}`,
			);
		}
		target.hash = "";
		const controller = new AbortController();
		this.httpRequests.add(controller);
		const signal = AbortSignal.any([
			controller.signal,
			AbortSignal.timeout(timeoutMs),
		]);
		try {
			const response = await fetch(target, {
				method,
				headers: Object.fromEntries(headers),
				...(body === null ? {} : { body }),
				redirect: "manual",
				signal,
			});
			if (
				response.body !== null &&
				Number(response.headers.get("content-length")) > maxBodyBytes
			) {
				throw new ShimError(
					"action",
					"HTTP response exceeds the 1 MiB body limit",
				);
			}
			const chunks: Uint8Array[] = [];
			let length = 0;
			if (response.body !== null) {
				for await (const chunk of response.body) {
					length += chunk.length;
					if (length > maxBodyBytes)
						throw new ShimError(
							"action",
							"HTTP response exceeds the 1 MiB body limit",
						);
					chunks.push(chunk);
				}
			}
			const bytes = Buffer.concat(chunks, length);
			const responseHeaders = [...response.headers].map(([header, value]) => ({
				name: header,
				value,
			}));
			this.#httpNames.add(name);
			this.#responses.set(name, {
				status: () => response.status,
				headerValue: async (header) => response.headers.get(header),
				body: async () => bytes,
				url: () => target.href,
				headersArray: async () => responseHeaders,
			});
		} catch (error) {
			if (error instanceof ShimError) throw error;
			if (signal.aborted)
				throw new ShimError(
					"timeout",
					`HTTP response ${name} exceeded its ${String(timeoutMs)}ms timeout or was cancelled`,
				);
			throw new ShimError(
				"action",
				`HTTP request ${name} failed: ${shortErrorMessage(error)}`,
			);
		} finally {
			controller.abort();
			this.httpRequests.delete(controller);
		}
	}

	async capture(
		name: string,
		method: string,
		url: string,
		page: Page,
		timeoutMs: number,
	): Promise<void> {
		if (this.#responses.has(name))
			throw new ShimError("action", `response ${name} is already named`);
		let normalizedUrl: URL;
		try {
			normalizedUrl = new URL(url);
		} catch {
			throw new ShimError(
				"action",
				"RESPONSE needs an absolute HTTP URL or a path with app-url",
			);
		}
		if (!/^https?:$/.test(normalizedUrl.protocol))
			throw new ShimError(
				"action",
				"RESPONSE only observes HTTP and HTTPS requests",
			);
		normalizedUrl.hash = "";
		const expectedUrl = normalizedUrl.href;
		const deadline = new Deadline(timeoutMs);
		let selected: Request | undefined;
		await pollUntilPass(
			deadline.remainingMs(),
			async () => {
				if (this.#overflow)
					throw new ShimError(
						"action",
						`more than ${String(maxRequestsPerEntry)} requests in this entry; split the flow into shorter entries`,
					);
				if (
					this.#context.browser()?.isConnected() === false ||
					page.isClosed()
				) {
					throw new ShimError(
						"action",
						"the window closed before its response could be selected",
					);
				}
				selected = this.#requests.find(
					(request) =>
						request.method() === method &&
						request.url() === expectedUrl &&
						belongsToPage(request, page),
				);
				return {
					pass: selected !== undefined,
					actual: `no ${method} ${expectedUrl} request from the selected window in this entry`,
				};
			},
			{
				kind: "timeout",
				message: `no matching request for response ${name} within ${String(timeoutMs)}ms`,
			},
		);
		if (selected === undefined)
			throw new ShimError(
				"internal",
				"a passing request match must select a request",
			);
		const response = await withinTimeout(
			selected.response(),
			deadline.remainingMs(),
		);
		if (response === null) {
			throw new ShimError(
				"action",
				`request for response ${name} failed: ${selected.failure()?.errorText ?? "no HTTP response"}`,
			);
		}
		this.#responses.set(name, response);
		this.#selectedRequests.set(name, selected);
	}

	/**
	 * Reads the request that a `RESPONSE` name selected (protocol 4.7): its
	 * method, URL without a fragment, the headers the browser sent, and its
	 * body within the 1 MiB limit.
	 */
	async readRequest(name: string, timeoutMs: number): Promise<RequestRead> {
		const request = this.#selectedRequests.get(name);
		if (request === undefined)
			throw new ShimError("internal", `unknown request ${name}`);
		const headers = await withinTimeout(request.headersArray(), timeoutMs);
		const url = new URL(request.url());
		url.hash = "";
		const base = {
			method: request.method(),
			url: url.href,
			headers: headers.map(
				({ name: header, value }) => [header, value] as const,
			),
		};
		const body = request.postDataBuffer() ?? Buffer.alloc(0);
		if (body.length > maxBodyBytes) {
			return {
				...base,
				bodyBase64: null,
				bodyError: "the request exceeds the 1 MiB body limit",
			};
		}
		return { ...base, bodyBase64: body.toString("base64"), bodyError: null };
	}

	/** The body within the SPEC 1 MiB limit, read once per response. */
	#body(response: NamedResponse): Promise<Buffer> {
		let body = this.#bodies.get(response);
		if (body === undefined) {
			body = (async (): Promise<Buffer> => {
				const declaredLength = await response.headerValue("content-length");
				if (declaredLength !== null && Number(declaredLength) > maxBodyBytes)
					throw new Error("the response exceeds the 1 MiB body limit");
				const buffer = await response.body();
				if (buffer.length > maxBodyBytes)
					throw new Error("the response exceeds the 1 MiB body limit");
				return buffer;
			})();
			this.#bodies.set(response, body);
		}
		return body;
	}

	/**
	 * True when the browser may have handed the body back as decoded text,
	 * re-encoded as UTF-8: Chromium and WebKit do this for some text types.
	 * Firefox and `HTTP` entries give the exact bytes (protocol 4.5).
	 */
	#bodyMayBeDecoded(name: string): boolean {
		if (this.#httpNames.has(name)) return false;
		const engine = this.#context.browser()?.browserType().name();
		return engine !== "firefox";
	}

	/**
	 * Reads a named response for Rust's check engine (protocol 4.5). With
	 * `withBody`, the body is read too; a body that cannot be read, such as
	 * a redirect's or one over the limit, comes back as `bodyError`.
	 */
	async readResponse(
		name: string,
		withBody: boolean,
		timeoutMs: number,
	): Promise<ResponseRead> {
		const response = this.#responses.get(name);
		if (response === undefined)
			throw new ShimError("internal", `unknown response ${name}`);
		const deadline = new Deadline(timeoutMs);
		const headers = await withinTimeout(
			response.headersArray(),
			deadline.remainingMs(),
		);
		const base = {
			status: response.status(),
			url: response.url(),
			headers: headers.map(
				({ name: header, value }) => [header, value] as const,
			),
			bodyMayBeDecoded: this.#bodyMayBeDecoded(name),
		};
		if (!withBody) {
			return { ...base, bodyBase64: null, bodyError: null };
		}
		try {
			const body = await withinTimeout(
				this.#body(response),
				deadline.remainingMs(),
			);
			return { ...base, bodyBase64: body.toString("base64"), bodyError: null };
		} catch (error) {
			if (error instanceof ShimError) throw error;
			return { ...base, bodyBase64: null, bodyError: shortErrorMessage(error) };
		}
	}
}
