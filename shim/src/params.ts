// Light request-parameter readers. Rust owns the other side of the wire and
// guarantees the shapes (docs/engineering/shim-protocol.md), so these check
// the top-level fields a handler touches and answer a malformed request
// with kind "internal" instead of crashing.

import type {
	BrowserEngine,
	EndFlowParams,
	HttpParams,
	StartFlowParams,
} from "./protocol.js";
import { ShimError } from "./protocol.js";

export type Params = Record<string, unknown>;

function malformed(key: string, expected: string): ShimError {
	return new ShimError(
		"internal",
		`malformed request: param "${key}" is not ${expected}`,
	);
}

export function asParams(value: unknown): Params {
	if (typeof value !== "object" || value === null || Array.isArray(value)) {
		throw new ShimError(
			"internal",
			"malformed request: params is not an object",
		);
	}
	return value as Params;
}

export function fieldString(params: Params, key: string): string {
	const value = params[key];
	if (typeof value !== "string") {
		throw malformed(key, "a string");
	}
	return value;
}

export function fieldStringOrNull(params: Params, key: string): string | null {
	const value = params[key];
	if (value === null || value === undefined) {
		return null;
	}
	if (typeof value !== "string") {
		throw malformed(key, "a string or null");
	}
	return value;
}

export function fieldNumber(params: Params, key: string): number {
	const value = params[key];
	if (typeof value !== "number" || !Number.isFinite(value)) {
		throw malformed(key, "a number");
	}
	return value;
}

export function fieldBoolean(params: Params, key: string): boolean {
	const value = params[key];
	if (typeof value !== "boolean") {
		throw malformed(key, "a boolean");
	}
	return value;
}

export function fieldObject(params: Params, key: string): Params {
	const value = params[key];
	if (typeof value !== "object" || value === null || Array.isArray(value)) {
		throw malformed(key, "an object");
	}
	return value as Params;
}

export function fieldObjectOrNull(params: Params, key: string): Params | null {
	const value = params[key];
	if (value === null || value === undefined) {
		return null;
	}
	return fieldObject(params, key);
}

export function fieldArray(params: Params, key: string): readonly unknown[] {
	const value = params[key];
	if (!Array.isArray(value)) {
		throw malformed(key, "an array");
	}
	return value;
}

export function fieldArrayOrNull(
	params: Params,
	key: string,
): readonly unknown[] | null {
	const value = params[key];
	if (value === null || value === undefined) {
		return null;
	}
	return fieldArray(params, key);
}

export function decodeHttpParams(params: Params): HttpParams {
	const headers = fieldArray(params, "headers").map((pair) => {
		if (
			!Array.isArray(pair) ||
			pair.length !== 2 ||
			typeof pair[0] !== "string" ||
			typeof pair[1] !== "string"
		) {
			throw malformed("headers", "an array of string pairs");
		}
		return [pair[0], pair[1]] as const;
	});
	return {
		name: fieldString(params, "name"),
		method: fieldString(params, "method"),
		url: fieldString(params, "url"),
		headers,
		body: fieldStringOrNull(params, "body"),
	};
}

export function fieldEnum<T extends string>(
	params: Params,
	key: string,
	values: readonly T[],
): T {
	const value = params[key];
	if (
		typeof value !== "string" ||
		!(values as readonly string[]).includes(value)
	) {
		throw malformed(key, `one of ${values.join(", ")}`);
	}
	return value as T;
}

const browserEngines: readonly BrowserEngine[] = [
	"chromium",
	"firefox",
	"webkit",
];

/** Decodes startFlow params (protocol section 3). */
export function decodeStartFlowParams(params: Params): StartFlowParams {
	const viewport = fieldObject(params, "viewport");
	const video = fieldObjectOrNull(params, "video");
	const allowHosts = fieldArrayOrNull(params, "allowHosts");
	return {
		browser: fieldEnum(params, "browser", browserEngines),
		headed: fieldBoolean(params, "headed"),
		viewport: {
			width: fieldNumber(viewport, "width"),
			height: fieldNumber(viewport, "height"),
		},
		storageStatePath: fieldStringOrNull(params, "storageStatePath"),
		dialogs: fieldEnum(params, "dialogs", ["dismiss", "accept"]),
		allowHosts:
			allowHosts === null ? null : allowHosts.map((host) => String(host)),
		navTimeoutMs: fieldNumber(params, "navTimeoutMs"),
		userAgent: fieldStringOrNull(params, "userAgent"),
		reducedMotion:
			fieldStringOrNull(params, "reducedMotion") === null
				? null
				: fieldEnum(params, "reducedMotion", [
						"reduce",
						"no-preference",
					] as const),
		video:
			video === null
				? null
				: {
						tempDir: fieldString(video, "tempDir"),
						finalPath: fieldString(video, "finalPath"),
						fps:
							video["fps"] === null || video["fps"] === undefined
								? null
								: fieldNumber(video, "fps"),
					},
		harPath: fieldStringOrNull(params, "harPath"),
		trace: fieldBoolean(params, "trace"),
	};
}

/** Decodes endFlow params (protocol section 3). */
export function decodeEndFlowParams(params: Params): EndFlowParams {
	return {
		saveStoragePath: fieldStringOrNull(params, "saveStoragePath"),
		tracePath: fieldStringOrNull(params, "tracePath"),
	};
}
