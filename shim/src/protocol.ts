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
	| "capture"
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

export type LocatorSegment =
	| RoleSegment
	| TextEngineSegment
	| TestidSegment
	| CssSegment
	| FrameSegment
	| NthSegment;

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

export interface StringOpValue {
	readonly op: "==" | "!=" | "contains";
	readonly value: string;
}

export interface StringOpMatches {
	readonly op: "matches";
	readonly source: string;
	readonly flags: string;
}

export type StringOp = StringOpValue | StringOpMatches;

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

export interface TextCheck {
	readonly type: "text";
	readonly op: StringOp;
}

export interface ValueCheck {
	readonly type: "value";
	readonly op: StringOp;
}

export interface AttrCheck {
	readonly type: "attr";
	readonly name: string;
	readonly op: StringOp;
}

export type CountOp = "==" | "!=" | "<" | "<=" | ">" | ">=";

export interface CountCheck {
	readonly type: "count";
	readonly op: CountOp;
	readonly value: number;
}

export type AssertCheck =
	| StateCheck
	| TextCheck
	| ValueCheck
	| AttrCheck
	| CountCheck;

export interface LocatorSubject {
	readonly type: "locator";
	readonly locator: readonly LocatorSegment[];
}

export interface UrlSubject {
	readonly type: "url";
}

export interface TitleSubject {
	readonly type: "title";
}

export type AssertSubject = LocatorSubject | UrlSubject | TitleSubject;

export interface AssertSpec {
	readonly subject: AssertSubject;
	readonly check: AssertCheck;
}

// --- Capture sources (protocol 4.4) ---

export interface TextExtract {
	readonly type: "text";
}

export interface ValueExtract {
	readonly type: "value";
}

export interface CountExtract {
	readonly type: "count";
}

export interface AttrExtract {
	readonly type: "attr";
	readonly name: string;
}

export type ElementExtract =
	| TextExtract
	| ValueExtract
	| CountExtract
	| AttrExtract;

export interface ElementCaptureSource {
	readonly type: "element";
	readonly locator: readonly LocatorSegment[];
	readonly extract: ElementExtract;
}

export interface UrlCaptureSource {
	readonly type: "url";
}

export interface TitleCaptureSource {
	readonly type: "title";
}

export interface EvalCaptureSource {
	readonly type: "eval";
	readonly script: string;
}

export type CaptureSource =
	| ElementCaptureSource
	| UrlCaptureSource
	| TitleCaptureSource
	| EvalCaptureSource;

export interface CaptureFilter {
	readonly source: string;
	readonly flags: string;
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
	| "page"
	| "assert"
	| "capture";

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
	"page",
	"assert",
	"capture",
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
