interface BinaryConstructor {
	new (buffer: ArrayBuffer, offset: number, length: number): object;
	readonly BYTES_PER_ELEMENT?: number;
}

export interface BinaryReference {
	path: (string | number)[];
	type: string;
	object: number;
	buffer: number;
	bufferLength: number;
	byteOffset: number;
	byteLength: number;
}
export interface DatabaseRecord {
	key: unknown;
	value: unknown;
	binary?: BinaryReference[] | undefined;
	undefinedPaths?: (string | number)[][] | undefined;
}

export interface Database {
	name: string;
	version: number;
	stores: {
		name: string;
		keyPath: string | string[] | null;
		autoIncrement: boolean;
		nextKey?: number | undefined;
		indexes: {
			name: string;
			keyPath: string | string[];
			unique: boolean;
			multiEntry: boolean;
		}[];
		records: DatabaseRecord[];
	}[];
}

/** Self-contained for browser injection. Binary data stays under its original
 * field for privacy rules; a separate path list distinguishes it from app JSON. */
export function databaseRecord(
	record: DatabaseRecord,
	restore = false,
): DatabaseRecord {
	const types = [
		"DataView",
		"Int8Array",
		"Uint8Array",
		"Uint8ClampedArray",
		"Int16Array",
		"Uint16Array",
		"Int32Array",
		"Uint32Array",
		"Float16Array",
		"Float32Array",
		"Float64Array",
		"BigInt64Array",
		"BigUint64Array",
	];
	const required = <T>(value: T | undefined): T => {
		if (value === undefined) throw new Error("Invalid IndexedDB metadata");
		return value;
	};
	const invalid = () => {
		throw new Error("Invalid binary IndexedDB state");
	};
	if (restore) {
		// JSON records without codec metadata need no decoding.
		if (!record.binary?.length && !record.undefinedPaths?.length) return record;
		const result = structuredClone({ key: record.key, value: record.value });
		const locate = (path: BinaryReference["path"]) => {
			if (
				!Array.isArray(path) ||
				!path.length ||
				!["key", "value"].includes(String(path[0]))
			)
				return invalid();
			let parent: unknown = result;
			for (const part of path.slice(0, -1)) {
				if (
					!parent ||
					typeof parent !== "object" ||
					!Object.hasOwn(parent, part)
				)
					return invalid();
				parent = (parent as Record<string | number, unknown>)[part];
			}
			const key = required(path.at(-1));
			if (!parent || typeof parent !== "object" || !Object.hasOwn(parent, key))
				return invalid();
			return { parent: parent as Record<string | number, unknown>, key };
		};
		const buffers = new Map<
			number,
			{ data: string; length: number; value?: ArrayBuffer }
		>();
		for (const entry of record.binary ?? []) {
			if (
				![
					entry.object,
					entry.buffer,
					entry.bufferLength,
					entry.byteOffset,
					entry.byteLength,
				].every((n) => Number.isSafeInteger(n) && n >= 0)
			)
				return invalid();
			if (
				entry.byteOffset + entry.byteLength > entry.bufferLength ||
				!(entry.type === "ArrayBuffer" || types.includes(entry.type))
			)
				return invalid();
			const { parent, key } = locate(entry.path),
				data = parent[key];
			if (typeof data !== "string") return invalid();
			const previous = buffers.get(entry.buffer);
			if (previous) {
				if (previous.length !== entry.bufferLength || previous.data !== data)
					return invalid();
			} else buffers.set(entry.buffer, { data, length: entry.bufferLength });
		}
		for (const buffer of buffers.values()) {
			let bytes: Uint8Array;
			{
				let decoded: string;
				try {
					decoded = atob(buffer.data);
				} catch {
					return invalid();
				}
				if (decoded.length !== buffer.length) return invalid();
				bytes = Uint8Array.from(decoded, (char) => char.charCodeAt(0));
			}
			buffer.value = bytes.buffer as ArrayBuffer;
		}
		const objects = new Map<number, { signature: string; value: unknown }>();
		for (const entry of record.binary ?? []) {
			const signature = JSON.stringify([
				entry.type,
				entry.buffer,
				entry.byteOffset,
				entry.byteLength,
			]);
			let object = objects.get(entry.object);
			if (object && object.signature !== signature) return invalid();
			if (!object) {
				const buffer = required(buffers.get(entry.buffer)?.value);
				let value: unknown;
				if (entry.type === "ArrayBuffer") {
					if (entry.byteOffset !== 0 || entry.byteLength !== entry.bufferLength)
						return invalid();
					value = buffer;
				} else {
					const ctor = (
						globalThis as unknown as Record<string, BinaryConstructor>
					)[entry.type];
					if (typeof ctor !== "function") return invalid();
					const size =
						entry.type === "DataView" ? 1 : required(ctor.BYTES_PER_ELEMENT);
					if (entry.byteOffset % size || entry.byteLength % size)
						return invalid();
					value = new ctor(buffer, entry.byteOffset, entry.byteLength / size);
				}
				object = { signature, value };
				objects.set(entry.object, object);
			}
			const { parent, key } = locate(entry.path);
			Object.defineProperty(parent, key, {
				value: object.value,
				writable: true,
				enumerable: true,
				configurable: true,
			});
		}
		for (const path of record.undefinedPaths ?? []) {
			const { parent, key } = locate(path);
			if (parent[key] !== null) return invalid();
			Object.defineProperty(parent, key, {
				value: undefined,
				writable: true,
				enumerable: true,
				configurable: true,
			});
		}
		return result;
	}
	const binary: BinaryReference[] = [];
	const undefinedPaths: (string | number)[][] = [];
	const buffers = new Map<ArrayBuffer, { id: number; data: string }>(),
		objects = new Map<object, number>();
	const ancestors = new Set<object>();
	const seen = new Set<object>();
	const json = (value: unknown, path: BinaryReference["path"]): unknown => {
		if (value === undefined) {
			undefinedPaths.push(path);
			return null;
		}
		if (
			value === null ||
			typeof value === "string" ||
			typeof value === "boolean" ||
			(typeof value === "number" &&
				Number.isFinite(value) &&
				!Object.is(value, -0))
		)
			return value;
		if (value instanceof ArrayBuffer || ArrayBuffer.isView(value)) {
			const buffer = value instanceof ArrayBuffer ? value : value.buffer;
			if (!(buffer instanceof ArrayBuffer) || buffer.resizable)
				throw new Error(
					"IndexedDB state export does not support shared or resizable binary buffers",
				);
			let saved = buffers.get(buffer);
			if (!saved) {
				const bytes = new Uint8Array(buffer);
				let data = "";
				for (let start = 0; start < bytes.length; start += 32768)
					data += String.fromCharCode(...bytes.subarray(start, start + 32768));
				saved = { id: buffers.size, data: btoa(data) };
				buffers.set(buffer, saved);
			}
			const type =
				value instanceof ArrayBuffer
					? "ArrayBuffer"
					: types.find((name) => {
							const ctor = (
								globalThis as unknown as Record<
									string,
									BinaryConstructor | undefined
								>
							)[name];
							return ctor !== undefined && value instanceof ctor;
						});
			if (!type) return invalid();
			if (!objects.has(value)) objects.set(value, objects.size);
			binary.push({
				path,
				type,
				object: required(objects.get(value)),
				buffer: saved.id,
				bufferLength: buffer.byteLength,
				byteOffset: value instanceof ArrayBuffer ? 0 : value.byteOffset,
				byteLength: value.byteLength,
			});
			return saved.data;
		}
		if (
			Array.isArray(value) ||
			(value && Object.getPrototypeOf(value) === Object.prototype)
		) {
			if (ancestors.has(value))
				throw new Error(
					"IndexedDB state export does not support cyclic objects",
				);
			if (Array.isArray(value) && Object.keys(value).length !== value.length)
				throw new Error(
					"IndexedDB state export does not support sparse arrays or extra array properties",
				);
			if (seen.has(value))
				throw new Error(
					"IndexedDB state export does not support shared object references",
				);
			seen.add(value);
			ancestors.add(value);
			try {
				return Array.isArray(value)
					? value.map((child, index) => json(child, [...path, index]))
					: Object.fromEntries(
							Object.entries(value as Record<string, unknown>).map(
								([key, child]) => [key, json(child, [...path, key])],
							),
						);
			} finally {
				ancestors.delete(value);
			}
		}
		const kind =
			value === undefined
				? "undefined"
				: typeof value === "bigint"
					? "bigint"
					: value instanceof Date
						? "Date"
						: value instanceof Blob
							? "Blob"
							: value instanceof ArrayBuffer
								? "ArrayBuffer"
								: ArrayBuffer.isView(value)
									? "typed array"
									: value instanceof Map
										? "Map"
										: value instanceof Set
											? "Set"
											: value instanceof RegExp
												? "RegExp"
												: typeof CryptoKey !== "undefined" &&
														value instanceof CryptoKey
													? "CryptoKey"
													: "object";
		throw new Error(
			`IndexedDB state export supports JSON values and binary buffers; this database contains a non-JSON value (${kind})`,
		);
	};
	const result: DatabaseRecord = {
		key: json(record.key, ["key"]),
		value: json(record.value, ["value"]),
	};
	if (binary.length) result.binary = binary;
	if (undefinedPaths.length) result.undefinedPaths = undefinedPaths;
	return result;
}

/** Read or restore JSON, undefined and binary IndexedDB records in the page's own realm. */
export async function databaseState(
	seed?: Database[],
	transform = databaseRecord,
): Promise<Database[]> {
	const required = <T>(value: T | undefined): T => {
		if (value === undefined) throw new Error("Invalid IndexedDB metadata");
		return value;
	};
	const request = <T>(value: IDBRequest<T>) =>
		new Promise<T>((resolve, reject) => {
			value.onsuccess = () => resolve(value.result);
			value.onerror = () => reject(value.error);
		});
	const complete = (transaction: IDBTransaction) =>
		new Promise<void>((resolve, reject) => {
			transaction.oncomplete = () => resolve();
			transaction.onabort = () =>
				reject(transaction.error ?? Error("IndexedDB transaction aborted"));
		});
	const databases: Database[] = [];
	for (const metadata of seed ?? (await indexedDB.databases())) {
		if (metadata.name === undefined || metadata.version === undefined)
			throw new Error(
				"IndexedDB metadata is incomplete; wait for pending database changes",
			);
		const opening = indexedDB.open(metadata.name, metadata.version);
		if (seed)
			opening.onupgradeneeded = () => {
				for (const store of (metadata as Database).stores) {
					const object = opening.result.createObjectStore(store.name, {
						keyPath: store.keyPath,
						autoIncrement: store.autoIncrement,
					});
					for (const index of store.indexes)
						object.createIndex(index.name, index.keyPath, {
							unique: index.unique,
							multiEntry: index.multiEntry,
						});
				}
			};
		const database = await request(opening);
		try {
			const result: Database = {
				name: database.name,
				version: database.version,
				stores: [],
			};
			for (const name of database.objectStoreNames) {
				const transaction = database.transaction(
						name,
						seed ? "readwrite" : "readonly",
					),
					done = complete(transaction),
					store = transaction.objectStore(name);
				if (seed) {
					const input = required(
						(metadata as Database).stores.find((store) => store.name === name),
					);
					for (const saved of input.records) {
						const record = transform(saved, true);
						store.keyPath === null
							? store.put(record.value, record.key as IDBValidKey)
							: store.put(record.value);
					}
					const last = Math.max(
						0,
						...input.records.map((record) =>
							typeof record.key === "number" ? record.key : 0,
						),
					);
					if (
						input.autoIncrement &&
						input.nextKey &&
						input.nextKey - 1 > last
					) {
						const key = input.nextKey - 1,
							value: Record<string, unknown> = {};
						if (typeof store.keyPath === "string") {
							const parts = store.keyPath.split(".");
							let object = value;
							for (const part of parts.slice(0, -1)) {
								const child: Record<string, unknown> = {};
								object[part] = child;
								object = child;
							}
							object[required(parts.at(-1))] = key;
							store.put(value);
						} else store.put(value, key);
						store.delete(key);
					}
				} else {
					const [keys, values] = await Promise.all([
						request(store.getAllKeys()),
						request(store.getAll()),
					]);
					result.stores.push({
						name,
						keyPath: store.keyPath,
						autoIncrement: store.autoIncrement,
						indexes: [...store.indexNames].map((name) => {
							const index = store.index(name);
							return {
								name,
								keyPath: index.keyPath,
								unique: index.unique,
								multiEntry: index.multiEntry,
							};
						}),
						records: keys.map((key, index) =>
							transform({ key, value: values[index] }),
						),
					});
				}
				await done;
				if (!seed && store.autoIncrement) {
					const probe = database.transaction(name, "readwrite");
					const aborted = new Promise<void>((resolve) => {
						probe.onabort = () => resolve();
					});
					try {
						const nextKey = await request(probe.objectStore(name).add({}));
						required(result.stores[result.stores.length - 1]).nextKey =
							Number(nextKey);
					} catch (error) {
						if (
							!(error instanceof DOMException) ||
							error.name !== "ConstraintError"
						)
							throw error;
						throw new Error(
							"IndexedDB state export does not support exhausted key generators",
						);
					} finally {
						try {
							probe.abort();
						} catch {
							// A failed request can abort the transaction before cleanup runs.
						}
						await aborted;
					}
				}
			}
			databases.push(result);
		} finally {
			database.close();
		}
	}
	return databases;
}
