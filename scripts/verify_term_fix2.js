// verify_term_fix2.js — round-2 verification: UTF-8 Chinese text + Ctrl+C
//
// DEPENDENCIES (all must be in place BEFORE running; run via main agent after
// deployment, do NOT run against the old binary):
//   1. New flygo-webcli running (Binary output frames + chcp 65001 session
//      setup + TERM/COLORTERM env). Old binary sends Text JSON frames whose
//      GBK/lossy text breaks every Chinese assertion below.
//   2. Frontend index.html terminal onmessage must handle Binary/Blob frames
//      (it only JSON.parse()s Text frames today -> Blob frames are dropped).
//      That patch is coordinated separately; until it lands, this script
//      FAILS on purpose and that is expected, not a webcli regression.
//
// Checks added on top of verify_term_fix.js (which has no Chinese-output case
// and no real Ctrl+C interrupt case):
//   A. echo 你好世界 -> "你好世界" visible, zero U+FFFD (�) anywhere
//   B. vim opens a UTF-8 file containing Chinese -> text renders correctly
//   C. Ctrl+C interrupts `ping -t 127.0.0.1` -> output stops, prompt returns
//
// Note: B and C exercise the full panel path (page -> panel ws proxy ->
// webcli). A raw-ws check against webcli:8788 is not included because the
// panel's ticket/token flow is the production path we must validate.
// Machine-specific paths come from env vars (PW_PATH / PW_SHELL /
// FENGPAGE_TERM_ENV / FENGPAGE_TEST_SHOTS / FENGPAGE_TEST_TMP /
// FENGPAGE_TEST_VIM) with dev-machine fallbacks.
function pw() {
  try { return require("playwright"); }
  catch (e) {
    if (process.env.PW_PATH) return require(process.env.PW_PATH);
    throw e;
  }
}
const { chromium } = pw();
const fs = require("fs");
const path = require("path");
const envFile = process.env.FENGPAGE_TERM_ENV
  || (fs.existsSync(path.join(__dirname, "..", "panel", "term_token.env"))
    ? path.join(__dirname, "..", "panel", "term_token.env")
    : (() => { throw new Error("term token env not found: set FENGPAGE_TERM_ENV"); })());
const env = fs.readFileSync(envFile, "utf-8");
const PASS = (env.match(/PASSWORD=(\S+)/) || [])[1];
if (!PASS) { console.log("FAIL | cannot read PASSWORD from " + envFile); process.exit(1); }
const OUT = process.env.FENGPAGE_TEST_SHOTS || "./shots";
const TMP = process.env.FENGPAGE_TEST_TMP || "./tmp_fix2";
fs.mkdirSync(OUT, { recursive: true });
fs.mkdirSync(TMP, { recursive: true });
const CN_FILE = path.join(TMP, "utf8_cn.txt");
fs.writeFileSync(CN_FILE, "你好世界 - UTF-8 content line\n第二行 mixed ASCII 123\n", { encoding: "utf8" });
function findShell2() {
  if (process.env.PW_SHELL) return process.env.PW_SHELL;
  try {
    const base = path.join(process.env.LOCALAPPDATA || "", "ms-playwright");
    const dirs = fs.readdirSync(base).filter(d => d.startsWith("chromium_headless_shell-")).sort().reverse();
    for (const d of dirs) {
      const c = path.join(base, d, "chrome-headless-shell-win64", "chrome-headless-shell.exe");
      if (fs.existsSync(c)) return c;
    }
  } catch (e) { /* Edge fallback */ }
  return "";
}
function findVim2() {
  if (process.env.FENGPAGE_TEST_VIM) return process.env.FENGPAGE_TEST_VIM;
  try {
    const out = require("child_process").execSync("where vim", { encoding: "utf8" });
    const first = out.split(/\r?\n/).map(x => x.trim()).filter(Boolean)[0];
    if (first && fs.existsSync(first)) return '"' + first + '"';
  } catch (e) { /* optional */ }
  return "";
}
const SHELL = findShell2();
const VIM = findVim2();
let fails = 0;
const check = (name, cond, extra) => { console.log((cond ? "PASS" : "FAIL") + " | " + name + (extra ? " | " + extra : "")); if (!cond) fails++; };

(async () => {
  let launchOpts = { headless: true };
  if (fs.existsSync(SHELL)) launchOpts.executablePath = SHELL;
  else launchOpts.channel = "msedge";
  const browser = await chromium.launch(launchOpts);
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  const errors = [];
  page.on("pageerror", e => errors.push("pageerror: " + e.message));
  page.on("console", m => { if (m.type() === "error") errors.push("console: " + m.text()); });

  const screenTxt = () => page.evaluate(() => {
    const b = TERM_TERM.buffer.active; let out = [];
    for (let y = 0; y < Math.min(b.length, 60); y++) out.push(b.getLine(y).translateToString(true));
    return out.join("\n");
  });
  const lockIn = async () => {
    await page.waitForSelector("#term-lock:not([hidden])", { timeout: 5000 });
    await page.fill("#term-pass", PASS);
    await page.click("#term-unlock");
    await page.waitForSelector("#term-host .xterm", { timeout: 10000 });
    await page.waitForFunction(() => document.getElementById("term-stt").textContent.includes("已连接"), { timeout: 8000 });
  };

  await page.goto("http://127.0.0.1:8388/", { waitUntil: "domcontentloaded" });
  await page.click('button.navi[data-v="terminal"]');
  await lockIn();
  await page.click("#term-host .xterm");
  await page.waitForTimeout(600);

  // ============ A. Chinese echo roundtrip (needs chcp 65001 + Binary frames) ============
  await page.keyboard.type("echo 你好世界");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(1200);
  const txtA = await screenTxt();
  check("A: echo 你好世界 -> text visible in xterm buffer", txtA.includes("你好世界"));
  check("A: no replacement char U+FFFD (�) anywhere on screen", !txtA.includes("\uFFFD"), txtA.slice(-200));
  await page.screenshot({ path: OUT + "/40_fix2_chinese_echo.png" });

  // ============ B. vim opens a UTF-8 file with Chinese ============
  await page.click("#term-host .xterm");
  await page.waitForTimeout(300);
  await page.keyboard.type(VIM + " " + CN_FILE);
  await page.keyboard.press("Enter");
  await page.waitForTimeout(2500);
  const txtB = await screenTxt();
  const altB = await page.evaluate(() => TERM_TERM.buffer.active.type);
  check("B: vim started (alternate screen)", altB === "alternate");
  check("B: Chinese file content rendered (你好世界 visible, no U+FFFD)", txtB.includes("你好世界") && !txtB.includes("\uFFFD"), txtB.slice(0, 200));
  await page.screenshot({ path: OUT + "/41_fix2_vim_chinese.png" });

  // quit vim
  await page.keyboard.press("Escape");
  await page.keyboard.type(":q!");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(1200);

  // ============ C. Ctrl+C interrupts ping -t (needs working \x03 path) ============
  await page.click("#term-host .xterm");
  await page.waitForTimeout(300);
  await page.keyboard.type("ping -t 127.0.0.1");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(3500); // >=2 replies at 1s interval
  const txtPing = await screenTxt();
  check("C: ping -t running (TTL replies visible)", txtPing.includes("TTL"), txtPing.slice(-120));
  await page.keyboard.press("Control+c");
  await page.waitForTimeout(1500);
  const snap1 = await screenTxt();
  await page.waitForTimeout(2000);
  const snap2 = await screenTxt();
  const stable = snap1 === snap2;
  check("C: Ctrl+C stops ping (screen stable for 2s)", stable);
  check("shell prompt visible again after interrupt", /[A-Za-z]:\\|[~$#>]/.test(snap2), snap2.slice(-120));
  await page.screenshot({ path: OUT + "/42_fix2_ctrl_c_ping.png" });

  console.log("PAGE ERRORS:", errors.length ? errors.slice(0, 8) : "(none)");
  if (errors.length) fails++;
  await browser.close();
  console.log(fails === 0 ? "ALL VERIFY-2 CHECKS PASSED" : fails + " CHECK(S) FAILED");
  console.log("NOTE: A/B/C all require the NEW webcli (Binary output + chcp 65001) deployed and");
  console.log("      frontend Binary-frame support landed. Failures here do NOT count against the");
  console.log("      backend fix until both are live.");
  process.exit(fails === 0 ? 0 : 1);
})().catch(e => { console.log("FATAL: " + (e && e.stack || e)); process.exit(2); });
