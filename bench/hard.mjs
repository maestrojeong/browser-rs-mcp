// Run browser-rs (default headful "be-real" mode) against a hard detector
// and dump a verdict excerpt. Usage: node bench/hard.mjs <url> [waitMs]
import { spawn } from "node:child_process";
const bin = "target/release/browser-rs";
const url = process.argv[2];
const waitMs = Number(process.argv[3] || 9000);
const c = spawn(bin, [], { stdio: ["pipe", "pipe", "ignore"] }); // default = headful, no patching
let b=""; const w=new Map();
c.stdout.on("data",d=>{b+=d;let i;while((i=b.indexOf("\n"))>=0){const l=b.slice(0,i).trim();b=b.slice(i+1);if(!l)continue;let m;try{m=JSON.parse(l)}catch{continue}if(m.id&&w.has(m.id)){w.get(m.id)(m);w.delete(m.id)}}});
let id=0;const s=(me,p)=>{const my=++id;c.stdin.write(JSON.stringify({jsonrpc:"2.0",id:my,method:me,params:p})+"\n");return new Promise(r=>w.set(my,r))};
const n=(me,p)=>c.stdin.write(JSON.stringify({jsonrpc:"2.0",method:me,params:p})+"\n");
const t=async(name,args)=>(await s("tools/call",{name,arguments:args})).result?.content?.[0]?.text;
await s("initialize",{protocolVersion:"2025-06-18",capabilities:{},clientInfo:{name:"hard",version:"0"}});n("notifications/initialized",{});
const nav=await t("browser_navigate",{url});
if(!nav||!nav.startsWith("page ")){console.error("navigate failed:\n"+nav);c.kill();process.exit(2);}
await new Promise(r=>setTimeout(r,waitMs));
const txt=await t("browser_evaluate",{page:"p1",expression:"(document.body?document.body.innerText:'').replace(/\\n{2,}/g,'\\n').slice(0,2200)"});
let out=""; try{ out=JSON.parse(txt);}catch{ out=txt; }
console.log("\n===== "+url+" =====\n"+out);
c.kill();process.exit(0);
