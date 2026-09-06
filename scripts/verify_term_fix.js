// verify_term_fix.js — post-fix verification (v2, no reloads: theme via button)
// Machine-specific paths come from env vars (PW_PATH / PW_SHELL /
// FENGPAGE_TEST_SHOTS / FENGPAGE_TEST_VIM); auto-detected otherwise;
// vim section skips when no vim is installed.
function pw() {
  try { return require("playwright"); }
  catch (e) {
    if (process.env.PW_PATH) return require(process.env.PW_PATH);
    throw e;
  }
}
const { chromium } = pw();
const fs = require("fs");
const OUT = process.env.FENGPAGE_TEST_SHOTS || "./shots";
fs.mkdirSync(OUT, { recursive: true });
function findShell() {
  if (process.env.PW_SHELL) return process.env.PW_SHELL;
  try {
    const path = require("path");
    const base = path.join(process.env.LOCALAPPDATA || "", "ms-playwright");
    const dirs = fs.readdirSync(base).filter(d => d.startsWith("chromium_headless_shell-")).sort().reverse();
    for (const d of dirs) {
      const c = path.join(base, d, "chrome-headless-shell-win64", "chrome-headless-shell.exe");
      if (fs.existsSync(c)) return c;
    }
  } catch (e) { /* Edge fallback below */ }
  return "";
}
function findVim() {
  if (process.env.FENGPAGE_TEST_VIM) return process.env.FENGPAGE_TEST_VIM;
  try {
    const out = require("child_process").execSync("where vim", { encoding: "utf8" });
    const first = out.split(/\r?\n/).map(x => x.trim()).filter(Boolean)[0];
    if (first && fs.existsSync(first)) return '"' + first + '"';
  } catch (e) { /* vim optional: TUI cases skip */ }
  return "";
}
const SHELL = findShell();
const VIM = findVim();
let fails = 0;
const check = (name, cond, extra) => { console.log((cond ? "PASS" : "FAIL") + " | " + name + (extra ? " | " + extra : "")); if (!cond) fails++; };

(async () => {
  let launchOpts = { headless: true };
  if (fs.existsSync(SHELL)) launchOpts.executablePath = SHELL;
  else launchOpts.channel = "msedge";
  const browser = await chromium.launch(launchOpts);
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  const errors = [];
  const sentFrames = [];
  let wsCount = 0;
  page.on("pageerror", e => errors.push("pageerror: " + e.message));
  page.on("console", m => { if (m.type() === "error") errors.push("console: " + m.text()); });
  page.on("websocket", ws => { wsCount++; ws.on("framesent", e => { try { sentFrames.push(e.payload.slice(0, 80)); } catch (_) {} }); });

  const screenTxt = () => page.evaluate(() => {
    const b = TERM_TERM.buffer.active; let out = [];
    for (let y = 0; y < Math.min(b.length, 30); y++) out.push(b.getLine(y).translateToString(true));
    return out.join("\n");
  });
  const lockIn = async () => {
    await page.waitForSelector("#term-lock:not([hidden])", { timeout: 5000 });
    await page.fill("#term-pass", "0909");
    await page.click("#term-unlock");
    await page.waitForSelector("#term-host .xterm", { timeout: 10000 });
    await page.waitForFunction(() => document.getElementById("term-stt").textContent.includes("已连接"), { timeout: 8000 });
  };

  // ============ LIGHT theme: terminal must be black bg + light text ============
  await page.goto("http://127.0.0.1:8388/", { waitUntil: "domcontentloaded" });
  await page.evaluate(() => { localStorage.setItem("fcp-theme", "light"); });
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.click('button.navi[data-v="terminal"]');
  await lockIn();
  await page.click("#term-host .xterm");
  await page.waitForTimeout(400);
  const lstate = await page.evaluate(() => {
    const q = s => getComputedStyle(document.querySelector(s)).backgroundColor;
    return { host: q("#term-host"), xterm: q("#term-host .xterm"), screen: q("#term-host .xterm-screen"), theme: document.documentElement.getAttribute("data-theme") };
  });
  console.log("LIGHT state:", JSON.stringify(lstate));
  check("light theme: opaque dark terminal bg (host/xterm/screen)", lstate.host === "rgb(11, 14, 20)" && lstate.xterm === "rgb(11, 14, 20)" && lstate.screen === "rgb(11, 14, 20)", JSON.stringify(lstate));
  await page.keyboard.type("echo LIGHT_OK");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(900);
  check("light theme: echo roundtrip", (await screenTxt()).includes("LIGHT_OK"));
  await page.screenshot({ path: OUT + "/30_fix_light_theme.png" });

  // ============ switch to dark theme via button, session must survive ============
  await page.click("#theme-btn"); // light -> dark
  await page.waitForTimeout(300);
  check("theme btn switches to dark", await page.evaluate(() => document.documentElement.getAttribute("data-theme")) === "dark");

  // ============ vim TUI (skipped when no vim installed) ============
  if (!VIM) { console.log("SKIP | vim TUI (no vim found; set FENGPAGE_TEST_VIM)"); }
  else {
  await page.click("#term-host .xterm");
  await page.waitForTimeout(300);
  await page.keyboard.type(VIM);
  await page.keyboard.press("Enter");
  await page.waitForTimeout(2200);
  const vim1 = await screenTxt();
  check("vim starts (alt-screen, tildes/content)", vim1.includes("Vi IMproved") || vim1.includes("~"), JSON.stringify(vim1.split("\n")[0].slice(0, 30)));
  await page.screenshot({ path: OUT + "/31_fix_vim.png" });

  // ============ view switch: session preserved, hidden resize sends nothing ============
  const wsBefore = wsCount;
  sentFrames.length = 0;
  await page.click('button.navi[data-v="docker"]');
  await page.waitForTimeout(400);
  await page.setViewportSize({ width: 1000, height: 700 });
  await page.waitForTimeout(1000);
  const garbage = sentFrames.filter(f => f.includes("resize"));
  check("hidden-view window resize: NO resize frames sent", garbage.length === 0, JSON.stringify(garbage));
  await page.click('button.navi[data-v="terminal"]');
  await page.waitForTimeout(1400);
  check("re-enter terminal: no new WS connection (session preserved)", wsCount === wsBefore, "ws " + wsBefore + " -> " + wsCount);
  const back = await screenTxt();
  check("re-enter: vim still running (alt-screen)", (await page.evaluate(() => TERM_TERM.buffer.active.type)) === "alternate", back.split("\n")[0].slice(0, 30));
  await page.screenshot({ path: OUT + "/32_fix_vim_after_nav.png" });

  // ============ visible resize while vim runs ============
  sentFrames.length = 0;
  await page.setViewportSize({ width: 900, height: 600 });
  await page.waitForTimeout(1000);
  check("vim survives visible resize", (await page.evaluate(() => TERM_TERM.buffer.active.type)) === "alternate");
  const rs = sentFrames.filter(f => f.includes("resize"));
  check("visible resize: exactly one deduped resize frame", rs.length === 1, JSON.stringify(rs));
  await page.screenshot({ path: OUT + "/33_fix_vim_after_resize.png" });

  // ============ quit vim -> shell back ============
  await page.click("#term-host .xterm");
  await page.waitForTimeout(200);
  await page.keyboard.press("Escape");
  await page.keyboard.type(":q!");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(1200);
  const afterQ = await screenTxt();
  check("vim :q! -> back at shell prompt", (await page.evaluate(() => TERM_TERM.buffer.active.type)) === "normal" && /[A-Za-z]:\\|[~$#>]/.test(afterQ), afterQ.split("\n")[0].slice(0, 30));
  } // end vim TUI block

  // ============ Ctrl+C delivered as \x03 input frame (panel responsibility) ============
  // Note: interrupting a running child (ping) depends on webcli's ConPTY input
  // handling — pre-existing backend limitation, verified via raw WS (bypassing panel).
  await page.click("#term-host .xterm");
  await page.waitForTimeout(200);
  sentFrames.length = 0;
  await page.keyboard.press("Control+c");
  await page.waitForTimeout(500);
  const cc = sentFrames.filter(f => f.includes("input"));
  check("Ctrl+C -> \\x03 input frame sent", cc.length >= 1 && cc[0].includes("\\u0003"), JSON.stringify(cc[0]));

  // ============ 375px mobile ============
  await page.setViewportSize({ width: 375, height: 700 });
  await page.waitForTimeout(1200);
  const mob = await page.evaluate(() => ({ cols: TERM_TERM.cols, rows: TERM_TERM.rows, hostW: document.getElementById("term-host").clientWidth, hostH: document.getElementById("term-host").clientHeight }));
  check("375px fit sane", mob.cols >= 20 && mob.cols <= 120 && mob.rows >= 10 && mob.rows <= 50, JSON.stringify(mob));
  await page.screenshot({ path: OUT + "/34_fix_mobile_375.png" });

  console.log("PAGE ERRORS:", errors.length ? errors.slice(0, 8) : "(none)");
  if (errors.length) fails++;
  await browser.close();
  console.log(fails === 0 ? "ALL VERIFY CHECKS PASSED" : fails + " CHECK(S) FAILED");
  process.exit(fails === 0 ? 0 : 1);
})().catch(e => { console.log("FATAL: " + (e && e.stack || e)); process.exit(2); });
