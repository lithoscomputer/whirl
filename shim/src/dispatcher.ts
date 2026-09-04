// Protocol framing and request dispatch (protocol sections 2, 3, and 6).
//
// Newline-delimited JSON on stdin/stdout. The read loop never blocks on an
// in-flight step, so cancelFlow works out of band. Every request gets
// exactly one response, and no exception escapes the dispatcher.

import { createInterface } from "node:readline";
import type { Readable, Writable } from "node:stream";
import type { ShimDriver } from "./driver.js";
import type { Params } from "./params.js";
import {
	asParams,
	decodeEndFlowParams,
	decodeStartFlowParams,
} from "./params.js";
import type { ProtocolError } from "./protocol.js";
import { isStepCommand, toProtocolError } from "./protocol.js";

const protocolVersion = 1;

/** One-response-per-request guard. */
interface ResponseSlot {
	sent: boolean;
}

interface InFlightStep {
	readonly id: number;
	readonly slot: ResponseSlot;
}

export interface DispatcherOptions {
	readonly input: Readable;
	readonly output: Writable;
	readonly driver: ShimDriver;
	readonly onExit: (code: number) => void;
	/** Diagnostics sink; defaults to stderr via console.error. */
	readonly log?: (message: string) => void;
}

export class Dispatcher {
	readonly #input: Readable;
	readonly #output: Writable;
	readonly #driver: ShimDriver;
	readonly #onExit: (code: number) => void;
	readonly #log: (message: string) => void;
	#inFlight: InFlightStep | null = null;
	#lastWrite: Promise<void> = Promise.resolve();

	constructor(options: DispatcherOptions) {
		this.#input = options.input;
		this.#output = options.output;
		this.#driver = options.driver;
		this.#onExit = options.onExit;
		this.#log = options.log ?? ((message) => console.error(message));
	}

	run(): void {
		const lines = createInterface({
			input: this.#input,
			crlfDelay: Number.POSITIVE_INFINITY,
		});
		lines.on("line", (line) => {
			if (line.trim() === "") {
				return;
			}
			this.#handleLine(line);
		});
	}

	#write(payload: Record<string, unknown>): void {
		const data = `${JSON.stringify(payload)}\n`;
		this.#lastWrite = this.#lastWrite.then(
			() =>
				new Promise<void>((resolve) => {
					this.#output.write(data, () => resolve());
				}),
		);
	}

	#respondOk(id: number, slot: ResponseSlot, result: Params): void {
		if (slot.sent) {
			return;
		}
		slot.sent = true;
		this.#write({ id, ok: true, result });
	}

	#respondError(id: number, slot: ResponseSlot, error: ProtocolError): void {
		if (slot.sent) {
			return;
		}
		slot.sent = true;
		this.#write({ id, ok: false, error });
	}

	#handleLine(line: string): void {
		let parsed: unknown;
		try {
			parsed = JSON.parse(line);
		} catch {
			// A malformed request is answered with kind "internal"; recover
			// the id best-effort so Rust can match the response.
			const idMatch = /"id"\s*:\s*(\d+)/.exec(line);
			const id = idMatch?.[1] === undefined ? 0 : Number(idMatch[1]);
			this.#respondError(
				id,
				{ sent: false },
				{
					kind: "internal",
					message: "malformed request: invalid JSON",
				},
			);
			return;
		}
		if (
			typeof parsed !== "object" ||
			parsed === null ||
			Array.isArray(parsed)
		) {
			this.#respondError(
				0,
				{ sent: false },
				{
					kind: "internal",
					message: "malformed request: not an object",
				},
			);
			return;
		}
		const record = parsed as Record<string, unknown>;
		const id = typeof record["id"] === "number" ? record["id"] : 0;
		const slot: ResponseSlot = { sent: false };
		const cmd = record["cmd"];
		if (typeof cmd !== "string") {
			this.#respondError(id, slot, {
				kind: "internal",
				message: "malformed request: missing cmd",
			});
			return;
		}
		let params: Params;
		try {
			params = asParams(record["params"] ?? {});
		} catch (error) {
			this.#respondError(id, slot, toProtocolError(error));
			return;
		}
		// Detached on purpose: the read loop must keep consuming stdin while
		// a step is in flight so cancelFlow arrives out of band. Each handler
		// catches everything and answers exactly once.
		void this.#handleRequest(id, slot, cmd, params);
	}

	async #handleRequest(
		id: number,
		slot: ResponseSlot,
		cmd: string,
		params: Params,
	): Promise<void> {
		try {
			if (cmd === "hello") {
				this.#respondOk(id, slot, {
					protocol: protocolVersion,
					playwrightVersion: this.#driver.playwrightVersion,
				});
				return;
			}
			if (cmd === "startFlow") {
				const result = await this.#driver.startFlow(
					decodeStartFlowParams(params),
				);
				this.#respondOk(id, slot, result);
				return;
			}
			if (cmd === "endFlow") {
				const result = await this.#driver.endFlow(decodeEndFlowParams(params));
				this.#respondOk(id, slot, {
					blockedHosts: [...result.blockedHosts],
					videoPath: result.videoPath,
				});
				return;
			}
			if (cmd === "cancelFlow") {
				await this.#handleCancel();
				this.#respondOk(id, slot, {});
				return;
			}
			if (cmd === "shutdown") {
				await this.#handleShutdown(id, slot);
				return;
			}
			if (isStepCommand(cmd)) {
				await this.#handleStep(id, slot, cmd, params);
				return;
			}
			this.#respondError(id, slot, {
				kind: "internal",
				message: `unknown command "${cmd}"`,
			});
		} catch (error) {
			this.#respondError(id, slot, toProtocolError(error));
		}
	}

	async #handleStep(
		id: number,
		slot: ResponseSlot,
		cmd: string,
		params: Params,
	): Promise<void> {
		if (!isStepCommand(cmd)) {
			return;
		}
		const inFlight: InFlightStep = { id, slot };
		this.#inFlight = inFlight;
		try {
			const result = await this.#driver.runStep(cmd, params);
			this.#respondOk(id, slot, result);
		} catch (error) {
			this.#respondError(id, slot, toProtocolError(error));
		} finally {
			if (this.#inFlight === inFlight) {
				this.#inFlight = null;
			}
		}
	}

	async #handleCancel(): Promise<void> {
		// Fail the in-flight step, if any, with kind "cancelled" before the
		// force-close: closing the context makes the abandoned Playwright
		// call reject with its own error, and the response slot guard must
		// already be spent so that late answer is dropped.
		const inFlight = this.#inFlight;
		if (inFlight !== null) {
			this.#inFlight = null;
			this.#respondError(inFlight.id, inFlight.slot, {
				kind: "cancelled",
				message: "step aborted by cancelFlow",
			});
		}
		try {
			await this.#driver.cancelFlow();
		} catch (error) {
			this.#log(`whirl-shim: cancelFlow cleanup failed: ${String(error)}`);
		}
	}

	async #handleShutdown(id: number, slot: ResponseSlot): Promise<void> {
		this.#respondOk(id, slot, {});
		try {
			await this.#driver.dispose();
		} catch (error) {
			this.#log(`whirl-shim: shutdown cleanup failed: ${String(error)}`);
		}
		await this.#lastWrite;
		this.#onExit(0);
	}
}
