"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const { chromium } = require("playwright");
const BASE = process.env.MDDIFF_HOSTED_URL;
let phase = "setup";
const check = (condition, message) => assert.ok(Boolean(condition), message);

async function main() {
  const runtime = JSON.parse(fs.readFileSync(process.env.MDDIFF_RUNTIME_JSON, "utf8"));
  const browser = await chromium.launch({ headless: true, ...(process.env.MDDIFF_BROWSER_CHANNEL ? { channel: process.env.MDDIFF_BROWSER_CHANNEL } : {}) });
  const contexts = [];
  const context = async () => { const c = await browser.newContext(); contexts.push(c); return c; };
  async function login(c, name) {
    const page = await c.newPage();
    await page.goto(BASE + "/");
    await page.locator("#username").fill(runtime.users[name].username);
    await page.locator("#password").fill(runtime.users[name].password);
    await page.locator("#kc-login").click();
    await page.waitForURL(BASE + "/");
    await page.waitForFunction(() => !document.querySelector("#base-paste").disabled);
  }
  async function api(c, path = "/api/work", method = "GET", data) {
    const session = await c.request.get(BASE + "/auth/session");
    check(session.status() === 200, "The owner session is unavailable.");
    const csrf = (await session.json()).csrf_token;
    const r = await c.request.fetch(BASE + path, { method, headers: { Origin: BASE, "X-CSRF-Token": csrf }, ...(data ? { data } : {}) });
    check(r.status() === 200, "An authenticated review operation failed.");
    return r.json();
  }
  try {
    phase = "owner opens a pair and creates a link";
    const owner = await context(); await login(owner, "alice");
    const before = await api(owner);
    const pair = await api(owner, "/api/work", "PUT", { expected_revision: before.revision,
      base: { label: "previo.md", markdown: "# Revisión privada de Alice\r\n\r\nEl texto previo.\r\n" },
      candidate: { label: "actual.md", markdown: "# Revisión privada de Alice\r\n\r\nEl texto actual.\r\n" } });
    const forbidden = await owner.request.post(BASE + "/api/review-links", { data: { expected_revision: pair.revision } });
    check(forbidden.status() === 403, "Cookie-based link creation bypassed CSRF.");
    const link = await api(owner, "/api/review-links", "POST", { expected_revision: pair.revision });
    const url = new URL(link.viewer_url);
    check(url.origin === BASE && url.pathname === "/review" && url.hash.length === 44 && !url.search && link.expires_in_seconds === 3600,
      "The generated link has an invalid scope or expiry.");
    const reader = await context(); const page = await reader.newPage();
    const errors = []; page.on("pageerror", () => errors.push(true));
    const requests = []; page.on("request", (r) => requests.push(new URL(r.url()).pathname));
    await page.goto(link.viewer_url);
    await page.locator("#document-view").waitFor({ state: "visible" });
    check((await page.locator("#rows").innerText()).includes("Revisión privada de Alice"), "A logged-out browser did not receive the pair.");
    check((await reader.cookies()).length === 0 && !requests.includes("/auth/session") && !requests.includes("/api/events"),
      "Review mode depends on a session or starts a normal owner event stream.");
    for (const id of ["login-button", "logout-button", "delete-work", "base-paste", "candidate-upload", "candidate-agent", "reload-button"]) {
      check(await page.locator("#" + id).isHidden(), "A review displayed account or editing controls.");
    }
    await page.locator("#source-tab").click();
    check(await page.locator("#pane-headers").isVisible(), "Read-only headers disappeared when switching format.");
    await page.locator("#reading-tab").click(); await page.locator("#diff-tab").click();
    await page.locator("#rendered-tab").click();
    const theme = await page.evaluate(() => document.documentElement.dataset.theme);
    await page.locator("#theme-toggle").click();
    check(await page.evaluate(() => document.documentElement.dataset.theme) !== theme, "Theme switching stopped working.");
    const cap = { "X-Mddiff-Review": url.hash.slice(1) };
    for (const path of ["/api/work", "/api/review-links", "/mcp"]) {
      const denied = await reader.request.post(BASE + path, { headers: cap, data: { expected_revision: pair.revision } });
      check(denied.status() === 401, "A review capability authorized an unrelated write.");
    }
    phase = "the open page clears when its one-hour timer expires";
    const clockContext = await context(); const clock = await clockContext.newPage();
    await clock.clock.install(); await clock.goto(link.viewer_url);
    await clock.locator("#document-view").waitFor({ state: "visible" });
    await clock.clock.fastForward(3600000);
    await clock.waitForFunction(() => document.querySelector("#rows").textContent === "" && document.querySelector("#connection").textContent === "Enlace finalizado");
    check(await clock.locator("#login-button").isHidden(), "Expiry offered a different account instead of closing the review.");
    phase = "a reused tab switches to the new fragment capability";
    const bob = await context(); await login(bob, "bob");
    const bobWork = await api(bob);
    const bobPair = await api(bob, "/api/work", "PUT", { expected_revision: bobWork.revision,
      base: { label: "bob-previo.md", markdown: "# Private Bob review" },
      candidate: { label: "bob-actual.md", markdown: "# Private Bob review changed" } });
    const bobLink = await api(bob, "/api/review-links", "POST", { expected_revision: bobPair.revision });
    const reused = await reader.newPage(); await reused.goto(link.viewer_url);
    await reused.locator("#document-view").waitFor({ state: "visible" });
    await reused.goto(bobLink.viewer_url);
    await reused.waitForFunction(() => document.querySelector("#rows").textContent.includes("Private Bob review"));
    check(!(await reused.locator("#rows").innerText()).includes("Revisión privada de Alice"), "Changing the fragment reused a different capability's document.");
    phase = "revision replacement invalidates existing links and conditional reads";
    const current = await api(owner);
    await api(owner, "/api/work", "PATCH", { expected_revision: current.revision, candidate: { label: "next.md", markdown: "# Another revision" } });
    await page.waitForFunction(() => document.querySelector("#rows").textContent === "", null, { timeout: 20000 });
    const stale = await reader.request.get(BASE + "/api/review", { headers: cap });
    check(stale.status() === 410, "An old link read a replacement document.");
    phase = "deletion invalidates a fresh link";
    const next = await api(owner);
    const last = await api(owner, "/api/review-links", "POST", { expected_revision: next.revision });
    await api(owner, "/api/work", "DELETE", { expected_revision: next.revision });
    const deleted = await reader.request.get(BASE + "/api/review", { headers: { "X-Mddiff-Review": new URL(last.viewer_url).hash.slice(1) } });
    check(deleted.status() === 410 && errors.length === 0, "Deleted work remained readable or the viewer raised JavaScript errors.");
    console.log("Review links: logged-out read-only browser, strict writes/CSRF, fragment capability, theme/format, expiry, replacement and deletion passed.");
  } finally {
    for (const c of contexts) await c.close();
    await browser.close();
  }
}
main().catch((error) => {
  console.error(`Review links failed during ${phase}: ${error instanceof assert.AssertionError ? error.message : "Browser operation failed; capability and credentials suppressed."}`);
  process.exitCode = 1;
});
