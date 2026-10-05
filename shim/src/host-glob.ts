// Host glob matching for the allow-hosts and block-hosts options (SPEC
// section 5).
//
// Globs match the request URL's hostname only. `*` matches any run of
// characters including dots, so `*.example.com` matches every subdomain but
// never the apex `example.com` (the literal dot must be present). Matching
// is case-insensitive, and IP literals match textually.

function stripBrackets(host: string): string {
	if (host.startsWith("[") && host.endsWith("]")) {
		return host.slice(1, -1);
	}
	return host;
}

const regexSpecials = /[.+?^${}()|[\]\\]/g;

export function hostGlobToRegExp(glob: string): RegExp {
	const escaped = stripBrackets(glob)
		.replace(regexSpecials, "\\$&")
		.replaceAll("*", ".*");
	return new RegExp(`^${escaped}$`, "i");
}

export function hostnameMatchesGlob(hostname: string, glob: string): boolean {
	return hostGlobToRegExp(glob).test(stripBrackets(hostname));
}

/** A hostname that a host rule blocked (SPEC section 5). */
export interface BlockedHost {
	readonly host: string;
	/** The option whose rule blocked the host. */
	readonly option: "allow-hosts" | "block-hosts";
	/** The `block-hosts` glob that matched; null when no `allow-hosts` glob did. */
	readonly glob: string | null;
}

/**
 * Compiles both host lists once and returns a hostname check: the rule that
 * blocks the host, or null when the page may reach it. A `block-hosts` glob
 * wins over `allow-hosts`, including the base host that Rust appends to it.
 * A null list sets no rule.
 */
export function createHostPolicy(
	allowHosts: readonly string[] | null,
	blockHosts: readonly string[] | null,
): (hostname: string) => BlockedHost | null {
	const isAllowed =
		allowHosts === null ? null : createHostAllowlist(allowHosts);
	const blocks = (blockHosts ?? []).map((glob) => ({
		glob,
		pattern: hostGlobToRegExp(glob),
	}));
	return (hostname: string): BlockedHost | null => {
		const host = hostname.toLowerCase();
		const block = blocks.find(({ pattern }) =>
			pattern.test(stripBrackets(hostname)),
		);
		if (block !== undefined) {
			return { host, option: "block-hosts", glob: block.glob };
		}
		if (isAllowed !== null && !isAllowed(hostname)) {
			return { host, option: "allow-hosts", glob: null };
		}
		return null;
	};
}

/** The rule that blocked a host, as messages and reports write it. */
export function describeBlockedHost(blocked: BlockedHost): string {
	return blocked.glob === null
		? blocked.option
		: `${blocked.option} ${blocked.glob}`;
}

/** The hosts a flow blocked, each with the first rule that blocked it. */
export class BlockedHostLog {
	readonly #hosts = new Map<string, BlockedHost>();

	record(blocked: BlockedHost): void {
		if (!this.#hosts.has(blocked.host)) {
			this.#hosts.set(blocked.host, blocked);
		}
	}

	/** Every blocked host, sorted by hostname. */
	list(): readonly BlockedHost[] {
		return [...this.#hosts.values()].sort((a, b) =>
			a.host < b.host ? -1 : a.host > b.host ? 1 : 0,
		);
	}
}

/**
 * Compiles the globs once and returns a hostname predicate.
 * URL hostnames arrive as `URL.hostname` yields them; IPv6 literals keep or
 * drop their brackets on either side, both spellings match.
 */
export function createHostAllowlist(
	globs: readonly string[],
): (hostname: string) => boolean {
	const patterns = globs.map(hostGlobToRegExp);
	return (hostname: string): boolean => {
		const bare = stripBrackets(hostname);
		return patterns.some((pattern) => pattern.test(bare));
	};
}
