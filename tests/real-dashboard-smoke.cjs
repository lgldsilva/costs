// Real-export browser smoke: generates a dashboard with the actual CLI binary
// from a SQLite store snapshot and exercises it in a real browser. Offline;
// the parent owns browser execution and no browser package joins the project.
//
// Usage: NODE_PATH=<playwright installation> node tests/real-dashboard-smoke.cjs \
//   --bin target/debug/costs --db store.sqlite --config config.toml [--month 2026-11] [--provider zai]
const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { pathToFileURL } = require('node:url');

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 2) {
    if (!argv[i].startsWith('--') || !argv[i + 1]) throw new Error(`usage: --bin <costs> --db <store> --config <toml> [--month YYYY-MM] [--provider ID]`);
    args[argv[i].slice(2)] = argv[i + 1];
  }
  for (const key of ['bin', 'db', 'config']) {
    assert(args[key], `--${key} is required`);
  }
  return args;
}

function generateReport(args, output) {
  const cli = [args.bin, '--db', args.db, '--config', args.config, 'report', '--format', 'html', '--output', output];
  if (args.month) cli.push('--month', args.month);
  if (args.provider) cli.push('--provider', args.provider);
  const result = spawnSync(cli[0], cli.slice(1), { encoding: 'utf8' });
  assert.equal(result.status, 0, `report generation failed: ${result.stderr}`);
  return fs.readFileSync(output, 'utf8');
}

async function checkDashboard(page, url) {
  const problems = [];
  page.on('pageerror', e => problems.push('pageerror: ' + e.message));
  page.on('console', m => { if (m.type() === 'error') problems.push('console.error: ' + m.text()); });
  await page.setViewportSize({ width: 1200, height: 900 });
  await page.goto(url);

  // Real data renders: KPIs, period header, daily chart and the four peak cards.
  await page.waitForSelector('#daily .day');
  assert((await page.locator('#kpis > .card').count()) >= 3, 'KPI cards rendered');
  const period = await page.locator('#period').innerText();
  assert(!period.includes('—'), 'period header filled: ' + period);
  const days = await page.locator('#daily .day').count();
  assert(days > 0, 'daily chart has days');
  assert.equal(await page.locator('.peaks .peak').count(), 4, 'four peak cards');

  // Dimension selects carry real values; AND filters narrow, never grow.
  const scoped = await page.locator('#rows tr').count();
  assert(scoped > 0, 'detail rows rendered');
  for (const dim of ['provider', 'agent', 'host', 'model']) {
    assert((await page.locator(`#${dim} option`).count()) >= 2, `#${dim} has real options`);
  }
  const providerOptions = await page.locator('#provider option').count();
  await page.selectOption('#provider', { index: providerOptions - 1 });
  const narrowed = await page.locator('#rows tr').count();
  assert(narrowed > 0 && narrowed <= scoped, `provider filter narrows rows (${scoped} -> ${narrowed})`);

  // Daily drilldown scopes the detail table to the clicked date.
  await page.selectOption('#provider', '');
  const day = page.locator('#daily .day button').first();
  const dayTitle = await day.getAttribute('title');
  await day.click();
  const local = dayTitle.split('-').reverse().join('/');
  assert((await page.locator('#scope').innerText()).includes(local), 'drilldown scopes to the clicked day');
  for (const cell of await page.locator('#rows tr td:first-child').allInnerTexts()) {
    assert.equal(cell, local, 'all rows are the selected day');
  }
  await day.click();

  // Plans carry confidence badges; freshness shows the generation timestamp.
  assert((await page.locator('#plans tr').count()) >= 1, 'plans table has rows');
  assert(/INCONCLUSIVO|Estimado/.test(await page.locator('#plans').textContent()), 'plans carry confidence badges');
  assert(/Gerado em/.test(await page.locator('#freshness').textContent()), 'freshness shows generation time');

  // Search narrows; both desktop and phone viewports stay within bounds.
  await page.fill('#search', 'claude');
  assert((await page.locator('#rows tr').count()) <= scoped, 'search narrows rows');
  await page.fill('#search', '');
  for (const width of [1200, 390]) {
    await page.setViewportSize({ width, height: 900 });
    const dims = await page.evaluate(() => ({ doc: document.documentElement.scrollWidth, vw: innerWidth }));
    assert(dims.doc <= dims.vw, `${width}px viewport overflows: ${JSON.stringify(dims)}`);
  }
  assert.deepEqual(problems, [], 'no browser errors: ' + problems.join(' | '));
  return { days, scoped };
}

if (require.main === module) {
  (async () => {
    const args = parseArgs(process.argv.slice(2));
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'costs-real-smoke-'));
    const report = path.join(directory, 'dashboard.html');
    try {
      const document = generateReport(args, report);
      assert(!document.includes('src=http'), 'offline document pulls no external resources');
      const { chromium } = require('playwright');
      const browser = await chromium.launch({ headless: true });
      try {
        const stats = await checkDashboard(await browser.newPage(), pathToFileURL(report).href);
        console.log(`PASS: real-export dashboard smoke — ${stats.days} days, ${stats.scoped} scoped rows`);
      } finally { await browser.close(); }
    } finally { fs.rmSync(directory, { recursive: true, force: true }); }
  })().catch(error => { console.error(error.message); process.exit(1); });
}

module.exports = { checkDashboard, generateReport };
