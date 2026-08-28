// Whirl browser shim entry point: newline-delimited JSON over stdin/stdout,
// diagnostics on stderr only (docs/engineering/shim-protocol.md).

import process from "node:process";
import { Dispatcher } from "./dispatcher.js";
import { PlaywrightDriver } from "./playwright-driver.js";

// The process must survive any step error; the dispatcher answers every
// request itself, so anything landing here is logged and the loop goes on.
// Rust kills the process if it ever becomes unresponsive.
process.on("uncaughtException", (error) => {
	console.error(`whirl-shim: uncaught exception: ${String(error)}`);
});
process.on("unhandledRejection", (reason) => {
	console.error(`whirl-shim: unhandled rejection: ${String(reason)}`);
});

const dispatcher = new Dispatcher({
	input: process.stdin,
	output: process.stdout,
	driver: new PlaywrightDriver(),
	onExit: (code) => process.exit(code),
});
dispatcher.run();
