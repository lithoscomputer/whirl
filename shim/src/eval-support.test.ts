import assert from "node:assert/strict";
import { test } from "node:test";
import {
	buildCaptureEvalExpression,
	buildEvalExpression,
	classifyEvalResult,
	isExpressionScript,
} from "./eval-support.js";

test("single expressions classify as expressions", () => {
	assert.equal(isExpressionScript("1 + 2"), true);
	assert.equal(isExpressionScript("document.title.trim()"), true);
	assert.equal(isExpressionScript("foo()"), true);
	// Parenthesized wrapping lets object literals parse as expressions.
	assert.equal(isExpressionScript("{ a: 1 }"), true);
	// Await is available in the expression form.
	assert.equal(isExpressionScript("await fetch('/x')"), true);
});

test("statement scripts classify as statements", () => {
	assert.equal(isExpressionScript("foo(); bar();"), false);
	assert.equal(isExpressionScript("return 5"), false);
	assert.equal(isExpressionScript("const x = 1; x"), false);
	assert.equal(isExpressionScript("if (a) { b(); }"), false);
	assert.equal(isExpressionScript("throw new Error('x')"), false);
});

test("unbalanced scripts cannot complete the probe wrapper", () => {
	// Without the outer parentheses this script completed the probe's own
	// `(...)` pair, misclassified as an expression, and its tail became
	// dead code after the emitted `return`.
	assert.equal(isExpressionScript("1); (window.__x = 2"), false);
	// Not an expression, so it runs as written and its SyntaxError fails
	// the entry (SPEC 7).
	const expression = buildEvalExpression("1); (window.__x = 2");
	assert.match(expression, /\n1\); \(window\.__x = 2\n/);
	assert.throws(() => {
		new Function(`return ${expression};`);
	}, SyntaxError);
});

test("expression scripts wrap as a returned expression", () => {
	const expression = buildEvalExpression("1 + 2");
	assert.match(expression, /^\(async \(\) => \{\n/);
	assert.match(expression, /return \(async \(\) => \(1 \+ 2\n\)\)\(\);/);
});

test("statement scripts run as written", () => {
	const expression = buildEvalExpression("foo(); bar();");
	assert.match(expression, /\nfoo\(\); bar\(\);\n/);
	assert.doesNotMatch(expression, /return \(foo/);
});

test("capture eval wrapper embeds the classifier", () => {
	const expression = buildCaptureEvalExpression("1 + 2");
	assert.match(expression, /const classify = function classifyEvalResult/);
	assert.match(expression, /return classify\(value\);/);
});

test("strings are returned as-is", () => {
	assert.deepEqual(classifyEvalResult("hello"), { ok: true, value: "hello" });
	assert.deepEqual(classifyEvalResult(""), { ok: true, value: "" });
});

test("JSON-safe values serialize as compact JSON", () => {
	assert.deepEqual(classifyEvalResult(null), { ok: true, value: "null" });
	assert.deepEqual(classifyEvalResult(true), { ok: true, value: "true" });
	assert.deepEqual(classifyEvalResult(42), { ok: true, value: "42" });
	assert.deepEqual(classifyEvalResult(1.5), { ok: true, value: "1.5" });
	assert.deepEqual(classifyEvalResult([1, "a", null]), {
		ok: true,
		value: '[1,"a",null]',
	});
	assert.deepEqual(classifyEvalResult({ a: 1, b: [true] }), {
		ok: true,
		value: '{"a":1,"b":[true]}',
	});
	assert.deepEqual(classifyEvalResult(Object.create(null)), {
		ok: true,
		value: "{}",
	});
});

test("shared (non-cyclic) references are fine", () => {
	const shared = { a: 1 };
	const outcome = classifyEvalResult({ x: shared, y: shared });
	assert.deepEqual(outcome, { ok: true, value: '{"x":{"a":1},"y":{"a":1}}' });
});

test("values outside the contract are rejected", () => {
	assert.equal(classifyEvalResult(undefined).ok, false);
	assert.equal(classifyEvalResult(Number.NaN).ok, false);
	assert.equal(classifyEvalResult(Number.POSITIVE_INFINITY).ok, false);
	assert.equal(classifyEvalResult(BigInt(1)).ok, false);
	assert.equal(
		classifyEvalResult(() => {
			return 0;
		}).ok,
		false,
	);
	assert.equal(classifyEvalResult(Symbol("x")).ok, false);
	assert.equal(classifyEvalResult(new Date()).ok, false);
	assert.equal(classifyEvalResult(new Map()).ok, false);
});

test("nested offenders reject the whole value", () => {
	assert.equal(classifyEvalResult({ a: [1, { b: undefined }] }).ok, false);
	assert.equal(classifyEvalResult([Number.NaN]).ok, false);
	assert.equal(classifyEvalResult({ nested: new Date() }).ok, false);
});

test("cyclic structures are rejected with a reason", () => {
	interface Cyclic {
		self?: unknown;
	}
	const cyclic: Cyclic = {};
	cyclic.self = cyclic;
	const outcome = classifyEvalResult(cyclic);
	assert.equal(outcome.ok, false);
	if (!outcome.ok) {
		assert.match(outcome.reason, /cyclic/);
	}
});

test("the classifier source is self-contained", () => {
	// The compiled function is injected into the page via toString(), so it
	// must reference nothing from module scope.
	const source = classifyEvalResult.toString();
	const rebuilt = new Function(
		`return (${source});`,
	)() as typeof classifyEvalResult;
	assert.deepEqual(rebuilt({ a: [1] }), { ok: true, value: '{"a":[1]}' });
	assert.equal(rebuilt(undefined).ok, false);
});
