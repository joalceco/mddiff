"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");
const { chromium } = require("playwright");

const ROOT = path.resolve(__dirname, "../..");
const BASE = process.env.MDDIFF_HOSTED_URL;
const MAX = 2 * 1024 * 1024;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
let phase = "setup";

function check(condition, message) { assert.ok(Boolean(condition), message); }
function same(left, right) { return JSON.stringify(left) === JSON.stringify(right); }

async function until(predicate, message, timeout = 15000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await sleep(80);
  }
  check(false, message);
}

async function api(context, method = "GET", body) {
  const headers = {};
  if (method !== "GET") {
    const sessionResponse = await context.request.get(BASE + "/auth/session");
    check(sessionResponse.status() === 200, "Synthetic browser session expired unexpectedly.");
    headers.Origin = BASE;
    headers["X-CSRF-Token"] = (await sessionResponse.json()).csrf_token;
  }
  const response = await context.request.fetch(BASE + "/api/work", { method, headers, ...(body === undefined ? {} : { data: body }) });
  check(response.status() === 200, "The browser work API returned an unexpected status.");
  return response.json();
}

async function ready(page) {
  await page.waitForFunction(() => !document.querySelector("#auth-user").hidden && !document.querySelector("#base-paste").disabled);
}

async function login(context, runtime, username, entry = "/") {
  const page = await context.newPage();
  page.setDefaultTimeout(20000);
  await page.goto(BASE + entry);
  await page.locator("#username").fill(runtime.users[username].username);
  await page.locator("#password").fill(runtime.users[username].password);
  await page.locator("#kc-login").click();
  await page.waitForURL(BASE + "/");
  await ready(page);
  return page;
}

async function closed(page) {
  for (const side of ["base", "candidate"]) {
    if (await page.locator(`#${side}-text`).isVisible()) {
      await page.locator(`#${side}-paste`).click();
      await until(() => page.locator(`#${side}-text`).isHidden(), "Closing a text editor did not finish.");
    }
  }
}

async function paste(page, context, side, markdown) {
  await closed(page);
  await page.locator(`#${side}-paste`).click();
  await page.locator(`#${side}-text`).fill(markdown);
  await until(async () => (await api(context))[side]?.markdown === markdown, "Pasted Markdown was not committed automatically.");
  await until(() => page.locator(`#${side}-import-status`).evaluate((el) => !el.classList.contains("invalid") && el.textContent.includes("guardado")), "Paste did not reach its saved state.");
}

async function upload(page, context, side, name, bytes) {
  await closed(page);
  const choosing = page.waitForEvent("filechooser");
  await page.locator(`#${side}-upload`).click();
  await (await choosing).setFiles({ name, mimeType: "text/markdown", buffer: bytes });
  const text = bytes.toString("utf8");
  await until(async () => { const work = await api(context); return work[side]?.label === name && work[side]?.markdown === text; }, "A valid file did not preserve its name and exact UTF-8 contents.", 30000);
  await until(() => page.locator(`#${side}-import-status`).evaluate((el) => !el.classList.contains("invalid") && el.textContent.includes("guardado")), "Upload did not finish saving.", 30000);
}

async function rejectedFile(page, context, name, bytes, puts) {
  const before = await api(context);
  const count = puts.length;
  await page.locator("#base-file").setInputFiles({ name, mimeType: "text/markdown", buffer: bytes });
  await until(() => page.locator("#base-import-status").evaluate((el) => el.classList.contains("invalid")), "Invalid file did not produce an inline error.");
  await sleep(450);
  check(same(before, await api(context)), "Invalid file replaced the current pair.");
  check(puts.length === count, "Invalid file reached the write API.");
}

async function conflict(page, context, remotePage, puts) {
  await closed(page);
  const before = await api(context);
  await page.locator("#base-paste").click();
  const remoteText = "# Actualización externa sintética\n\nOtra revisión conserva sus cambios.\n";
  if (remotePage) await paste(remotePage, context, "candidate", remoteText);
  else await api(context, "PUT", { expected_revision: before.revision, base: before.base, candidate: { label: "Externa.md", markdown: remoteText } });
  const current = await api(context);
  const draft = "# Borrador local sintético\n\nEste texto no debe sobrescribir otra revisión.\n";
  const rejected = page.waitForResponse((response) => response.url() === BASE + "/api/work" && response.request().method() === "PUT" && response.status() === 409);
  await page.locator("#base-text").fill(draft);
  const response = await rejected;
  check(response.request().postDataJSON().expected_revision === before.revision, "Incoming SSE silently rebased an open draft.");
  await until(() => page.locator("#base-import-status").evaluate((el) => el.classList.contains("invalid")), "Conflict did not keep an inline error.");
  check(await page.locator("#base-text").inputValue() === draft, "Conflict destroyed the visible draft.");
  check(same(current, await api(context)), "Conflicting draft overwrote the current server pair.");
  const choosing = page.waitForEvent("filechooser");
  await page.locator("#base-upload").click();
  await (await choosing).setFiles([]);
  check(await page.locator("#base-text").isVisible() && await page.locator("#base-text").inputValue() === draft, "Cancelling the file picker destroyed a conflicted draft.");
  const count = puts.length;
  await page.locator("#base-text").fill(draft + "Aún visible.\n");
  await sleep(600); // Longer than debounce: a conflict must require closing and reopening.
  check(puts.length === count, "Conflicted editor retried without closing and reopening.");
  await closed(page);
  if (remotePage) await closed(remotePage);
  await paste(page, context, "base", draft);
  check((await api(context)).candidate.markdown === remoteText, "Retry lost the other side's current contents.");
}

async function queuedText(page, context) {
  await closed(page);
  const before = await api(context);
  const requests = [];
  let release;
  let started;
  const waiting = new Promise((resolve) => { started = resolve; });
  const gate = new Promise((resolve) => { release = resolve; });
  const handler = async (route) => {
    if (route.request().method() === "PUT") {
      requests.push(route.request().postDataJSON());
      if (requests.length === 1) { started(); await gate; }
    }
    await route.continue();
  };
  await page.route("**/api/work", handler);
  try {
    await page.locator("#base-paste").click();
    await page.locator("#base-text").fill("# Guardado A retenido\n");
    await Promise.race([waiting, sleep(10000).then(() => check(false, "First pending PUT never reached the route gate."))]);
    check(await page.locator("#base-text").isEditable(), "A pending save disabled further text entry.");
    await page.locator("#base-text").fill("# Guardado B más reciente\n");
    release();
    await until(async () => (await api(context)).base.markdown === "# Guardado B más reciente\n", "Queued edits did not preserve the newest text.");
    check(requests.length === 2 && requests[0].expected_revision === before.revision && requests[1].expected_revision === before.revision + 1, "Own queued saves did not advance revisions serially.");
    check(same((await api(context)).candidate, before.candidate), "Queued text edits changed the opposite document.");
  } finally { release(); await page.unroute("**/api/work", handler); }
  await closed(page);
}

async function switchedSession(browser, runtime, contexts, withBackoff) {
  const switching = await browser.newContext(); contexts.push(switching);
  const oldTab = await login(switching, runtime, "alice");
  await paste(oldTab, switching, "base", "# BORRADOR_PRIVADO_ALICE\n");
  if (withBackoff) {
    await until(() => oldTab.locator("#connection").textContent().then((text) => text === "Conectado"), "Initial SSE did not finish validating its browser session.");
    await oldTab.evaluate(() => {
      const Original = window.EventSource;
      window.__mddiff_reconnects = 0;
      window.EventSource = class extends Original {
        constructor(...args) { super(...args); window.__mddiff_reconnects++; }
      };
    });
    const checked = oldTab.waitForResponse((response) => response.url() === BASE + "/auth/session" && response.status() === 200);
    await oldTab.evaluate(() => events.dispatchEvent(new Event("error")));
    check((await (await checked).json()).user_label === "alice", "SSE error did not validate Alice before the account switch.");
    await oldTab.waitForFunction(() => reconnectTimer !== null);
  }
  await switching.clearCookies({ domain: "localhost" });
  const newTab = await login(switching, runtime, "bob", "/auth/login");
  if (withBackoff) {
    check(await oldTab.evaluate(() => window.__mddiff_reconnects === 0), "Synthetic account switch missed the reconnect backoff window.");
    await oldTab.waitForFunction(() => window.__mddiff_reconnects > 0);
  }
  await until(() => oldTab.locator("#auth-user").isHidden(), "An old tab silently adopted a new account after SID rotation.");
  check(await oldTab.locator("#base-text").inputValue() === "" && await oldTab.locator("#rows").textContent() === "", "An old account's visible draft or document survived session invalidation.");
  check(!(await newTab.locator("#rows").textContent()).includes("BORRADOR_PRIVADO_ALICE"), "A rotated account saw the previous user's draft.");
}

async function main() {
  check(new URL(BASE).hostname === "127.0.0.1" && new URL(BASE).protocol === "http:", "Only an explicit loopback test server is allowed.");
  const runtimePath = process.env.MDDIFF_RUNTIME_JSON || path.join(ROOT, "target/identity-spike/runtime.json");
  const stat = fs.lstatSync(runtimePath);
  check(stat.isFile() && !stat.isSymbolicLink() && (stat.mode & 0o077) === 0 && stat.uid === os.userInfo().uid, "Synthetic runtime file must remain private and owned.");
  const runtime = JSON.parse(fs.readFileSync(runtimePath, "utf8"));
  check(runtime.issuer === "http://localhost:8848/realms/mddiff-spike", "Refusing to send fixture credentials outside the local issuer.");
  const browser = await chromium.launch({ headless: true, ...(process.env.MDDIFF_BROWSER_CHANNEL ? { channel: process.env.MDDIFF_BROWSER_CHANNEL } : {}) });
  const contexts = [];
  const errors = [];
  const puts = [];
  const localPaths = [];
  let imageNetwork = false;
  try {
    const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
    contexts.push(context);
    context.on("request", (request) => {
      const url = new URL(request.url());
      if (url.origin === BASE && url.pathname === "/api/work" && request.method() === "PUT") puts.push(true);
      if (url.origin === BASE && ["/api/open", "/api/compare"].includes(url.pathname)) localPaths.push(true);
    });
    await context.route("https://mddiff-synthetic.invalid/**", async (route) => { imageNetwork = true; await route.abort(); });
    context.on("page", (page) => {
      page.on("pageerror", () => errors.push(true));
      page.on("console", (event) => {
        if (event.type() !== "error") return;
        const text = event.text();
        if (/Failed to load resource/.test(text) && /\b(400|401|409|413|500)\b/.test(text)) return;
        if (/Failed to load resource/.test(text) && /\b404\b/.test(text) && event.location().url.endsWith("/favicon.ico")) return;
        if (/Content Security Policy|Refused to load|Refused to execute/i.test(text)) return;
        errors.push(true);
      });
    });
    phase = "synthetic login and initial controls";
    const page = await login(context, runtime, "alice");
    check(await page.locator("#candidate-paste").isDisabled() && await page.locator("#candidate-upload").isDisabled(), "Candidate import was enabled without an original.");
    check(await page.locator("#base-text").isHidden() && await page.locator("#candidate-text").isHidden(), "Inline editors were visible initially.");
    for (const side of ["base", "candidate"]) {
      const accept = await page.locator(`#${side}-file`).getAttribute("accept");
      check(accept.includes(".md") && accept.includes(".markdown"), "File chooser omitted supported Markdown extensions.");
    }
    phase = "file imports preserve BOM CRLF and Unicode";
    const original = Buffer.from("\ufeff# Árbol 🌳\r\n\r\nTexto **verde** original.\r\n", "utf8");
    const candidate = Buffer.from("\ufeff# Árbol 🌲\r\n\r\nTexto **azul** propuesto.\r\n", "utf8");
    await upload(page, context, "base", "original.MD", original);
    await upload(page, context, "candidate", "propuesta.MARKDOWN", candidate);
    check((await api(context)).base.markdown === original.toString("utf8"), "Candidate upload changed the original.");
    check(await page.locator("#base-name").textContent() === "original.MD", "File label exposed a local path.");

    phase = "paste imports and keyboard focus";
    await page.locator("#base-paste").focus();
    await page.keyboard.press("Enter");
    await until(() => page.locator("#base-text").evaluate((el) => el === document.activeElement), "Opening a paste editor did not focus its textarea.");
    check(/Cerrar texto/.test(await page.locator("#base-paste").getAttribute("aria-label")), "The close action retained a paste-only accessible label.");
    await page.locator("#base-text").fill("# Pegado inmediato\n\nCerrar también guarda lo pendiente.\n");
    await page.locator("#base-paste").click();
    await until(async () => (await api(context)).base?.markdown.includes("Cerrar también guarda"), "Closing before debounce lost pasted contents.");
    await until(() => page.locator("#base-text").isHidden(), "Closing a pending editor did not complete.");
    check((await api(context)).candidate.markdown === candidate.toString("utf8"), "Pasting the original replaced its proposal.");
    await paste(page, context, "base", "# Original pegado\n\nUnicode: español y 日本語.\n");
    const pastedBase = (await api(context)).base;
    await paste(page, context, "candidate", "# Propuesta pegada\n\nUnicode: español, 日本語 y 🌍.\n");
    check(same((await api(context)).base, pastedBase), "Pasting the proposal changed the original.");
    check(await page.locator("#base-text").isHidden(), "Two inline editors remained active.");
    await closed(page);
    await page.locator("#base-paste").click();
    await page.locator("#base-text").waitFor({ state: "visible" });
    const clipboard = "# Portapapeles sintético\r\n\r\nConserva CRLF y 🌳.\r\n";
    await page.locator("#base-text").evaluate((field, text) => {
      field.focus(); field.select();
      const data = new DataTransfer(); data.setData("text/plain", text);
      field.dispatchEvent(new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: data }));
    }, clipboard);
    await until(async () => (await api(context)).base.markdown === clipboard, "A full clipboard paste normalized CRLF.");
    await closed(page);
    await queuedText(page, context);
    console.log("Hosted imports: upload/paste smoke, exact BOM/CRLF/Unicode, close flush and queued edits passed.");

    phase = "file validation and empty documents";
    await upload(page, context, "base", "vacío.md", Buffer.alloc(0));
    await upload(page, context, "candidate", "vacío-propuesta.md", Buffer.alloc(0));
    check(!await page.locator("#candidate-paste").isDisabled(), "An empty original disabled valid proposal import.");
    await upload(page, context, "base", "limite.md", Buffer.alloc(MAX, 97));
    await rejectedFile(page, context, "exceso.md", Buffer.alloc(MAX + 1, 97), puts);
    await rejectedFile(page, context, "invalido.md", Buffer.from([0xc3, 0x28]), puts);
    await rejectedFile(page, context, "extension.txt", Buffer.from("# Válido pero extensión incorrecta"), puts);
    await rejectedFile(page, context, "x".repeat(256) + ".md", Buffer.from("# Etiqueta larga"), puts);
    await rejectedFile(page, context, "control\n.md", Buffer.from("# Etiqueta inválida"), puts);
    await upload(page, context, "base", "seguridad.md", Buffer.from("# Seguridad\n\nTexto original.\n"));
    const beforeWide = await api(context);
    const beforeWidePuts = puts.length;
    await page.locator("#base-paste").click();
    await page.locator("#base-text").fill("é".repeat(MAX / 2 + 1));
    await until(() => page.locator("#base-import-status").evaluate((el) => el.classList.contains("invalid")), "UTF-8 byte overflow was accepted because its JS string length was small.");
    check(puts.length === beforeWidePuts && same(beforeWide, await api(context)), "UTF-8 byte overflow reached the API or replaced the pair.");
    await closed(page);

    phase = "sanitized rendering formats themes and navigation";
    const unsafe = '# Seguridad\n\nTexto propuesto.\n\n<img src="https://mddiff-synthetic.invalid/tracker.png" onerror="window.__mddiff_untrusted_ran=true">\n\n<script>window.__mddiff_untrusted_ran=true</script>\n';
    await upload(page, context, "candidate", "seguridad-propuesta.md", Buffer.from(unsafe));
    check(!await page.evaluate(() => window.__mddiff_untrusted_ran === true), "Untrusted Markdown executed in the viewer.");
    check(!imageNetwork, "Markdown initiated an outbound image request.");
    await page.locator("#source-tab").click();
    check((await page.locator("#rows").textContent()).includes("<script>"), "Source mode did not retain literal Markdown HTML.");
    await page.locator("#rendered-tab").click();
    const theme = await page.locator("html").getAttribute("data-theme");
    await page.locator("#theme-toggle").click();
    check(await page.locator("html").getAttribute("data-theme") !== theme, "Theme toggle did not change the view.");
    await page.locator("#next-change").click();
    check(!/^0\s*\//.test(await page.locator("#change-position").textContent()), "Diff navigation did not select a change.");
    await page.locator("#document-scroll").focus();
    await page.keyboard.press("j");
    await page.locator("#reading-tab").click();
    await page.locator("#diff-tab").click();

    phase = "independent account privacy";
    const bobContext = await browser.newContext(); contexts.push(bobContext);
    const bobPage = await login(bobContext, runtime, "bob");
    check((await api(bobContext)).base === null, "Another account inherited the original user's work.");
    const aliceBefore = await api(context);
    await paste(bobPage, bobContext, "base", "# Privado de Bob\n\nContenido sintético independiente.\n");
    check(same(aliceBefore, await api(context)), "Another account's import changed Alice's document pair.");
    check((await api(bobContext)).candidate === null && !(await bobPage.locator("#rows").textContent()).includes("Seguridad"), "Another user's document leaked into Bob's viewer.");
    check(!(await page.locator("#rows").textContent()).includes("Privado de Bob"), "Another account's event replaced Alice's visible document.");

    phase = "conflicts from API and another tab";
    await conflict(page, context, null, puts);
    const tab = await context.newPage(); await tab.goto(BASE + "/"); await ready(tab);
    await conflict(page, context, tab, puts);
    await tab.close();

    phase = "explicit server failure retains the draft and current pair";
    await closed(page);
    const beforeFault = await api(context);
    const failedText = "# Fallo de servidor sintético\n\nEl borrador permanece visible.\n";
    let injected = false;
    const fault = async (route) => {
      if (route.request().method() === "PUT" && !injected) {
        injected = true;
        await route.fulfill({ status: 500, contentType: "application/json", body: '{"error":"Fallo sintético de prueba"}' });
      } else await route.continue();
    };
    await page.route("**/api/work", fault);
    await page.locator("#base-paste").click();
    await page.locator("#base-text").fill(failedText);
    await until(() => page.locator("#base-import-status").evaluate((el) => el.classList.contains("invalid")), "Injected server failure was not shown inline.");
    check(injected && await page.locator("#base-text").inputValue() === failedText, "Server failure discarded the draft.");
    check(same(beforeFault, await api(context)), "Injected failure changed the committed pair.");
    await page.unroute("**/api/work", fault);
    await page.locator("#base-text").fill(failedText + "Corregido.\n");
    await until(async () => (await api(context)).base.markdown.endsWith("Corregido.\n"), "Editing after a server error did not recover.");
    await closed(page);

    phase = "mobile layout and screenshots";
    const screenshots = path.join(ROOT, "target/browser"); fs.mkdirSync(screenshots, { recursive: true });
    if (await page.locator("html").getAttribute("data-theme") !== "dark") await page.locator("#theme-toggle").click();
    await page.screenshot({ path: path.join(screenshots, "hosted-imports-desktop.png") });
    await page.setViewportSize({ width: 390, height: 844 });
    check(await page.locator("aside, .sidebar").count() === 0, "A sidebar returned to the mobile import flow.");
    check(await page.locator("#base-path").isHidden() && await page.locator("#candidate-path").isHidden(), "Hosted mobile view exposes server file paths.");
    await page.screenshot({ path: path.join(screenshots, "hosted-imports-mobile.png") });
    const geometry = await page.evaluate(() => ({ width: innerWidth, scrollWidth: document.documentElement.scrollWidth,
      overflowing: [...document.querySelectorAll("body *")].filter((el) => el.getBoundingClientRect().right > innerWidth + 1)
        .slice(0, 8).map((el) => ({ tag: el.tagName, id: el.id, width: Math.round(el.getBoundingClientRect().width) })) }));
    if (geometry.scrollWidth > geometry.width + 1) console.log("Synthetic mobile geometry: " + JSON.stringify(geometry));
    check(geometry.scrollWidth <= geometry.width + 1, "Mobile controls overflow the viewport.");
    await page.locator("#base-paste").click();
    await page.locator("#base-text").waitFor({ state: "visible" });
    await page.screenshot({ path: path.join(screenshots, "hosted-imports-mobile-editor.png") });
    check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), "An open mobile textarea overflowed the viewport.");
    await closed(page);
    await page.setViewportSize({ width: 1440, height: 1000 });

    phase = "old tab clears after another tab switches account";
    await switchedSession(browser, runtime, contexts, false);
    phase = "account switch during SSE reconnect backoff";
    await switchedSession(browser, runtime, contexts, true);
    console.log("Hosted imports: old tabs clear after SID rotation, including the SSE reconnect backoff window.");

    phase = "late upload after logout";
    const beforeLogout = await api(context);
    await page.evaluate(() => {
      const original = File.prototype.arrayBuffer;
      File.prototype.arrayBuffer = function () {
        if (this.name !== "late.md") return original.call(this);
        const file = this;
        window.__mddiff_late_started = true;
        return new Promise((resolve) => { window.__mddiff_release_late = async () => resolve(await original.call(file)); });
      };
    });
    await page.locator("#base-file").setInputFiles({ name: "late.md", mimeType: "text/markdown", buffer: Buffer.from("# No guardar después de salir\n") });
    await page.waitForFunction(() => window.__mddiff_late_started === true);
    await page.locator("#logout-button").click();
    await until(() => page.locator("#auth-user").isHidden(), "Logout did not clear the browser's private view.");
    const count = puts.length;
    const observer = await browser.newContext(); contexts.push(observer);
    await login(observer, runtime, "alice");
    check(same(beforeLogout, await api(observer)), "Logout changed the persisted pair.");
    await page.evaluate(() => window.__mddiff_release_late());
    await sleep(650);
    check(puts.length === count && same(beforeLogout, await api(observer)), "A late file read wrote work after logout.");
    check(localPaths.length === 0, "Hosted imports used a server-file API.");
    check(errors.length === 0, "Unexpected browser errors occurred during imports.");
    console.log("Hosted imports: file/paste, strict UTF-8/BOM/CRLF limits, safe rendering, accounts, conflicts, faults, mobile and logout passed.");
  } finally {
    for (const context of contexts) await context.close();
    await browser.close();
  }
}

main().catch((error) => {
  const detail = error instanceof assert.AssertionError ? error.message : "Browser operation failed; credential-bearing details were not logged.";
  console.error(`Hosted imports failed during ${phase}: ${detail}`);
  process.exitCode = 1;
});
