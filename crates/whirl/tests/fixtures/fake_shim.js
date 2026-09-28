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
// ignore-cancel mode from the start. FAKE_SHIM_VIDEO_SKIPPED=REASON makes
// startFlow start a 60 fps recording and endFlow report it skipped with
// that reason. FAKE_SHIM_VIDEO_BLANK=REASON makes startFlow start a 60 fps
// recording and endFlow report it saved, but blank for that reason; no
// file is written.

"use strict";

const readline = require("node:readline");

const rl = readline.createInterface({ input: process.stdin });
const inFlight = [];
let ignoreLifecycle = false;
let ignoreShutdown = false;
let ignoreCancel = process.env.FAKE_SHIM_IGNORE_CANCEL === "1";
const videoSkipped = process.env.FAKE_SHIM_VIDEO_SKIPPED ?? null;
const videoBlank = process.env.FAKE_SHIM_VIDEO_BLANK ?? null;
let videoPath = null;

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
  } else if (script === "ignorelifecycle") {
    ignoreLifecycle = true;
    reply(id, {});
  } else if (script === "ignoreshutdown") {
    ignoreShutdown = true;
    reply(id, {});
  } else if (script === "stopreading") {
    rl.pause();
    // Keep the process alive with an open stdin that no longer drains.
    setInterval(() => {}, 60_000);
    reply(id, {});
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
      if (ignoreLifecycle) break;
      reply(id, { protocol: 3, playwrightVersion: "0.0.0-fake" });
      break;
    case "startFlow":
      if (ignoreLifecycle) break;
      if (videoBlank !== null) videoPath = params.video.finalPath;
      reply(id, videoSkipped === null && videoBlank === null ? {} : { videoFps: 60 });
      break;
    case "endFlow":
      if (ignoreLifecycle) break;
      reply(id, {
        blockedHosts: ["a.example", "b.example"],
        videoPath,
        videoSkipped,
        videoBlank,
      });
      break;
    case "cancelFlow":
      if (ignoreCancel) break;
      while (inFlight.length > 0) {
        replyError(inFlight.shift(), { kind: "cancelled", message: "flow cancelled" });
      }
      reply(id, {});
      break;
    case "shutdown":
      if (ignoreShutdown) break;
      reply(id, {});
      process.exit(0);
      break;
    case "evalAction":
      handleStep(id, params);
      break;
    case "read":
      reply(id, { type: "value", value: "captured" });
      break;
    default:
      // Echo the params back so framing tests can inspect them.
      reply(id, { cmd, params });
  }
});
