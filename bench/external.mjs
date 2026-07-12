// Informational: run browser-rs against a real-world bot detector and
// summarize its verdict. Not a scored gate (external site, needs network).
// Usage: node bench/external.mjs [binary] [url]
import { spawn } from "node:child_process";
const bin = process.argv[2] || "target/release/browser-rs";
const url = process.argv[3] || "https://bot.sannysoft.com/";
const child = spawn(bin, [], { stdio: ["pipe","pipe","inherit"] });
let buf=""; const w=new Map();
child.stdout.on("data",d=>{buf+=d;let i;while((i=buf.indexOf("\n"))>=0){const l=buf.slice(0,i).trim();buf=buf.slice(i+1);if(!l)continue;let m;try{m=JSON.parse(l)}catch{continue}if(m.id&&w.has(m.id)){w.get(m.id)(m);w.delete(m.id)}}});
let id=0; const s=(method,params)=>{const my=++id;child.stdin.write(JSON.stringify({jsonrpc:"2.0",id:my,method,params})+"\n");return new Promise(r=>w.set(my,r))};
const n=(method,params)=>child.stdin.write(JSON.stringify({jsonrpc:"2.0",method,params})+"\n");
const t=async(name,args)=>(await s("tools/call",{name,arguments:args})).result?.content?.[0]?.text;
await s("initialize",{protocolVersion:"2025-06-18",capabilities:{},clientInfo:{name:"ext",version:"0"}}); n("notifications/initialized",{});
const nav=await t("browser_navigate",{url});
if(!nav||!nav.startsWith("page ")){console.error("navigate failed:\n"+nav);child.kill();process.exit(2);}
await new Promise(r=>setTimeout(r,2500)); // let async detectors settle
// Scrape sannysoft-style result table: rows colored green(passed)/red(failed).
const js = `(() => {
  const rows = [...document.querySelectorAll('table tr')];
  const out = [];
  for (const r of rows) {
    const cells = [...r.children].map(c => (c.textContent||'').trim());
    if (cells.length < 2) continue;
    const res = r.querySelector('.passed,.failed,.warn') || r.children[1];
    const cls = res.className || '';
    out.push({ name: cells[0], value: cells[1], status: /passed/.test(cls)?'pass':/failed/.test(cls)?'fail':/warn/.test(cls)?'warn':'?' });
  }
  return JSON.stringify(out);
})()`;
const raw = await t("browser_evaluate",{page:"p1",expression:js});
let rows=[]; try{ rows=JSON.parse(JSON.parse(raw)); }catch(e){}
child.kill();
console.log("\n  external detector: "+url+"\n");
const bad = rows.filter(r=>r.status==='fail');
for(const r of rows.slice(0,40)) console.log(`  ${r.status==='pass'?'✓':r.status==='fail'?'✗':'·'}  ${(r.name||'').slice(0,32).padEnd(32)} ${(r.value||'').slice(0,40)}`);
console.log(`\n  rows: ${rows.length}, failed: ${bad.length}\n`);
process.exit(0);
