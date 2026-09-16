// verify_ui.js: DOM repro of the served page. Real DOM semantics:
// unknown id => getElementById returns null (same as a real browser).
// Mocks backend: /api/docker returns items WITH and WITHOUT port.
// Asserts: renderCtl bug fixed, 打开 link rendered for ported items only,
// ctnload no longer "失败".
// 2026-08-16 update: makeEl gains closest() (initFs fullscreen wiring needs it);
// new asserts for fullscreen buttons/attrs, mobile menu, embed fail-state diagnosis.
const fs = require("fs");
// Served HTML dump location: FENGPAGE_TEST_HTML override, dev-machine fallback.
// Default to the checked-in panel bundle. FENGPAGE_TEST_HTML remains available
// for testing an explicitly served build, but a machine-local FlyGoLegacy dump can
// be stale and must not silently mask the current UI.
const defaultHtml = require("path").resolve(__dirname, "../panel/static/index.html");
const html = fs.readFileSync(process.env.FENGPAGE_TEST_HTML || defaultHtml, "utf-8");
const m = html.match(/<script>([\s\S]*?)<\/script>/);
if (!m) { console.log("FAIL: no <script> block found"); process.exit(3); }
const appJs = m[1];

// collect ids present in the HTML
const ids = new Set();
for (const x of html.matchAll(/id="([^"]+)"/g)) ids.add(x[1]);

const accessed = new Map();
function makeEl() {
  return {
    _text: "",
    textContent: "",
    innerHTML: "",
    disabled: false,
    style: {},
    hidden: false,
    dataset: {},
    classList: { toggle: () => {}, add: () => {}, remove: () => {} },
    closest: () => null, // added: initFs calls frame.closest('.frame-w')
    addEventListener: () => {},
    querySelector: () => null,
  };
}
const els = {};
const rootEl = {
  setAttribute: () => {},
  getAttribute: () => null,
  style: {},
  classList: { toggle: () => {}, add: () => {}, remove: () => {} },
};
global.document = {
  documentElement: rootEl,
  body: rootEl,
  addEventListener: () => {},
  createElement: () => makeEl(),
  getElementById: (id) => {
    accessed.set(id, (accessed.get(id) || 0) + 1);
    if (!ids.has(id)) return null; // real DOM: unknown id => null
    if (!els[id]) els[id] = makeEl();
    return els[id];
  },
  querySelectorAll: () => [],
  querySelector: () => null,
};
global.localStorage = { getItem: () => null, setItem: () => {} };
global.confirm = () => true;
global.alert = () => {};
global.setInterval = () => 0;
global.setTimeout = (fn) => { if (typeof fn === "function") fn(); return 0; }; // fast-forward retries
global.location = { href: "http://127.0.0.1:8388/", hostname: "127.0.0.1", protocol: "http:", origin: "http://127.0.0.1:8388", pathname: "/" };
global.window = global;
global.navigator = { clipboard: { writeText: async () => {} } };

const DOCKER = [
  { key: "portainer", name: "portainer", desc: "Docker 管理", port: 9443, icon: "🐳" },
  { key: "alist", name: "alist", desc: "网盘聚合", port: 5244, icon: "📁" },
  { key: "immich", name: "immich", desc: "相册(无端口项)", port: 0, icon: "📷" },
];
// /api/health/services mock: covers all four colors (green/yellow/red/gray)
const HEALTH_SVC = [
  { key: "alist", name: "alist", desc: "网盘聚合", port: 5244, icon: "📁", running: true, reachable: true, latency_ms: 12, color: "green", reason: "ok" },
  { key: "jellyfin", name: "jellyfin", desc: "媒体中心", port: 8096, icon: "📺", running: true, reachable: true, latency_ms: 350, color: "yellow", reason: "ok" },
  { key: "portainer", name: "portainer", desc: "Docker 管理", port: 9443, icon: "🐳", running: false, reachable: false, latency_ms: 3000, color: "red", reason: "timeout" },
  { key: "immich-machine-learning", name: "immich-machine-learning", desc: "AI 识别", port: 0, icon: "🧹", running: true, reachable: false, latency_ms: 0, color: "gray", reason: "no host port" },
];
global.fetch = async (url, opt) => {
  const u = String(url);
  const json = async () => {
    if (u === "/api/docker") return DOCKER;
    if (u === "/api/docker/control") return { ok: false, message: "Docker 未启动（WSL 已停机）" };
    if (u === "/api/health/services") return HEALTH_SVC;
    if (u === "/api/flags") return { ok: true, flags: {} };
    if (u === "/api/services") return [];
    if (u === "/api/health") return { ok: true, healthy: true };
    if (u === "/api/system") return { tasks: [], disks: [], shares: [], temp_c: null };
    if (u === "/api/system/usb") return { ok: true, message: "E: mounted" };
    if (u === "/api/autostart") return [];
    return {};
  };
  return { status: 200, json };
};
global.j = async (url, opt) => (await global.fetch(url, opt)).json();

process.on("unhandledRejection", (e) => { console.log("UNHANDLED REJECTION:", e && e.message); process.exit(2); });

try {
  eval(appJs);
} catch (e) {
  console.log("FAIL: eval threw:", e.message);
  process.exit(3);
}

(async () => {
  let fails = 0;
  const check = (name, cond) => { console.log((cond ? "PASS" : "FAIL") + " | " + name); if (!cond) fails++; };

  try {
    await loadDocker();
  } catch (e) {
    console.log("FAIL: loadDocker threw:", e.message);
    process.exit(4);
  }

  const btn = document.getElementById("btn-engine");
  const stt = document.getElementById("stt-engine");
  const ctn = document.getElementById("ctnload");
  const nav = document.getElementById("navload");
  const svc = document.getElementById("services").innerHTML;

  check("btn-engine text is 开 (not ...)", btn.textContent === "开");
  check("stt-engine text is 停机 (not —)", stt.textContent === "停机");
  check("ctnload is not 失败", ctn.textContent !== "失败");
  check("ctnload shows N/M 运行", /^\d+ \/ \d+ 运行$/.test(ctn.textContent));
  check("navload shows Docker 停机", nav.textContent === "Docker 停机");
  check("list has 打开 link for portainer", svc.includes('href="http://127.0.0.1:9443"') && svc.includes(">打开</a>"));
  check("list has 打开 link for alist", svc.includes('href="http://127.0.0.1:5244"'));
  check("port-0 item has NO 打开 link", !/href="http:\/\/127\.0\.0\.1:0"/.test(svc));
  check("all 打开 links carry target/rel", svc.includes('target="_blank" rel="noopener"'));
  check("启/停 button still present", svc.includes('data-k="portainer"') && svc.includes(">启</button>"));

  // ---- service card wall (/api/health/services) ----
  try {
    await loadSvcWall();
  } catch (e) {
    console.log("FAIL: loadSvcWall threw:", e.message);
    process.exit(4);
  }
  const sw = document.getElementById("svcwall").innerHTML;
  const swsub = document.getElementById("svcwall-sub").innerHTML;
  check("svcwall + svcwall-sub ids present in HTML", ids.has("svcwall") && ids.has("svcwall-sub"));
  check("swcard rendered per service (4 cards)", (sw.match(/class="swcard /g) || []).length === 4);
  check("green card class applied", sw.includes('class="swcard green"'));
  check("yellow card class applied", sw.includes('class="swcard yellow"'));
  check("red card class applied", sw.includes('class="swcard red"'));
  check("gray card class applied (port 0)", sw.includes('class="swcard gray"'));
  check("card shows latency ms for green", sw.includes("12ms"));
  check("card shows reason for red", sw.includes("timeout"));
  check("port-0 card says 未映射端口", sw.includes("未映射端口"));
  check("card click opens new tab via window.open", sw.includes("window.open('http://127.0.0.1:9443','_blank')"));
  check("port-0 card has no click handler", !sw.includes('window.open(\'http://127.0.0.1:0'));
  check("summary shows N/M 在线", /\d+\/\d+ 服务在线/.test(swsub));
  check("summary flags red alert", swsub.includes("有服务掉线") && swsub.includes('class="err"'));

  // ids accessed but absent from HTML => would be null in a real browser
  const missing = [];
  for (const [id, n] of accessed) if (!ids.has(id)) missing.push(id);
  check("no accessed-but-missing ids", missing.length === 0);
  if (missing.length) console.log("  missing:", JSON.stringify(missing));

  // ---- files page (alist/OpenList iframe) & FENG_IP/FENG_SCHEME fix ----
  check("files iframe present with alist svc", /id="lf-files" data-svc="alist" data-src="\$\{FENG_SCHEME\}:\/\/\$\{FENG_IP\}:5244"/.test(html));
  check("media iframe uses FENG_SCHEME+FENG_IP (8096)", /id="lf-media" data-svc="jellyfin" data-src="\$\{FENG_SCHEME\}:\/\/\$\{FENG_IP\}:8096"/.test(html));
  check("photos iframe uses FENG_SCHEME+FENG_IP (2283)", /id="lf-photos" data-svc="immich-server" data-src="\$\{FENG_SCHEME\}:\/\/\$\{FENG_IP\}:2283"/.test(html));
  check("files overlay ids present", ids.has("files-ov") && ids.has("files-start") && ids.has("files-retry"));
  check("switchView routes configured iframe pages", /if\(meta\)embed\(fid,meta\.ov,meta\.retry,meta\.start\)/.test(appJs));
  check("expandTpl interpolates FENG_SCHEME+FENG_IP", appJs.includes("replace(/\\$\\{FENG_SCHEME\\}/g,FENG_SCHEME)") && appJs.includes("replace(/\\$\\{FENG_IP\\}/g,FENG_IP)"));
  check("embed() expands via expandTpl", appJs.includes("const src=expandTpl(cur.dataset.src"));

  // ---- requirement 1: fullscreen ----
  check("3 iframes carry allowfullscreen + allow policy", ["lf-files","lf-media","lf-photos"].every(i => {
    const re = new RegExp('id="' + i + '"([^>]*?)allowfullscreen([^>]*?)allow="fullscreen; autoplay; picture-in-picture"');
    return re.test(html);
  }));
  check("fullscreen entry buttons present (files/media/photos)", ["files-fs","media-fs","photos-fs"].every(i => ids.has(i)));
  check("fs-exit small buttons present", ["files-fs-exit","media-fs-exit","photos-fs-exit"].every(i => ids.has(i)));
  check("rebuild keeps fullscreen attrs on iframe", appJs.includes("f.allowFullscreen=true") && appJs.includes("fullscreen; autoplay; picture-in-picture"));
  check("CSS pseudo-fullscreen fallback present", /\.frame-w\.fs-pseudo\s*\{/.test(html) && /body\.fs-active nav\{display:none\}/.test(html));
  check("fs button gated while overlay visible", appJs.includes("function setFs(on)") && appJs.includes("setFs(false)"));

  // ---- requirement 2: mobile hamburger menu ----
  check("menu-btn + mmenu present", ids.has("menu-btn") && ids.has("mmenu"));
  check("menu clones navi into dropdown", /cloneNode\(true\)/.test(appJs) && /appendChild\(c\)/.test(appJs));
  check("menu click collapses + switchView", /hidden\s*=\s*true;\s*switchView\(c\.dataset\.v\)/.test(appJs));
  check("mobile media query hides nav, shows menu-btn", /@media\s*\(max-width:899px\)/.test(html) && /nav\s*\{display:none\}/.test(html) && /#menu-btn\s*\{display:inline-flex/.test(html));
  check("desktop menu-btn hidden", /#menu-btn\{display:none\}/.test(html));

  // ---- requirement 3: embed fail-state diagnosis ----
  check("open-in-new-tab buttons present", ["files-open","media-open","photos-open"].every(i => ids.has(i)));
  check("showFail diagnoses /api/docker", appJs.includes("async function showFail()") && appJs.includes("await j('/api/docker')"));
  check("running-but-unreachable copy + window.open fallback", appJs.includes("端口访问不通") && appJs.includes("window.open(src,'_blank')"));

  // ---- M14 UI audit remediation ----
  check("top-bar tiles are hidden outside Overview", appJs.includes("tiles.hidden=!isOv"));
  check("narrow mobile tiles scroll as one row", /\.tiles\s*\{display:flex;flex-wrap:nowrap;overflow-x:auto/.test(html));
  check("focus-visible ring is defined", /:focus-visible\s*\{outline:2px solid var\(--accent\)/.test(html));
  check("theme-color and favicon metadata present", html.includes('name="theme-color"') && html.includes('rel="icon"'));
  check("shared error state includes reason and retry", appJs.includes("function loadError(err,retry)") && appJs.includes("load-retry"));
  check("sync page has explicit error and retry state", ids.has("sync2-err") && /loadError\(e,loadSyncPage\)/.test(appJs));

  console.log(fails === 0 ? "ALL CHECKS PASSED" : fails + " CHECK(S) FAILED");
  process.exit(fails === 0 ? 0 : 1);
})();
