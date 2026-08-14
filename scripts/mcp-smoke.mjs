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
const toolNames = tools.result.tools.map((t) => t.name);
console.log("tools:", toolNames.join(", "));
for (const required of ["browser_activate_page", "browser_pointer", "browser_wheel"]) {
  if (!toolNames.includes(required)) {
    child.kill();
    throw new Error(`missing tool: ${required}`);
  }
}

const nav = await send("tools/call", {
  name: "browser_navigate",
  arguments: { url: "https://example.com" },
});
const text = nav.result?.content?.[0]?.text || JSON.stringify(nav.error);
console.log("--- browser_navigate result ---");
console.log(text.split("\n").slice(0, 12).join("\n"));

const activated = await send("tools/call", {
  name: "browser_activate_page",
  arguments: { page: "p1" },
});
console.log("--- browser_activate_page ---");
console.log(activated.result?.content?.[0]?.text || JSON.stringify(activated.error));

await send("tools/call", {
  name: "browser_evaluate",
  arguments: { page: "p1", expression: "document.body.style.minHeight='3000px'; 0" },
});
const wheel = await send("tools/call", {
  name: "browser_wheel",
  arguments: { page: "p1", delta_y: 300, x: 640, y: 400 },
});
console.log("--- browser_wheel ---");
console.log(wheel.result?.content?.[0]?.text || JSON.stringify(wheel.error));
const scrollY = await send("tools/call", {
  name: "browser_evaluate",
  arguments: { page: "p1", expression: "scrollY" },
});
const scrollValue = Number(scrollY.result?.content?.[0]?.text);
if (!(scrollValue > 0)) {
  child.kill();
  throw new Error(`mouseWheel did not move the page: scrollY=${scrollValue}`);
}
console.log("scrollY after mouseWheel:", scrollValue);

async function freshRef() {
  const snapshot = await send("tools/call", {
    name: "browser_snapshot",
    arguments: { page: "p1" },
  });
  const snapshotText = snapshot.result?.content?.[0]?.text || "";
  return snapshotText.match(/\[ref=([^\]]+)\]/)?.[1];
}

// Each mutation settles and refreshes refs, so fetch a fresh capability before
// every later action.
let firstRef = await freshRef();
if (!firstRef) {
  child.kill();
  throw new Error("navigation snapshot had no interactive ref");
}
for (const inputRoute of ["trusted", "dom_event"]) {
  const hover = await send("tools/call", {
    name: "browser_pointer",
    arguments: {
      page: "p1",
      action: "hover",
      input_route: inputRoute,
      ref: firstRef,
    },
  });
  if (hover.result?.isError || hover.error) {
    child.kill();
    throw new Error(`${inputRoute} pointer hover failed: ${JSON.stringify(hover)}`);
  }
  console.log(`browser_pointer hover (${inputRoute}):`, hover.result?.content?.[0]?.text);
  firstRef = await freshRef();
}
const click = await send("tools/call", {
  name: "browser_click",
  arguments: { page: "p1", ref: firstRef },
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
