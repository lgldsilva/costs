// Optional real-browser check: NODE_PATH=<playwright installation> node tests/report-viewport.cjs
// Parent owns browser execution; no browser package is added to the Rust project.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');

async function checkViewport(page, url) {
  for (const width of [1200, 390]) {
    await page.setViewportSize({ width, height: 900 });
    await page.goto(url);
    await page.selectOption('#provider', 'zhipu');
    await page.selectOption('#host', 'lab-node1');
    const longest = await page.locator('#model option').evaluateAll(options => options.reduce((a, b) => a.length > b.value.length ? a : b.value, ''));
    await page.selectOption('#model', longest);
    const dimensions = await page.evaluate(() => ({document: document.documentElement.scrollWidth, viewport: innerWidth}));
    assert(dimensions.document <= dimensions.viewport, `${width}px viewport overflows: ${JSON.stringify(dimensions)}`);
    const tokens = page.locator('#kpis > .card').nth(1).locator('strong');
    assert((await tokens.innerText()).includes('bi'));
    assert((await tokens.getAttribute('title')).includes('6.928.436.240'));
    await page.locator('#month').fill('2024-02');
    await page.locator('#month').dispatchEvent('change');
    assert.equal(await page.locator('#preset').inputValue(), 'month');
    assert((await page.locator('#period').innerText()).includes('01/02/2024'));
  }
}
module.exports = { checkViewport };

if (require.main === module) {
  (async () => {
    const { chromium } = require('playwright');
    const model = 'glm-flash/' + 'long-model-name-'.repeat(30);
    const payload = {month:'2024-01',today:'2024-01-15',generatedAt:'2024-01-15T12:00:00Z',range:null,hardScope:'',expected:[],runs:[],plans:[],rows:[{date:'2024-01-15',host:'lab-node1',agent:'opencode',provider:'zhipu',model,input:6928436240,output:0,read:0,write:0,cost:73.08}]};
    const template = fs.readFileSync('crates/costs-core/src/report/dashboard.html','utf8');
    const directory = fs.mkdtempSync(path.join(os.tmpdir(),'costs-viewport-'));
    const report = path.join(directory,'report.html');
    fs.writeFileSync(report,template.replace('/*PAYLOAD*/null',JSON.stringify(payload).replaceAll('<','\\u003c')));
    const browser = await chromium.launch({headless:true});
    try { await checkViewport(await browser.newPage(),require('node:url').pathToFileURL(report).href);console.log('PASS: 1200/390 viewport after long model selection, compact exact tokens, month transition'); }
    finally { await browser.close();fs.rmSync(directory,{recursive:true,force:true}); }
  })().catch(error => {console.error(error);process.exitCode=1;});
}
