/** Self-contained for browser injection. Named keys can shadow Storage methods. */
export function storageEntries(
	area: "local" | "session",
): { name: string; value: string }[] {
	const storage = area === "local" ? localStorage : sessionStorage;
	const length = Object.getOwnPropertyDescriptor(
		Storage.prototype,
		"length",
	)?.get;
	if (length === undefined) throw new Error("Native storage is unavailable");
	const result: { name: string; value: string }[] = [];
	const count = Number(length.call(storage));
	for (let index = 0; index < count; index++) {
		const name = Storage.prototype.key.call(storage, index);
		if (name !== null) {
			const value = Storage.prototype.getItem.call(storage, name);
			if (value !== null) result.push({ name, value });
		}
	}
	return result;
}
