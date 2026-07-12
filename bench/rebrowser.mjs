// Regression guard against rebrowser-bot-detector.net (CDP-automation tells).
// Drives agent-browser in the default headful "be-real" mode, triggers the
// interactive tests via isolated-world evaluate, then asserts no test is 🔴.
// Needs network + a display (headful). Usage: node bench/rebrowser.mjs [binary]
import { spawn } from "node:child_process";

const bin = process.argv[2] || "target/release/agent-browser";
const c = spawn(bin, [], { stdio: ["pipe", "pipe", "ignore"] });
let b = "";
const w = new Map();
c.stdout.on("data", (d) => {
  b += d;
  let i;
  while ((i = b.indexOf("\n")) >= 0) {
    const l = b.slice(0, i).trim();
    b = b.slice(i + 1);
    if (!l) continue;
    let m;
    try { m = JSON.parse(l); } catch { continue; }
    if (m.id && w.has(m.id)) { w.get(m.id)(m); w.delete(m.id); }
  }
});
let id = 0;
const s = (me, p) => {
  const my = ++id;
  c.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: my, method: me, params: p }) + "\n");
  return new Promise((r) => w.set(my, r));
};
const n = (me, p) => c.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: me, params: p }) + "\n");
const t = async (name, args) => (await s("tools/call", { name, arguments: args })).result?.content?.[0]?.text;

await s("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "rebrowser", version: "0" } });
n("notifications/initialized", {});

const nav = await t("browser_navigate", { url: "https://bot-detector.rebrowser.net/" });
if (!nav || !nav.startsWith("page ")) { console.error("navigate failed:\n" + nav); c.kill(); process.exit(2); }
await new Promise((r) => setTimeout(r, 6000));

// Trigger the interactive tests through the ISOLATED world (default). If our
// isolation is correct, these calls are invisible to the page and stay ⚪️.
await t("browser_evaluate", { page: "p1", expression: "document.getElementsByClassName('div').length" });
await t("browser_evaluate", { page: "p1", expression: "typeof window.dummyFn" });
await t("browser_evaluate", { page: "p1", expression: "document.getElementById('detections-json') ? 1 : 0" });
await new Promise((r) => setTimeout(r, 2500));

const raw = await t("browser_evaluate", {
  page: "p1",
  main_world: true,
  expression: "[...document.querySelectorAll('tr')].map(r => r.innerText.replace(/\\t/g,' ').trim()).filter(x => /^(🟢|🔴|⚪️)/.test(x))",
});
let rows = [];
try { rows = JSON.parse(raw); } catch {}
c.kill();

if (!rows.length) { console.error("no test rows scraped"); process.exit(2); }

console.log("\n  rebrowser-bot-detector — agent-browser (be-real)\n");
const red = [];
for (const r of rows) {
  const name = r.split(/\s+/).slice(1).join(" ").slice(0, 60);
  const mark = r.startsWith("🔴") ? "✗" : r.startsWith("🟢") ? "✓" : "·";
  console.log(`  ${mark}  ${name}`);
  if (r.startsWith("🔴")) red.push(name);
}
if (red.length) {
  console.log(`\n  DETECTED (🔴): ${red.join("; ")}\n`);
  process.exit(1);
}
console.log("\n  no 🔴 detections — clean\n");
process.exit(0);
