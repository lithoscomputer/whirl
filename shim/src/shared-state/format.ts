import { z } from "zod";
import { databaseRecord } from "./database.js";

const origin = z.string().refine((value) => {
	try {
		const url = new URL(value);
		return ["http:", "https:"].includes(url.protocol) && url.origin === value;
	} catch {
		return false;
	}
}, "Expected a canonical HTTP(S) origin");
const path = z
	.array(z.union([z.string(), z.number().int().nonnegative()]))
	.min(1);
const keyPath = z.union([z.string(), z.array(z.string()).min(1)]);
const namedValue = z.strictObject({ name: z.string(), value: z.string() });
const binary = z.strictObject({
	path,
	type: z.string(),
	object: z.number().int().nonnegative(),
	buffer: z.number().int().nonnegative(),
	bufferLength: z.number().int().nonnegative(),
	byteOffset: z.number().int().nonnegative(),
	byteLength: z.number().int().nonnegative(),
});
const record = z.strictObject({
	key: z.unknown(),
	value: z.unknown(),
	binary: z.array(binary).optional(),
	undefinedPaths: z.array(path).optional(),
});
const database = z.strictObject({
	name: z.string(),
	version: z.number().int().positive(),
	stores: z.array(
		z.strictObject({
			name: z.string(),
			keyPath: keyPath.nullable(),
			autoIncrement: z.boolean(),
			nextKey: z
				.number()
				.refine(Number.isInteger)
				.min(1)
				.max(2 ** 53)
				.optional(),
			indexes: z.array(
				z.strictObject({
					name: z.string(),
					keyPath,
					unique: z.boolean(),
					multiEntry: z.boolean(),
				}),
			),
			records: z.array(record),
		}),
	),
});
export const stateSchema = z.strictObject({
	format: z.literal("whirl-state"),
	version: z.literal(1),
	redacted: z.boolean(),
	cookies: z.array(
		z.strictObject({
			name: z.string(),
			value: z.string(),
			domain: z
				.string()
				.min(1)
				.refine(
					(value) =>
						value === value.toLowerCase() &&
						!value.startsWith(".") &&
						!/[\s/@?#]/.test(value),
				),
			hostOnly: z.boolean(),
			path: z.string().startsWith("/"),
			expires: z.number().finite().nullable(),
			httpOnly: z.boolean(),
			secure: z.boolean(),
			sameSite: z.enum(["Strict", "Lax", "None"]).nullable(),
			partition: z
				.strictObject({
					topLevelSite: origin,
					hasCrossSiteAncestor: z.boolean(),
				})
				.optional(),
		}),
	),
	origins: z.array(
		z.strictObject({
			origin,
			localStorage: z.array(namedValue).optional(),
			indexedDB: z.array(database).optional(),
		}),
	),
	pages: z
		.array(
			z.strictObject({
				id: z.string().min(1),
				origins: z.array(
					z.strictObject({
						origin,
						sessionStorage: z.array(namedValue).optional(),
					}),
				),
			}),
		)
		.optional(),
	metadata: z.record(z.string(), z.unknown()).optional(),
});
export type SharedState = z.infer<typeof stateSchema>;
export type StateCookie = SharedState["cookies"][number];

function distinct<T>(items: readonly T[], key: (item: T) => string): void {
	if (new Set(items.map(key)).size !== items.length)
		throw new Error("Invalid state: duplicate identities");
}
function validKey(value: unknown): boolean {
	return (
		typeof value === "string" ||
		(typeof value === "number" && Number.isFinite(value)) ||
		value instanceof ArrayBuffer ||
		ArrayBuffer.isView(value) ||
		(Array.isArray(value) && value.every(validKey))
	);
}
function keyIdentity(value: unknown): string {
	if (value instanceof ArrayBuffer || ArrayBuffer.isView(value)) {
		const bytes =
			value instanceof ArrayBuffer
				? new Uint8Array(value)
				: new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
		return `binary:${JSON.stringify([...bytes])}`;
	}
	return Array.isArray(value)
		? `array:${JSON.stringify(value.map(keyIdentity))}`
		: `${typeof value}:${JSON.stringify(value)}`;
}
function inlineKey(value: unknown, path: string | string[]): unknown {
	if (Array.isArray(path)) return path.map((item) => inlineKey(value, item));
	if (path === "") return value;
	let result = value;
	for (const part of path.split(".")) {
		if (
			result === null ||
			result === undefined ||
			!Object.hasOwn(Object(result), part)
		)
			return undefined;
		result = (Object(result) as Record<string, unknown>)[part];
	}
	return result;
}
function validKeyPath(value: string | string[]): boolean {
	return typeof value === "string"
		? value === "" ||
				value
					.split(".")
					.every((part) =>
						/^[$_\p{ID_Start}](?:[$\p{ID_Continue}]|\u200C|\u200D)*$/u.test(
							part,
						),
					)
		: value.every((part) => validKeyPath(part));
}

/** Validate without including cookie, storage, or database values in errors. */
export function parseState(value: unknown): SharedState {
	const result = stateSchema.safeParse(value);
	if (!result.success)
		throw new Error(
			"Invalid state: expected whirl-state version 1 with valid storage fields; create a new file with --save-state",
		);
	const state = result.data;
	distinct(state.cookies, (cookie) =>
		JSON.stringify([
			cookie.domain,
			cookie.path,
			cookie.name,
			cookie.partition ?? null,
		]),
	);
	for (const cookie of state.cookies) {
		try {
			if (new URL(`http://${cookie.domain}`).hostname !== cookie.domain)
				throw new Error();
		} catch {
			throw new Error(
				"Invalid state: cookie domain must be a canonical hostname",
			);
		}
		if (
			(cookie.name.startsWith("__Http-") ||
				cookie.name.startsWith("__Host-Http-")) &&
			(!cookie.secure || !cookie.httpOnly)
		)
			throw new Error("Invalid state: HTTP cookie prefix restrictions");
		if (cookie.sameSite === "None" && !cookie.secure)
			throw new Error("Invalid state: SameSite=None requires Secure");
		if (cookie.name.startsWith("__Secure-") && !cookie.secure)
			throw new Error("Invalid state: __Secure- cookie requires Secure");
		if (
			cookie.name.startsWith("__Host-") &&
			(!cookie.secure || !cookie.hostOnly || cookie.path !== "/")
		)
			throw new Error("Invalid state: __Host- cookie restrictions");
	}
	distinct(state.origins, (item) => item.origin);
	distinct(state.pages ?? [], (item) => item.id);
	for (const page of state.pages ?? []) {
		distinct(page.origins, (item) => item.origin);
		for (const item of page.origins)
			distinct(item.sessionStorage ?? [], (entry) => entry.name);
	}
	for (const item of state.origins) {
		distinct(item.localStorage ?? [], (entry) => entry.name);
		distinct(item.indexedDB ?? [], (entry) => entry.name);
		for (const db of item.indexedDB ?? []) {
			distinct(db.stores, (entry) => entry.name);
			for (const store of db.stores) {
				if (store.keyPath !== null && !validKeyPath(store.keyPath))
					throw new Error("Invalid state: IndexedDB key path");
				if (
					store.autoIncrement &&
					(store.keyPath === "" ||
						Array.isArray(store.keyPath) ||
						store.nextKey === undefined)
				)
					throw new Error(
						"Invalid state: autoIncrement requires nextKey and a supported key path",
					);
				if (!store.autoIncrement && store.nextKey !== undefined)
					throw new Error("Invalid state: nextKey requires autoIncrement");
				distinct(store.indexes, (entry) => entry.name);
				for (const index of store.indexes)
					if (
						!validKeyPath(index.keyPath) ||
						(index.multiEntry && Array.isArray(index.keyPath))
					)
						throw new Error("Invalid state: IndexedDB index schema");
				const keys: string[] = [];
				for (const saved of store.records) {
					if (!Object.hasOwn(saved, "key") || !Object.hasOwn(saved, "value"))
						throw new Error(
							"Invalid state: IndexedDB record requires key and value",
						);
					try {
						const decoded = databaseRecord(saved, true);
						if (!validKey(decoded.key)) throw new Error();
						databaseRecord(decoded);
						if (
							store.keyPath !== null &&
							keyIdentity(inlineKey(decoded.value, store.keyPath)) !==
								keyIdentity(decoded.key)
						)
							throw new Error();
						distinct(
							[
								...(saved.binary?.map((entry) => entry.path) ?? []),
								...(saved.undefinedPaths ?? []),
							],
							(entry) => JSON.stringify(entry.map(String)),
						);
						keys.push(keyIdentity(decoded.key));
					} catch {
						throw new Error("Invalid state: unsupported IndexedDB record");
					}
				}
				distinct(keys, (key) => key);
				const max = Math.max(
					0,
					...store.records.map((saved) =>
						typeof saved.key === "number" ? saved.key : 0,
					),
				);
				if (
					store.nextKey !== undefined &&
					store.nextKey <= max &&
					max < 2 ** 53
				)
					throw new Error(
						"Invalid state: IndexedDB nextKey precedes an existing key",
					);
			}
		}
	}
	return state;
}

/** Capabilities are checked before opening a browser or running application code. */
export function checkStateCapabilities(
	state: SharedState,
	singleOrigin?: string,
): void {
	if (state.cookies.some((cookie) => cookie.partition !== undefined))
		throw new Error(
			"Partitioned cookies are not supported by this state adapter",
		);
	if (state.pages?.some((page) => page.id !== "main"))
		throw new Error("State adapter supports only the main page");
	if (
		singleOrigin !== undefined &&
		(state.origins.some((item) => item.origin !== singleOrigin) ||
			state.pages?.some((page) =>
				page.origins.some((item) => item.origin !== singleOrigin),
			))
	) {
		throw new Error(
			"State contains a different origin; this BrowserSim session supports one origin",
		);
	}
}
