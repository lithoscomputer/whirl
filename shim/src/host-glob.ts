// Host glob matching for the allowHosts option (SPEC section 5).
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
