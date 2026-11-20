// Lightweight offline behavior checks; no browser dependencies.
const fs = require('node:fs');
const vm = require('node:vm');
const assert = require('node:assert/strict');
class Element {
  constructor() { this.children=[]; this.value=''; this.style={}; this.textContent=''; this.listeners={}; }
  append(e) { this.children.push(e); }
  replaceChildren() { this.children=[]; }
  addEventListener(event,handler) { this.listeners[event]=handler; }
  setAttribute() {}
  scrollIntoView() {}
}
const elements = new Map();
const document = {getElementById(id) { if(!elements.has(id))elements.set(id,new Element());return elements.get(id); },createElement() {return new Element();}};
const row=(date,cost,input,read=0)=>({date,cost,input,read,output:0,write:0,host:'mac',agent:'codex',provider:'openai',model:'</script><img onerror=alert(1)>'});
const payload={month:'2024-01',today:'2024-01-15',range:null,hardScope:'all',expected:[],runs:[],plans:[{name:'Codex',fee:20,providers:['openai']},{name:'Go',fee:10,providers:['openai'],inconclusive:true}],rows:[row('2024-01-01',100,1),row('2024-01-08',1,1000,500),row('2024-01-09',1,1000),row('2023-12-31',500,1)]};
const html=fs.readFileSync('crates/costs-core/src/report/dashboard.html','utf8');
const script=html.split('<script>')[1].split('</script>')[0].replace('/*PAYLOAD*/null',JSON.stringify(payload));
const context=vm.createContext({document,Intl,console});
vm.runInContext(script,context);
function text(e){return e.textContent+' '+e.children.map(text).join(' ')}
assert.match(text(elements.get('kpis')),/01\/01\/2024/);
assert.match(text(elements.get('kpis')),/08\/01\/2024/);
assert.match(text(elements.get('plans')),/INCONCLUSIVO/);
assert.match(text(elements.get('rows')),/<img onerror/); // Literal text, not HTML.
vm.runInContext("$('preset').value='lastweek';render()",context);
assert.equal(vm.runInContext('period()[0]',context),'2024-01-08');
assert.equal(vm.runInContext('period()[1]',context),'2024-01-14');
vm.runInContext("$('preset').value='custom';$('from').value='2025-01-01';$('to').value='2025-01-02';render()",context);
assert.match(text(elements.get('kpis')),/Indisponível/);
assert.match(text(elements.get('empty')),/Sem dados/);
vm.runInContext("$('from').value='2025-01-03';render()",context);
assert.match(text(elements.get('dateError')),/válido/);
assert(!html.includes('innerHTML'));
console.log('PASS: date presets, clipped scope, distinct cost/token peaks, no-data, economics and text-only hostile names');
vm.runInContext("data.rows=[{...data.rows[0],date:'2023-12-31',cost:5,input:1},{...data.rows[0],date:'2024-01-01',cost:5,input:1}];$('from').value='2023-12-31';$('to').value='2024-01-01';render()",context);
assert.match(text(elements.get('kpis')),/25\/12\/2023 → 31\/12\/2023 \(parcial\)/);
// Regression: two calendar months must never be compared against one fee.
vm.runInContext("data.rows=[{...data.rows[0],date:'2024-01-15',cost:15},{...data.rows[0],date:'2024-02-15',cost:15}];$('from').value='2024-01-01';$('to').value='2024-02-29';render()",context);
assert.match(text(elements.get('plans').children[0]),/INCONCLUSIVO/);
assert.doesNotMatch(text(elements.get('plans').children[0]),/150%/);
assert.match(text(elements.get('period')),/01\/01\/2024.*29\/02\/2024/);
vm.runInContext("data.generatedAt='2024-01-15T12:00:00Z';data.expected=[{host:'mac',agent:'codex'},{host:'mac',agent:'claude'},{host:'mac',agent:''}];data.runs=[{host:'mac',agent:'codex',ts:'2024-01-15T09:59:00Z',status:'ok',message:'',lastSuccess:'2024-01-15T09:59:00Z'}];render()",context);
assert.match(text(elements.get('freshness')),/Desatualizado/);
assert.match(text(elements.get('freshness')),/Último sucesso: 2024-01-15T09:59:00Z/);
assert.match(text(elements.get('freshness')),/claude.*Sem registro/);
assert.match(text(elements.get('freshness')),/2024-01-15T12:00:00Z/);
assert.match(text(elements.get('freshness').children[1]),/121 minutos desde o sucesso/);
assert.match(text(elements.get('freshness').children[3]),/host \(não comprova cada agente\)/);
// The same host/agent key can appear in expected metadata, row data and run metadata.
// Preserve the first expected entry instead of letting runs overwrite its display agent.
vm.runInContext("data.expected=[{host:'expected-host',agent:'expected-agent'}];data.rows=[{...data.rows[0],host:'rows-host',agent:'rows-agent'}];data.runs=[{host:'runs-host',agent:'runs-agent',ts:'2024-01-15T12:00:00Z',status:'ok',message:'',lastSuccess:'2024-01-15T12:00:00Z'},{host:'expected-host',agent:'expected-agent',ts:'2024-01-15T12:00:00Z',status:'ok',message:'',lastSuccess:'2024-01-15T12:00:00Z'}];render()",context);
assert.equal(elements.get('freshness').children.length,4,'one header plus one row per distinct host/agent pair');
assert.match(text(elements.get('freshness').children[1]),/^expected-host \/ expected-agent/);
assert.doesNotMatch(text(elements.get('freshness').children[1]),/rows-agent|runs-agent/);
assert.match(text(elements.get('freshness').children[2]),/^rows-host \/ rows-agent/);
assert.match(text(elements.get('freshness').children[3]),/^runs-host \/ runs-agent/);
// A host-only expected entry does not associate any per-agent run with the host.
vm.runInContext("data.expected=[{host:'host-only',agent:''}];data.rows=[];data.runs=[{host:'host-only',agent:'codex',ts:'2024-01-15T12:00:00Z',status:'ok',message:'',lastSuccess:'2024-01-15T12:00:00Z'}];render()",context);
assert.equal(elements.get('freshness').children.length,3,'per-agent run must not add an implied host-only aggregate');
assert.match(text(elements.get('freshness').children[1]),/^host-only \/ host \(não comprova cada agente\) · Sem registro/);
assert.match(text(elements.get('freshness').children[2]),/^host-only \/ codex · Tentativa recente/);
// The threshold is two hours, not whole calendar days; a later error stays incomplete.
vm.runInContext("data.expected=[{host:'mac',agent:'codex'},{host:'mac',agent:'claude'},{host:'mac',agent:''}];data.runs=[{host:'mac',agent:'codex',ts:'2024-01-15T09:59:00Z',status:'ok',message:'',lastSuccess:'2024-01-15T09:59:00Z'}];render()",context);
vm.runInContext("data.runs[0].lastSuccess='2024-01-15T10:00:00Z';render()",context);
assert.doesNotMatch(text(elements.get('freshness').children[1]),/Desatualizado/);
vm.runInContext("data.runs[0].ts='2024-01-15T11:59:00Z';data.runs[0].status='error';render()",context);
assert.match(text(elements.get('freshness').children[1]),/Erro: frescor incompleto/);
assert.match(text(elements.get('freshness').children[1]),/Último sucesso: 2024-01-15T10:00:00Z/);
vm.runInContext("data.runs[0].status='ok';data.runs[0].lastSuccess='2024-01-15T12:00:00Z';render()",context);
assert.match(text(elements.get('freshness').children[1]),/Tentativa recente \(não comprova atualização da fonte\)/);
assert.match(text(elements.get('freshness').children[1]),/0 minutos desde o sucesso/);
assert.equal(elements.get('freshness').children[1].className||'','');
vm.runInContext("data.runs[0].lastSuccess='invalid';render()",context);
assert.match(text(elements.get('freshness').children[1]),/Desatualizado: frescor incompleto/);
assert.doesNotMatch(text(elements.get('freshness').children[1]),/minutos desde o sucesso/);
console.log('PASS: multi-month economics, live date header, 2h freshness and missing expected agents');
// Isolate the large-token KPI from intervening freshness fixtures, which omit token fields.
vm.runInContext("data.rows=[{...data.rows[0],date:'2024-01-15',input:2031388999,output:0,read:0,write:0}];$('from').value='2024-01-01';$('to').value='2024-01-31';render()",context);
let tokenValue=elements.get('kpis').children[1].children[1];
assert.equal(tokenValue.title,'2.031.388.999 tokens');
assert(tokenValue.textContent.length<15, 'large token KPI must be compact');
// The serialized dashboard row schema uses input/output/read/write; absent cost remains unknown.
vm.runInContext("data.rows=[{date:'2024-01-15',cost:null,input:2031388999,output:0,read:0,write:0,host:'mac',agent:'codex',provider:'openai',model:'fixture'}];render()",context);
tokenValue=elements.get('kpis').children[1].children[1];
assert.equal(tokenValue.title,'2.031.388.999 tokens','production-schema token counts must render numerically');
assert.equal(elements.get('missing').textContent.includes('2.031.388.999 tokens não precificados'),true,'unknown-cost tokens must remain counted and explicitly unpriced');
assert.match(text(elements.get('plans').children[0]),/INCONCLUSIVO/,'unknown costs must keep plan comparison inconclusive');
// Missing token fields are treated as zero, preserving the numeric KPI; explicit all-zero values remain zero.
vm.runInContext("data.rows=[{date:'2024-01-15',cost:null,host:'mac',agent:'codex',provider:'openai',model:'fixture'}];render()",context);
assert.equal(elements.get('kpis').children[1].children[1].title,'0 tokens');
assert.equal(elements.get('kpis').children[1].children[1].textContent,'0');
vm.runInContext("data.rows=[{date:'2024-01-15',cost:null,input:0,output:0,read:0,write:0,host:'mac',agent:'codex',provider:'openai',model:'fixture'}];render()",context);
assert.equal(elements.get('kpis').children[1].children[1].title,'0 tokens');

elements.get('month').value='2024-02';
elements.get('month').listeners.change();
assert.equal(elements.get('preset').value,'month','changing calendar month must select its preset');
// Author display rules must not override the semantic hidden state (including labels).
const css=html.split('<style>')[1].split('</style>')[0];
assert.match(css,/\[hidden\]\s*\{\s*display\s*:\s*none\s*!important\s*;?\s*\}/,'hidden must override author layout display rules');
assert.equal(elements.get('fromControl').hidden,true);
assert.equal(elements.get('toControl').hidden,true);

// Filtered peaks must retain the selected-calendar-range partial-week economics.
vm.runInContext("data.rows=[{...data.rows[0],date:'2024-01-03',cost:5,input:10},{...data.rows[0],date:'2024-01-08',cost:7,input:10}];$('from').value='2024-01-03';$('to').value='2024-01-08';$('preset').value='custom';render()",context);
assert.match(text(elements.get('kpis')),/01\/01\/2024 → 07\/01\/2024 \(parcial\)/);
assert.match(text(elements.get('kpis')),/08\/01\/2024 → 14\/01\/2024 \(parcial\)/);
assert.match(text(elements.get('daily')),/Escalas: USD/);
console.log('PASS: filtered week boundaries and independent scale legend');
