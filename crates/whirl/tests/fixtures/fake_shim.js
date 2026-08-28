// A fake browser shim for Rust client tests. It speaks the JSON-Lines
// protocol of docs/engineering/shim-protocol.md on stdin/stdout without
// launching anything. Step behavior is directed by the request itself:
// an `evalAction` script of the form below picks the response.
//
//   ok           reply {} immediately
//   delay:N      reply {} after N milliseconds
//   error        reply with an assert error object
//   never        never reply (until cancelFlow fails it as cancelled)
//   stderr:MSG   write MSG to stderr, then reply {}
//   die          exit(1) without replying
//   ignorecancel reply {}; from then on cancelFlow never gets a reply,
//                which lets tests exercise the kill path
//
// FAKE_SHIM_IGNORE_CANCEL=1 in the environment also enables the
// ignore-cancel mode from the start.

"use strict";

const readline = require("node:readline");

const rl = readline.createInterface({ input: process.stdin });
const inFlight = [];
let ignoreCancel = process.env.FAKE_SHIM_IGNORE_CANCEL === "1";

function reply(id, result) {
  process.stdout.write(JSON.stringify({ id, ok: true, result }) + "\n");
}

function replyError(id, error) {
  process.stdout.write(JSON.stringify({ id, ok: false, error }) + "\n");
}

function handleStep(id, params) {
  const script = params && typeof params.script === "string" ? params.script : "ok";
  if (script === "ok") {
    reply(id, {});
  } else if (script.startsWith("delay:")) {
    const ms = Number(script.slice("delay:".length));
    setTimeout(() => reply(id, {}), ms);
  } else if (script === "error") {
    replyError(id, {
      kind: "assert",
      message: "fake assertion failed",
      expected: "a",
      actual: "b",
    });
  } else if (script === "never") {
    inFlight.push(id);
  } else if (script === "ignorecancel") {
    ignoreCancel = true;
    reply(id, {});
  } else if (script.startsWith("stderr:")) {
    process.stderr.write(script.slice("stderr:".length) + "\n");
    reply(id, {});
  } else if (script === "die") {
    process.exit(1);
  } else {
    replyError(id, { kind: "internal", message: `unknown script '${script}'` });
  }
}

rl.on("line", (line) => {
  if (line.trim() === "") return;
  const request = JSON.parse(line);
  const { id, cmd, params } = request;
  switch (cmd) {
    case "hello":
      reply(id, { protocol: 1, playwrightVersion: "0.0.0-fake" });
      break;
    case "startFlow":
      reply(id, {});
      break;
    case "endFlow":
      reply(id, { blockedHosts: ["a.example", "b.example"], videoPath: null });
      break;
    case "cancelFlow":
      if (ignoreCancel) break;
      while (inFlight.length > 0) {
        replyError(inFlight.shift(), { kind: "cancelled", message: "flow cancelled" });
      }
      reply(id, {});
      break;
    case "shutdown":
      reply(id, {});
      process.exit(0);
      break;
    case "evalAction":
      handleStep(id, params);
      break;
    case "capture":
      reply(id, { value: "captured" });
      break;
    default:
      // Echo the params back so framing tests can inspect them.
      reply(id, { cmd, params });
  }
});
