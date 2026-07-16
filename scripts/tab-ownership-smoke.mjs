import { spawn } from "node:child_process";
import assert from "node:assert/strict";

const bin = process.argv[2] || "target/debug/browser-rs";
const child = spawn(bin, ["--headless"], { stdio: ["pipe", "pipe", "inherit"] });
const waiters = new Map();
let buffer = "";
let nextId = 0;

child.stdout.on("data", (chunk) => {
  buffer += chunk.toString();
  for (;;) {
    const newline = buffer.indexOf("\n");
    if (newline < 0) break;
    const line = buffer.slice(0, newline).trim();
    buffer = buffer.slice(newline + 1);
    if (!line) continue;
    const message = JSON.parse(line);
    const resolve = waiters.get(message.id);
    if (resolve) {
      waiters.delete(message.id);
      resolve(message);
    }
  }
});

function request(method, params) {
  const id = ++nextId;
  child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
  return new Promise((resolve) => waiters.set(id, resolve));
}

function notify(method, params) {
  child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method, params })}\n`);
}

async function call(name, args = {}) {
  const response = await request("tools/call", { name, arguments: args });
  if (response.error) throw new Error(JSON.stringify(response.error));
  if (response.result?.isError) {
    throw new Error(response.result.content?.[0]?.text || `${name} failed`);
  }
  return response.result?.content?.[0]?.text || "";
}

try {
  await request("initialize", {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "tab-ownership-smoke", version: "0" },
  });
  notify("notifications/initialized", {});

  await call("browser_navigate", {
    url: "data:text/html,<title>owner-test</title><button id=b onclick=\"window.open('data:text/html,<title>popup-test</title><h1>popup</h1>')\">open</button>",
  });
  assert.match(await call("browser_claim_page", { owner: "worker-a", page: "p1" }), /claimed p1/);
  assert.equal(await call("browser_evaluate", {
    page: "worker-a",
    expression: "document.title",
  }), '"owner-test"');

  const clickResult = await call("browser_click", {
    page: "worker-a",
    selector: "#b",
  });
  assert.match(clickResult, /new pages: p\d+/);
  await new Promise((resolve) => setTimeout(resolve, 250));
  const pages = await call("browser_pages");
  assert.match(pages, /p1  owner=worker-a/);

  const popup = pages.match(/^(p\d+)  owner=-/m)?.[1];
  assert.ok(popup, `popup page id missing:\n${pages}`);
  await call("browser_claim_page", { owner: "worker-b", page: popup });

  const conflict = await request("tools/call", {
    name: "browser_claim_page",
    arguments: { owner: "worker-c", page: popup },
  });
  assert.ok(conflict.error || conflict.result?.isError, "conflicting claim should fail");

  await call("browser_close_page", { page: "worker-b" });
  const afterClose = await call("browser_pages");
  assert.doesNotMatch(afterClose, /worker-b/);
  console.log("PASS tab ownership aliases, popup tracking, conflict rejection, claim cleanup");
} finally {
  child.kill();
}
