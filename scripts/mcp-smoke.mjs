// Minimal MCP stdio client to smoke-test the browser-rs server.
// Sends: initialize -> initialized -> tools/list -> tools/call browser_navigate
import { spawn } from "node:child_process";

const bin = process.argv[2] || "target/debug/browser-rs";
const child = spawn(bin, [], { stdio: ["pipe", "pipe", "inherit"] });

let buf = "";
const waiters = new Map();
child.stdout.on("data", (d) => {
  buf += d.toString();
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (!line) continue;
    let msg;
    try { msg = JSON.parse(line); } catch { continue; }
    if (msg.id && waiters.has(msg.id)) {
      waiters.get(msg.id)(msg);
      waiters.delete(msg.id);
    }
  }
});

let id = 0;
function send(method, params) {
  const myId = ++id;
  const line = JSON.stringify({ jsonrpc: "2.0", id: myId, method, params }) + "\n";
  child.stdin.write(line);
  return new Promise((res) => waiters.set(myId, res));
}
function notify(method, params) {
  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
}

const init = await send("initialize", {
  protocolVersion: "2025-06-18",
  capabilities: {},
  clientInfo: { name: "smoke", version: "0" },
});
console.log("initialize:", init.result?.serverInfo?.name, "| tools cap:", !!init.result?.capabilities?.tools);
notify("notifications/initialized", {});

const tools = await send("tools/list", {});
console.log("tools:", tools.result.tools.map((t) => t.name).join(", "));

const nav = await send("tools/call", {
  name: "browser_navigate",
  arguments: { url: "https://example.com" },
});
const text = nav.result?.content?.[0]?.text || JSON.stringify(nav.error);
console.log("--- browser_navigate result ---");
console.log(text.split("\n").slice(0, 12).join("\n"));

// act: click the first ref, then re-snapshot to verify navigation.
const click = await send("tools/call", {
  name: "browser_click",
  arguments: { page: "p1", ref: "e1" },
});
const clickResult = click.result?.content?.[0]?.text || JSON.stringify(click.error);
console.log("--- browser_click (with settle-diff) ---");
console.log(clickResult.split("\n").slice(0, 8).join("\n"));

const url = await send("tools/call", {
  name: "browser_evaluate",
  arguments: { page: "p1", expression: "location.href" },
});
console.log("--- location after click (settle-diff shown above) ---");
console.log(url.result?.content?.[0]?.text || JSON.stringify(url.error));

const fp = await send("tools/call", {
  name: "browser_fingerprint_check",
  arguments: { page: "p1" },
});
console.log("--- fingerprint check ---");
console.log(fp.result?.content?.[0]?.text || JSON.stringify(fp.error));

console.log("=== click settle-diff (raw) ===");
console.log((clickResult || "").split("\n").slice(0, 10).join("\n"));

child.kill();
process.exit(0);
