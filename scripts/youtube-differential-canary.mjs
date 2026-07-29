import { execFile, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const workerPath = resolve(
  process.env.WOTOHA_YOUTUBE_JS_WORKER ?? "target/debug/wotoha-youtube-js-worker",
);
const ejsRun = resolve(process.env.WOTOHA_EJS_RUN ?? "vendor/ejs/run.ts");
const watchUrl = "https://www.youtube.com/watch?v=H7HmzwI67ec&hl=en";
const nChallenges = [
  "1234567890abcdef",
  "eabGFpsUKuWHXGh6FR4",
  "eabGF/ps%UK=uWHXGh6FR4",
];
const signatureChallenges = [
  "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_".repeat(2),
  "0123456789".repeat(11),
];

function playerUrlFromWatchHtml(html) {
  const match = html.match(/"(?:jsUrl|PLAYER_JS_URL)"\s*:\s*"([^"]+)"/);
  if (!match) throw new Error("watch page did not expose a Player URL");
  const decoded = JSON.parse(`"${match[1]}"`);
  const url = new URL(decoded, "https://www.youtube.com");
  if (
    url.protocol !== "https:" ||
    !["www.youtube.com", "youtube.com"].includes(url.hostname) ||
    !url.pathname.startsWith("/s/player/") ||
    !url.pathname.endsWith(".js")
  ) {
    throw new Error("watch page exposed an unsafe Player URL");
  }
  return url.href;
}

async function fetchText(url) {
  let lastError;
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    try {
      const response = await fetch(url, {
        headers: { "user-agent": "Mozilla/5.0 WotohaYouTubeCanary/1.0" },
        redirect: "follow",
        signal: AbortSignal.timeout(15_000),
      });
      if (response.ok) return response.text();
      if (response.status !== 429 && response.status < 500) {
        throw new Error(`HTTP ${response.status} for ${url}`);
      }
      lastError = new Error(`transient HTTP ${response.status} for ${url}`);
    } catch (error) {
      if (
        error instanceof Error &&
        error.message.startsWith("HTTP ") &&
        !error.message.startsWith("HTTP 429 ")
      ) {
        throw error;
      }
      lastError = error;
    }
    if (attempt < 3) {
      await new Promise((resolveDelay) =>
        setTimeout(resolveDelay, attempt * 1_000),
      );
    }
  }
  throw lastError ?? new Error(`failed to fetch ${url}`);
}

async function solveWithRustWorker(playerUrl, playerSource, playerHash) {
  const inputs = [
    ...nChallenges.map((n) => ({ signature: null, n })),
    ...signatureChallenges.map((signature) => ({ signature, n: null })),
  ];
  const request = Buffer.from(
    JSON.stringify({
      protocol_version: 1,
      player_key: `${playerUrl}#${playerHash}`,
      player_source: playerSource,
      inputs,
      per_input_results: true,
    }),
  );
  const header = Buffer.alloc(4);
  header.writeUInt32BE(request.length);
  const child = spawn(workerPath, [], {
    env: {},
    signal: AbortSignal.timeout(30_000),
    stdio: ["pipe", "pipe", "pipe"],
  });
  const stdout = [];
  const stderr = [];
  child.stdout.on("data", (chunk) => stdout.push(chunk));
  child.stderr.on("data", (chunk) => stderr.push(chunk));
  child.stdin.end(Buffer.concat([header, request]));
  const exit = new Promise((accept, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => {
      if (code === 0) accept();
      else reject(new Error(`Rust worker exited with code=${code} signal=${signal}`));
    });
  });
  const timer = setTimeout(() => child.kill("SIGKILL"), 15_000);
  try {
    await exit;
  } finally {
    clearTimeout(timer);
  }
  const framed = Buffer.concat(stdout);
  if (framed.length < 4) {
    throw new Error(`Rust worker returned no frame: ${Buffer.concat(stderr)}`);
  }
  const length = framed.readUInt32BE(0);
  if (length === 0 || framed.length !== length + 4) {
    throw new Error("Rust worker returned an invalid frame length");
  }
  const response = JSON.parse(framed.subarray(4).toString("utf8"));
  if (response.error) throw new Error(`Rust worker failed: ${response.error}`);
  if (!Array.isArray(response.results) || response.results.length !== inputs.length) {
    throw new Error("Rust worker returned an invalid result count");
  }
  const values = response.results.map((result, index) => {
    if (result.error || !result.output) {
      throw new Error(`Rust worker failed challenge ${index}: ${result.error}`);
    }
    return index < nChallenges.length ? result.output.n : result.output.signature;
  });
  return {
    n: Object.fromEntries(nChallenges.map((challenge, index) => [challenge, values[index]])),
    sig: Object.fromEntries(
      signatureChallenges.map((challenge, index) => [
        challenge,
        values[nChallenges.length + index],
      ]),
    ),
  };
}

async function solveWithOfficialEjs(playerFile) {
  const args = [
    "--experimental-strip-types",
    ejsRun,
    playerFile,
    ...nChallenges.map((challenge) => `n:${challenge}`),
    ...signatureChallenges.map((challenge) => `sig:${challenge}`),
  ];
  const { stdout } = await execFileAsync(process.execPath, args, {
    env: {},
    maxBuffer: 8 * 1024 * 1024,
    timeout: 30_000,
  });
  const output = JSON.parse(stdout);
  if (
    output.type !== "result" ||
    output.responses?.length !== 2 ||
    output.responses.some((response) => response.type !== "result")
  ) {
    throw new Error(`official EJS failed: ${JSON.stringify(output)}`);
  }
  return {
    n: output.responses[0].data,
    sig: output.responses[1].data,
  };
}

const temporary = await mkdtemp(join(tmpdir(), "wotoha-youtube-canary-"));
try {
  let watchHtml;
  let playerSource;
  let playerUrl;
  try {
    watchHtml = await fetchText(watchUrl);
    playerUrl = playerUrlFromWatchHtml(watchHtml);
    playerSource = await fetchText(playerUrl);
  } catch (error) {
    throw new Error(`network_unavailable: ${error.message}`);
  }
  if (Buffer.byteLength(playerSource) > 8 * 1024 * 1024) {
    throw new Error("Player source exceeded 8 MiB");
  }
  const playerHash = createHash("sha256").update(playerSource).digest("hex");
  const playerFile = join(temporary, "player.js");
  await writeFile(playerFile, playerSource);

  const official = await solveWithOfficialEjs(playerFile);
  const first = await solveWithRustWorker(playerUrl, playerSource, playerHash);
  const second = await solveWithRustWorker(playerUrl, playerSource, playerHash);
  if (JSON.stringify(first) !== JSON.stringify(official)) {
    throw new Error("wotoha_mismatch: Rust output differed from official EJS");
  }
  if (JSON.stringify(second) !== JSON.stringify(first)) {
    throw new Error("wotoha_nondeterministic: worker restart changed its output");
  }
  console.log(
    JSON.stringify({
      status: "ok",
      player_sha256: playerHash,
      n_cases: nChallenges.length,
      signature_cases: signatureChallenges.length,
    }),
  );
} finally {
  await rm(temporary, { recursive: true, force: true });
}
