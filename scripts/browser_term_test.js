// browser_term_test.js: E2E of the FlyGo terminal page.
// Requires the panel running on 127.0.0.1:8388.
// Password is read from the term token env file (never hardcoded).
// Machine-specific paths come from env vars (PW_PATH / PW_SHELL /
// FENGPAGE_TERM_ENV); repo-relative defaults otherwise, Edge fallback last.
// Usage: node browser_term_test.js
const fs = require("fs");
function pw() {
  // PW_PATH optional override; otherwise plain require (repo-local
  // node_modules or NODE_PATH). No machine-specific fallback: a missing
  // playwright must fail loudly, not silently load a stale copy.
  try { return require("playwright"); }
  catch (e) {
    if (process.env.PW_PATH) return require(process.env.PW_PATH);
    throw e;
  }
}
const { chromium } = pw();
function termEnvFile() {
  // Repo checkout first (panel/term_token.env), then env override for
  // deployed layouts. Never a hardcoded absolute deploy path.
  const path = require("path");
  const repoLocal = path.join(__dirname, "..", "panel", "term_token.env");
  if (process.env.FENGPAGE_TERM_ENV) return process.env.FENGPAGE_TERM_ENV;
  if (fs.existsSync(repoLocal)) return repoLocal;
  throw new Error("term token env not found: set FENGPAGE_TERM_ENV");
}
const env = fs.readFileSync(termEnvFile(), "utf-8");
const pass = (env.match(/PASSWORD=(\S+)/) || [])[1];
if (!pass) { console.log("FAIL | cannot read PASSWORD from " + termEnvFile()); process.exit(1); }

const BASE = "http://127.0.0.1:8388/";
let fails = 0;
const check = (name, cond) => { console.log((cond ? "PASS" : "FAIL") + " | " + name); if (!cond) fails++; };

(async () => {
  // Installed playwright's bundled revision may be absent; point at the
  // locally installed chromium headless shell (PW_SHELL, fallback to the
  // original dev machine's copy), fallback to Edge.
  // Shell resolution: explicit PW_SHELL > newest ms-playwright headless
// shell under %LOCALAPPDATA% > Edge channel (existing fallback below).
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
  } catch (e) { /* fall through to Edge */ }
  return "";
}
const shell = findShell();
  let launchOpts = { headless: true };
  if (fs.existsSync(shell)) launchOpts.executablePath = shell;
  else launchOpts.channel = "msedge";
  const browser = await chromium.launch(launchOpts);
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  const errors = [];
  page.on("pageerror", e => errors.push("pageerror: " + e.message));

  // ---- desktop: nav -> lock -> wrong pw -> unlock -> xterm -> echo roundtrip ----
  await page.goto(BASE, { waitUntil: "domcontentloaded" });
  await page.click('button.navi[data-v="terminal"]');
  await page.waitForSelector("#term-lock:not([hidden])", { timeout: 5000 });
  check("navi 终端 -> lock card visible", true);

  await page.fill("#term-pass", "wrongpw");
  await page.click("#term-unlock");
  await page.waitForFunction(() => {
    const el = document.getElementById("term-lock-msg");
    return el && el.textContent.includes("密码错误");
  }, { timeout: 5000 });
  check("wrong password -> 密码错误 shown", true);

  await page.fill("#term-pass", pass);
  await page.click("#term-unlock");
  await page.waitForSelector("#term-host .xterm", { timeout: 10000 });
  check("correct password -> xterm mounted", true);
  await page.waitForTimeout(1000); // let WS + shell settle
  await page.click("#term-host .xterm"); // focus the terminal before typing
  await page.waitForTimeout(300);

  await page.keyboard.type("echo TERM_OK");
  await page.keyboard.press("Enter");
  const echoed = await page.waitForFunction(() => {
    try {
      const t = TERM_TERM; // top-level let in page script, visible to evaluate
      const b = t && t.buffer && t.buffer.active;
      if (!b) return false;
      for (let y = 0; y < b.length; y++) {
        const l = b.getLine(y);
        if (l && l.translateToString(true).includes("TERM_OK")) return true;
      }
      return false;
    } catch (e) { return false; }
  }, { timeout: 10000 }).then(() => true).catch(() => false);
  check("echo TERM_OK roundtripped through webcli", echoed);

  // status row shows 已连接
  const stt = await page.textContent("#term-stt");
  check("status row connected-ish (" + stt + ")", stt.includes("已连接") || stt.includes("会话"));

  // ---- mobile 375px: lock card must not overflow ----
  await page.setViewportSize({ width: 375, height: 700 });
  await page.waitForTimeout(400);
  const overflow = await page.evaluate(() => {
    const lock = document.getElementById("term-lock");
    const pass = document.getElementById("term-pass");
    return {
      docScroll: document.documentElement.scrollWidth,
      lockRectW: lock.getBoundingClientRect().width,
      passRectW: pass.getBoundingClientRect().width,
      viewport: window.innerWidth,
    };
  });
  check(
    "375px no horizontal overflow (doc " + overflow.docScroll + " vs " + overflow.viewport + ")",
    overflow.docScroll <= overflow.viewport + 1
  );
  check("375px pass input fits inside lock card", overflow.passRectW <= overflow.lockRectW + 1);

  // ---- password change via UI roundtrip (pass -> 6688 -> back to <pw>) ----
  // After change the ticket map is cleared -> lock card must reappear.
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.waitForSelector("#term-box:not([hidden])", { timeout: 5000 });
  await page.waitForTimeout(400);
  // change to 6688 (from unlocked state, use the terminal header's 修改密码 button)
  await page.click("#term-pw-toggle2");
  await page.waitForSelector("#term-lock:not([hidden])", { timeout: 5000 });
  check("terminal-header 修改密码 drops to lock screen with form", true);
  await page.fill("#term-old", pass);
  await page.fill("#term-new", "6688");
  await page.fill("#term-new2", "6688");
  await page.click("#term-pw-submit");
  await page.waitForFunction(() => {
    const el = document.getElementById("term-pw-msg");
    return el && el.textContent.includes("已修改");
  }, { timeout: 5000 });
  check("UI password change <pw>->6688 ok", true);
  await page.waitForSelector("#term-lock:not([hidden])", { timeout: 5000 });
  check("after password change lock card reappears (tickets cleared)", true);

  // unlock with new password 6688
  await page.fill("#term-pass", "6688");
  await page.click("#term-unlock");
  await page.waitForSelector("#term-host .xterm", { timeout: 10000 });
  check("unlock with new password 6688 works", true);

  // change back to <pw> via UI
  await page.click("#term-pw-toggle2");
  await page.waitForSelector("#term-lock:not([hidden])", { timeout: 5000 });
  await page.fill("#term-old", "6688");
  await page.fill("#term-new", pass);
  await page.fill("#term-new2", pass);
  await page.click("#term-pw-submit");
  await page.waitForFunction(() => {
    const el = document.getElementById("term-pw-msg");
    return el && el.textContent.includes("已修改");
  }, { timeout: 5000 });
  check("UI password change 6688->pass ok", true);
  await page.waitForSelector("#term-lock:not([hidden])", { timeout: 5000 });
  await page.fill("#term-pass", pass);
  await page.click("#term-unlock");
  await page.waitForSelector("#term-host .xterm", { timeout: 10000 });
  check("re-unlock with restored <pw> works", true);

  if (errors.length) {
    console.log("PAGE ERRORS:");
    errors.slice(0, 5).forEach(e => console.log("  " + e));
  }

  await browser.close();
  console.log(fails === 0 ? "ALL BROWSER CHECKS PASSED" : fails + " BROWSER CHECK(S) FAILED");
  process.exit(fails === 0 ? 0 : 1);
})().catch(e => { console.log("FATAL: " + (e && e.message)); process.exit(2); });
