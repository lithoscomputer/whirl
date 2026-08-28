// EVAL script handling (SPEC section 7 and 10, protocol section 4).
//
// A script runs as the body of an async function in the page's main world.
// A script that parses as a single expression runs as `return (script);`;
// any other script runs as written.

/**
 * True when the script parses as a single expression. The probe wraps the
 * script in an async arrow so `await <expr>` classifies as an expression.
 * `new Function` only parses here; nothing is ever executed in Node.
 */
export function isExpressionScript(script: string): boolean {
	try {
		// eslint-style note: parse-only probe; the function is discarded.
		new Function(`return async () => (${script}\n);`);
		return true;
	} catch {
		return false;
	}
}

function functionBody(script: string): string {
	return isExpressionScript(script) ? `return (${script}\n);` : script;
}

/**
 * The expression handed to page.evaluate for the EVAL action form.
 * Playwright awaits the resulting promise; the caller discards the value.
 */
export function buildEvalExpression(script: string): string {
	return `(async () => {\n${functionBody(script)}\n})()`;
}

export interface EvalAccepted {
	readonly ok: true;
	readonly value: string;
}

export interface EvalRejected {
	readonly ok: false;
	readonly reason: string;
}

export type EvalClassification = EvalAccepted | EvalRejected;

/**
 * The SPEC section 10 result contract, applied to a capture `eval` result.
 * A string is returned as-is; null, booleans, finite numbers, and
 * arrays/plain objects recursively containing only those become compact
 * JSON; anything else is rejected with a reason.
 *
 * This function must stay fully self-contained: its compiled source is
 * injected into the page with `toString()` and runs there, so the
 * classification is independent of Playwright's transport.
 */
export function classifyEvalResult(value: unknown): EvalClassification {
	if (typeof value === "string") {
		return { ok: true, value };
	}
	let reason = "";
	const describe = (offender: unknown): string => {
		if (offender === undefined) {
			return "undefined";
		}
		if (typeof offender === "number") {
			return `a non-finite number (${String(offender)})`;
		}
		if (typeof offender === "bigint") {
			return "a BigInt";
		}
		if (typeof offender === "function") {
			return "a function";
		}
		if (typeof offender === "symbol") {
			return "a symbol";
		}
		const proto = Object.getPrototypeOf(offender);
		const name =
			proto !== null &&
			typeof proto.constructor === "function" &&
			typeof proto.constructor.name === "string" &&
			proto.constructor.name !== ""
				? proto.constructor.name
				: "unknown type";
		return `an object of type ${name}`;
	};
	const ancestors: unknown[] = [];
	const acceptable = (candidate: unknown): boolean => {
		if (candidate === null || typeof candidate === "boolean") {
			return true;
		}
		if (typeof candidate === "string") {
			return true;
		}
		if (typeof candidate === "number") {
			if (Number.isFinite(candidate)) {
				return true;
			}
			reason = describe(candidate);
			return false;
		}
		if (Array.isArray(candidate)) {
			if (ancestors.includes(candidate)) {
				reason = "a cyclic structure";
				return false;
			}
			ancestors.push(candidate);
			const fine = candidate.every((item) => acceptable(item));
			ancestors.pop();
			return fine;
		}
		if (typeof candidate === "object") {
			const proto = Object.getPrototypeOf(candidate);
			if (proto !== Object.prototype && proto !== null) {
				reason = describe(candidate);
				return false;
			}
			if (ancestors.includes(candidate)) {
				reason = "a cyclic structure";
				return false;
			}
			ancestors.push(candidate);
			const fine = Object.values(candidate).every((item) => acceptable(item));
			ancestors.pop();
			return fine;
		}
		reason = describe(candidate);
		return false;
	};
	if (acceptable(value)) {
		return { ok: true, value: JSON.stringify(value) };
	}
	return { ok: false, reason };
}

/**
 * The expression handed to page.evaluate for a capture `eval` source. The
 * section 10 result contract runs inside the page; the wrapper returns a
 * plain `{ok, value}` or `{ok, reason}` object that always survives the
 * transport.
 */
export function buildCaptureEvalExpression(script: string): string {
	return [
		"(async () => {",
		`const classify = ${classifyEvalResult.toString()};`,
		`const value = await (async () => {\n${functionBody(script)}\n})();`,
		"return classify(value);",
		"})()",
	].join("\n");
}
