// Stealth benchmark: drive agent-browser (over MCP) against the bot detector,
// scrape its verdict, print a scorecard. Exit nonzero on any failed check so it
// can gate CI — this is the regression guard that makes browser + detector grow
// together.
//
// Usage: bun bench/run.mjs [path-to-agent-browser-binary]
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const bin = process.argv[2] || resolve(here, "../target/release/agent-browser");
const detector = "file://" + resolve(here, "detector.html");

// This bench validates the *headless injection fallback* (AB_STEALTH), which
// must be deterministic across machines/CI. The recommended default mode is
// headful with no injection — proven against a real detector by external.mjs.
const child = spawn(bin, [], {
  stdio: ["pipe", "pipe", "inherit"],
  env: { ...process.env, AB_HEADLESS: "1", AB_STEALTH: "1" },
});
let buf = "";
const waiters = new Map();
child.stdout.on("data", (d) => {
  buf += d.toString();
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (!line) continue;
    let m; try { m = JSON.parse(line); } catch { continue; }
    if (m.id && waiters.has(m.id)) { waiters.get(m.id)(m); waiters.delete(m.id); }
  }
});
let id = 0;
const send = (method, params) => {
  const myId = ++id;
  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: myId, method, params }) + "\n");
  return new Promise((res) => waiters.set(myId, res));
};
const notify = (method, params) =>
  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
const callText = async (name, args) => {
  const r = await send("tools/call", { name, arguments: args });
  return r.result?.content?.[0]?.text ?? JSON.stringify(r.error);
};

await send("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "bench", version: "0" } });
notify("notifications/initialized", {});

const navResult = await callText("browser_navigate", { url: detector });
if (!navResult.startsWith("page ")) {
  console.error("navigate did not return a page:\n" + navResult);
}

// Poll until the async checks have populated the global.
let result = null;
for (let i = 0; i < 30; i++) {
  const ready = await callText("browser_evaluate", { page: "p1", expression: "window.__DETECT_READY__ === true" });
  if (ready.trim() === "true") {
    const json = await callText("browser_evaluate", { page: "p1", expression: "JSON.stringify(window.__DETECT__)" });
    result = JSON.parse(JSON.parse(json)); // evaluate returns a JSON string value
    break;
  }
  await new Promise((r) => setTimeout(r, 200));
}

child.kill();

if (!result) {
  console.error("detector did not produce a result");
  process.exit(2);
}

console.log(`\n  agent-browser stealth benchmark — ${detector}\n`);
for (const c of result.checks) {
  console.log(`  ${c.pass ? "✓" : "✗"}  ${c.name.padEnd(24)} ${c.detail}`);
}
const ok = result.passed === result.total;
console.log(`\n  score: ${result.passed}/${result.total} ${ok ? "— looks human ✓" : "— LEAKS DETECTED ✗"}\n`);
process.exit(ok ? 0 : 1);
