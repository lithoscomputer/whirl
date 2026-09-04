import type { BrowserContext, Page, Request, Response } from "@playwright/test";
import { createHostAllowlist } from "./host-glob.js";
import type { CountOp, HttpParams, StringOp } from "./protocol.js";
import { assertNever, ShimError } from "./protocol.js";
import { Deadline, pollUntilPass, shortErrorMessage } from "./step-util.js";

export type ResponseField =
	| { readonly type: "status" }
	| { readonly type: "header"; readonly name: string }
	| { readonly type: "json"; readonly pointer: string };

export type ResponseCheck =
	| { readonly type: "status"; readonly op: CountOp; readonly value: number }
	| {
			readonly type: "value";
			readonly field: ResponseField;
			readonly op: StringOp;
	  };

const maxRequestsPerEntry = 10_000;
const maxJsonBodyBytes = 1_048_576;
type NamedResponse = Pick<Response, "status" | "headerValue" | "body">;

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

function matchesNumber(actual: number, op: CountOp, expected: number): boolean {
	switch (op) {
		case "==":
			return actual === expected;
		case "!=":
			return actual !== expected;
		case "<":
			return actual < expected;
		case "<=":
			return actual <= expected;
		case ">":
			return actual > expected;
		case ">=":
			return actual >= expected;
		default:
			return assertNever(op);
	}
}

function matchesString(actual: string, op: StringOp): boolean {
	switch (op.op) {
		case "==":
			return actual === op.value;
		case "!=":
			return actual !== op.value;
		case "contains":
			return actual.includes(op.value);
		case "matches":
			return new RegExp(op.source, op.flags).test(actual);
		default:
			return assertNever(op);
	}
}

function jsonPointerValue(json: unknown, pointer: string): unknown {
	if (
		(pointer !== "" && !pointer.startsWith("/")) ||
		/~(?![01])/.test(pointer)
	) {
		throw new Error("invalid JSON Pointer");
	}
	if (pointer === "") return json;
	let value = json;
	for (const segment of pointer.slice(1).split("/")) {
		const key = segment.replace(/~1/g, "/").replace(/~0/g, "~");
		if (Array.isArray(value)) {
			if (!/^(0|[1-9]\d*)$/.test(key) || Number(key) >= value.length) {
				throw new Error(`JSON Pointer ${pointer} does not exist`);
			}
			value = (value as readonly unknown[])[Number(key)];
		} else if (
			typeof value === "object" &&
			value !== null &&
			Object.hasOwn(value, key)
		) {
			value = (value as Record<string, unknown>)[key];
		} else {
			throw new Error(`JSON Pointer ${pointer} does not exist`);
		}
	}
	return value;
}

/** Records requests at context level, including popup navigation before the page event. */
export class FlowNetwork {
	readonly #context: BrowserContext;
	readonly #responses = new Map<string, NamedResponse>();
	readonly #jsonBodies = new Map<NamedResponse, Promise<unknown>>();
	private readonly httpRequests = new Set<AbortController>();
	private readonly allowedHost: (hostname: string) => boolean;
	private readonly blockedHosts: Set<string>;
	#requests: Request[] = [];
	#overflow = false;

	constructor(
		context: BrowserContext,
		allowHosts: readonly string[] | null,
		blockedHosts: Set<string>,
	) {
		this.#context = context;
		this.allowedHost =
			allowHosts === null ? () => true : createHostAllowlist(allowHosts);
		this.blockedHosts = blockedHosts;
		context.on("request", this.#onRequest);
		context.once("close", () => {
			for (const controller of this.httpRequests) controller.abort();
			this.httpRequests.clear();
			context.off("request", this.#onRequest);
			this.#requests = [];
			this.#responses.clear();
			this.#jsonBodies.clear();
		});
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
				"HTTP needs an absolute HTTP URL or a path with base",
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
		if (!this.allowedHost(target.hostname)) {
			this.blockedHosts.add(target.hostname);
			throw new ShimError(
				"action",
				`HTTP host ${target.hostname} is blocked by allow-hosts`,
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
				Number(response.headers.get("content-length")) > maxJsonBodyBytes
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
					if (length > maxJsonBodyBytes)
						throw new ShimError(
							"action",
							"HTTP response exceeds the 1 MiB body limit",
						);
					chunks.push(chunk);
				}
			}
			const bytes = Buffer.concat(chunks, length);
			this.#responses.set(name, {
				status: () => response.status,
				headerValue: async (header) => response.headers.get(header),
				body: async () => bytes,
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
				"RESPONSE needs an absolute HTTP URL or a path with base",
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
						"the tab closed before its response could be selected",
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
					actual: `no ${method} ${expectedUrl} request from the selected tab in this entry`,
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
	}

	#named(name: string): NamedResponse {
		const response = this.#responses.get(name);
		if (response === undefined)
			throw new ShimError("action", `unknown response ${name}`);
		return response;
	}

	async #json(response: NamedResponse): Promise<unknown> {
		let body = this.#jsonBodies.get(response);
		if (body === undefined) {
			body = (async (): Promise<unknown> => {
				const declaredLength = await response.headerValue("content-length");
				if (
					declaredLength !== null &&
					Number(declaredLength) > maxJsonBodyBytes
				)
					throw new Error("JSON response exceeds the 1 MiB body limit");
				const buffer = await response.body();
				if (buffer.length > maxJsonBodyBytes)
					throw new Error("JSON response exceeds the 1 MiB body limit");
				return JSON.parse(buffer.toString("utf8")) as unknown;
			})();
			this.#jsonBodies.set(response, body);
		}
		return body;
	}

	async #read(name: string, field: ResponseField): Promise<string> {
		const response = this.#named(name);
		switch (field.type) {
			case "status":
				return String(response.status());
			case "header": {
				if (!/^[A-Za-z_][A-Za-z0-9_-]*$/.test(field.name))
					throw new Error("invalid response header name");
				const header = await response.headerValue(field.name);
				if (header === null)
					throw new Error(`response header ${field.name} is absent`);
				return header;
			}
			case "json": {
				const value = jsonPointerValue(
					await this.#json(response),
					field.pointer,
				);
				const text = typeof value === "string" ? value : JSON.stringify(value);
				if (text === undefined)
					throw new Error("JSON Pointer did not resolve to a JSON value");
				return text;
			}
			default:
				return assertNever(field);
		}
	}

	async read(
		name: string,
		field: ResponseField,
		timeoutMs: number,
		kind: "assert" | "capture" = "capture",
	): Promise<string> {
		try {
			return await withinTimeout(this.#read(name, field), timeoutMs);
		} catch (error) {
			if (error instanceof ShimError) throw error;
			throw new ShimError(
				kind,
				`response ${name}: ${shortErrorMessage(error)}`,
			);
		}
	}

	async assert(
		name: string,
		check: ResponseCheck,
		timeoutMs: number,
	): Promise<void> {
		if (check.type === "status") {
			const actual = this.#named(name).status();
			if (!matchesNumber(actual, check.op, check.value)) {
				throw new ShimError("assert", `response ${name} status did not match`, {
					expected: `${check.op} ${String(check.value)}`,
					actual: String(actual),
				});
			}
			return;
		}
		const actual = await this.read(name, check.field, timeoutMs, "assert");
		if (!matchesString(actual, check.op)) {
			throw new ShimError("assert", `response ${name} field did not match`, {
				expected:
					check.op.op === "matches"
						? `matches /${check.op.source}/${check.op.flags}`
						: `${check.op.op} ${JSON.stringify(check.op.value)}`,
				actual,
			});
		}
	}
}
