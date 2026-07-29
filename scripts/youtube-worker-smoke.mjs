import { spawn } from "node:child_process";
import { resolve } from "node:path";

const workerPath = resolve(
  process.env.WOTOHA_YOUTUBE_JS_WORKER ??
    "target/debug/wotoha-youtube-js-worker",
);
const playerSource = `
var _player = {};
(function(g) {
  function Param() { this.values = new Map(); }
  Param.prototype.set = function(key, value) { this.values.set(key, value); };
  Param.prototype.get = function(key) { return this.values.get(key); };
  Param.prototype.clone = function() { return this; };
  Param.prototype.transform = function() {
    const n = this.values.get("n");
    if (n) this.values.set("n", n.slice(1) + n[0]);
  };
  function solve(a, b, c) {
    if (c && decodeURIComponent(c) === "reject") throw new Error("rejected");
    const value = new Param();
    value.set("alr", "yes");
    return value;
  }
})(_player);
`;

function frame(request) {
  const body = Buffer.from(JSON.stringify(request));
  const header = Buffer.alloc(4);
  header.writeUInt32BE(body.length);
  return Buffer.concat([header, body]);
}

async function run() {
  const child = spawn(workerPath, [], {
    env: {},
    stdio: ["pipe", "pipe", "pipe"],
  });
  const stderr = [];
  child.stderr.on("data", (chunk) => stderr.push(chunk));
  const responses = [];
  let bytes = Buffer.alloc(0);
  const timeout = setTimeout(() => child.kill(), 20_000);
  const close = new Promise((accept) => child.once("close", accept));
  const received = new Promise((accept, reject) => {
    child.once("error", reject);
    child.stdout.on("data", (chunk) => {
      bytes = Buffer.concat([bytes, chunk]);
      while (bytes.length >= 4) {
        const length = bytes.readUInt32BE(0);
        if (length === 0) {
          reject(new Error("empty response frame"));
          return;
        }
        if (bytes.length < 4 + length) return;
        responses.push(JSON.parse(bytes.subarray(4, 4 + length).toString("utf8")));
        bytes = bytes.subarray(4 + length);
        if (responses.length === 2) {
          accept();
          return;
        }
      }
    });
    child.once("close", (code, signal) => {
      if (responses.length < 2) {
        reject(
          new Error(
            `worker exited early code=${code} signal=${signal}: ${Buffer.concat(stderr).toString("utf8").slice(0, 500)}`,
          ),
        );
      }
    });
  });
  child.stdin.write(
    Buffer.concat([
      frame({
        protocol_version: 1,
        request_id: 1,
        player_key: "smoke-player",
        player_source: playerSource,
        inputs: [
          { signature: null, n: "1234" },
          { signature: "reject", n: null },
        ],
        per_input_results: true,
      }),
      frame({
        protocol_version: 1,
        request_id: 2,
        player_key: "smoke-player",
        player_source: null,
        inputs: [{ signature: null, n: "abcd" }],
        per_input_results: true,
      }),
    ]),
  );
  await received;
  if (responses.length !== 2) throw new Error("wrong response count");
  if (
    responses[0].request_id !== 1 ||
    responses[0].results?.[0]?.output?.n !== "2341" ||
    typeof responses[0].results?.[1]?.error !== "string"
  ) {
    throw new Error("first response failed partial-result checks");
  }
  if (
    responses[1].request_id !== 2 ||
    responses[1].results?.[0]?.output?.n !== "bcda"
  ) {
    throw new Error("second response failed session-reuse checks");
  }
  child.kill();
  await close;
  clearTimeout(timeout);
  process.stdout.write(
    `${JSON.stringify({ status: "ok", responses: responses.length })}\n`,
  );
}

await run();
