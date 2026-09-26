// Wire contract between the Rust binary and this shim.
// Shapes follow docs/engineering/shim-protocol.md exactly.

export type ErrorKind =
	| "timeout"
	| "strictness"
	| "assert"
	| "snapshot-mismatch"
	| "snapshot-missing-baseline"
	| "eval"
	| "eval-result"
	| "read"
	| "action"
	| "cancelled"
	| "internal";

export interface ProtocolError {
	readonly kind: ErrorKind;
	readonly message: string;
	readonly expected?: string;
	readonly actual?: string;
	readonly candidates?: readonly string[];
}

export interface ErrorDetails {
	readonly expected?: string;
	readonly actual?: string;
	readonly candidates?: readonly string[];
}

/** A step failure carrying the protocol error shape. */
export class ShimError extends Error {
	readonly kind: ErrorKind;
	readonly details: ErrorDetails;

	constructor(kind: ErrorKind, message: string, details: ErrorDetails = {}) {
		super(message);
		this.name = "ShimError";
		this.kind = kind;
		this.details = details;
	}

	toProtocolError(): ProtocolError {
		return {
			kind: this.kind,
			message: this.message,
			...(this.details.expected === undefined
				? {}
				: { expected: this.details.expected }),
			...(this.details.actual === undefined
				? {}
				: { actual: this.details.actual }),
			...(this.details.candidates === undefined
				? {}
				: { candidates: this.details.candidates }),
		};
	}
}

export function toProtocolError(error: unknown): ProtocolError {
	if (error instanceof ShimError) {
		return error.toProtocolError();
	}
	const message = error instanceof Error ? error.message : String(error);
	return { kind: "internal", message };
}

// --- Locators (protocol 4.1) ---

export interface RoleSegment {
	readonly type: "role";
	readonly role: string;
	readonly name: string | null;
	readonly exact: boolean;
}

export interface TextEngineSegment {
	readonly type: "label" | "placeholder" | "text" | "alt" | "title";
	readonly text: string;
	readonly exact: boolean;
}

export interface TestidSegment {
	readonly type: "testid";
	readonly id: string;
}

export interface CssSegment {
	readonly type: "css";
	readonly selector: string;
}

export interface FrameSegment {
	readonly type: "frame";
	readonly selector: string;
}

export interface NthSegment {
	readonly type: "nth";
	readonly index: number;
}

/** An element ref from the latest `ariaSnapshot` (SPEC 7.4); Rust-only. */
export interface RefSegment {
	readonly type: "ref";
	readonly ref: string;
}

export type LocatorSegment =
	| RoleSegment
	| TextEngineSegment
	| TestidSegment
	| CssSegment
	| FrameSegment
	| NthSegment
	| RefSegment;

// --- PAGE expectations (protocol 4.2) ---

export interface PagePathExpectation {
	readonly kind: "path";
	readonly value: string;
}

export interface PagePathQueryExpectation {
	readonly kind: "pathQuery";
	readonly value: string;
}

export interface PageUrlExpectation {
	readonly kind: "url";
	readonly value: string;
}

export interface PageRegexExpectation {
	readonly kind: "regex";
	readonly source: string;
	readonly flags: string;
}

export type PageExpectation =
	| PagePathExpectation
	| PagePathQueryExpectation
	| PageUrlExpectation
	| PageRegexExpectation;

// --- Assert specs (protocol 4.3) ---

export type ElementState =
	| "visible"
	| "hidden"
	| "enabled"
	| "disabled"
	| "checked"
	| "unchecked"
	| "focused";

export interface StateCheck {
	readonly type: "state";
	readonly state: ElementState;
}

export interface LocatorSubject {
	readonly type: "locator";
	readonly locator: readonly LocatorSegment[];
}

/** A state check; `tab:NAME closed` is dispatched before this shape. */
export interface AssertSpec {
	readonly subject: LocatorSubject;
	readonly check: StateCheck;
}

// --- Reads (protocol 4.4, 4.5) ---

export type ReadExtract =
	| { readonly type: "text" }
	| { readonly type: "value" }
	| { readonly type: "attr"; readonly name: string };

export interface ElementReadSubject {
	readonly type: "element";
	readonly locator: readonly LocatorSegment[];
	readonly extract: ReadExtract;
}

export interface CountReadSubject {
	readonly type: "count";
	readonly locator: readonly LocatorSegment[];
}

export interface UrlReadSubject {
	readonly type: "url";
}

export interface TitleReadSubject {
	readonly type: "title";
}

export interface EvalReadSubject {
	readonly type: "eval";
	readonly script: string;
}

export type ReadSubject =
	| ElementReadSubject
	| CountReadSubject
	| UrlReadSubject
	| TitleReadSubject
	| EvalReadSubject;

export type JsonValue =
	| null
	| boolean
	| number
	| string
	| readonly JsonValue[]
	| { readonly [key: string]: JsonValue };

export interface ReadValue {
	readonly type: "value";
	readonly value: JsonValue;
}

export interface ReadMissing {
	readonly type: "missing";
	readonly reason: "no-element" | "absent-attribute";
}

export type ReadResult = ReadValue | ReadMissing;

export interface ResponseRead {
	readonly status: number;
	readonly url: string;
	readonly headers: readonly (readonly [string, string])[];
	readonly bodyBase64: string | null;
	readonly bodyError: string | null;
}

// --- Lifecycle params (protocol 3) ---

export type BrowserEngine = "chromium" | "firefox" | "webkit";

export interface ViewportSize {
	readonly width: number;
	readonly height: number;
}

export interface VideoConfig {
	readonly tempDir: string;
	readonly finalPath: string;
	/**
	 * Frames per second for the screencast recorder (Chromium only); null
	 * selects Playwright's own recorder at its fixed rate.
	 */
	readonly fps: number | null;
}

export interface StartFlowParams {
	readonly browser: BrowserEngine;
	readonly headed: boolean;
	readonly viewport: ViewportSize;
	readonly storageStatePath: string | null;
	readonly dialogs: "dismiss" | "accept";
	readonly allowHosts: readonly string[] | null;
	readonly navTimeoutMs: number;
	readonly userAgent: string | null;
	readonly reducedMotion: "reduce" | "no-preference" | null;
	readonly video: VideoConfig | null;
	readonly harPath: string | null;
	readonly trace: boolean;
}

export interface EndFlowParams {
	readonly saveStoragePath: string | null;
	readonly tracePath: string | null;
}

export interface EndFlowResult {
	readonly blockedHosts: readonly string[];
	readonly videoPath: string | null;
}

// --- Step commands (protocol 4) ---

export interface HttpParams {
	readonly name: string;
	readonly method: string;
	readonly url: string;
	readonly headers: readonly (readonly [string, string])[];
	readonly body: string | null;
}

export type StepCommand =
	| "http"
	| "response"
	| "popup"
	| "tab"
	| "close"
	| "visit"
	| "click"
	| "dblclick"
	| "fill"
	| "type"
	| "press"
	| "checkbox"
	| "selectOption"
	| "hover"
	| "upload"
	| "screenshot"
	| "snapshot"
	| "evalAction"
	| "store"
	| "ariaSnapshot"
	| "page"
	| "assert"
	| "read"
	| "readResponse"
	| "traceGroup"
	| "traceGroupEnd";

const stepCommandList: readonly StepCommand[] = [
	"http",
	"response",
	"popup",
	"tab",
	"close",
	"visit",
	"click",
	"dblclick",
	"fill",
	"type",
	"press",
	"checkbox",
	"selectOption",
	"hover",
	"upload",
	"screenshot",
	"snapshot",
	"evalAction",
	"store",
	"ariaSnapshot",
	"page",
	"assert",
	"read",
	"readResponse",
	"traceGroup",
	"traceGroupEnd",
];

const stepCommandSet: ReadonlySet<string> = new Set(stepCommandList);

export function isStepCommand(cmd: string): cmd is StepCommand {
	return stepCommandSet.has(cmd);
}

export function assertNever(value: never): never {
	throw new ShimError(
		"internal",
		`unexpected variant: ${JSON.stringify(value)}`,
	);
}
