import assert from "node:assert/strict";
import { readFile, lstat } from "node:fs/promises";
import { userInfo } from "node:os";
import { chromium } from "playwright";

const base = process.env.MDDIFF_HOSTED_URL;
const runtimePath = process.env.MDDIFF_RUNTIME_JSON;
let phase = "setup";
let browser;

async function api(context, method = "GET", body) {
  const headers = {};
  if (method !== "GET") {
    const session = await context.request.get(base + "/auth/session");
    assert.equal(session.status(), 200, "The synthetic browser session expired.");
    headers.Origin = base;
    headers["X-CSRF-Token"] = (await session.json()).csrf_token;
  }
  const response = await context.request.fetch(base + "/api/work", { method, headers, ...(body === undefined ? {} : { data: body }) });
  assert.equal(response.status(), 200, "The owner's work API returned an unexpected status.");
  return response.json();
}

async function ready(page) {
  await page.waitForFunction(() => !document.querySelector("#auth-user").hidden && document.querySelector("#connection").textContent === "Conectado");
}

async function login(context, runtime, username) {
  const page = await context.newPage();
  page.setDefaultTimeout(20000);
  await page.goto(base + "/");
  await page.locator("#username").fill(runtime.users[username].username);
  await page.locator("#password").fill(runtime.users[username].password);
  await page.locator("#kc-login").click();
  await page.waitForURL(base + "/");
  await ready(page);
  return page;
}

async function shows(page, marker) {
  await page.waitForFunction((text) => document.querySelector("#rows").textContent.includes(text), marker);
}

try {
  assert.equal(new URL(base).hostname, "127.0.0.1");
  assert.equal(new URL(base).protocol, "http:");
  const metadata = await lstat(runtimePath);
  assert.ok(metadata.isFile() && !metadata.isSymbolicLink() && (metadata.mode & 0o077) === 0 && metadata.uid === userInfo().uid);
  const runtime = JSON.parse(await readFile(runtimePath, "utf8"));
  assert.equal(runtime.issuer, "http://localhost:8848/realms/mddiff-spike");
  browser = await chromium.launch({ headless: true, ...(process.env.MDDIFF_BROWSER_CHANNEL ? { channel: process.env.MDDIFF_BROWSER_CHANNEL } : {}) });
  const alice = await browser.newContext();
  const bob = await browser.newContext();
  const errors = [];
  for (const context of [alice, bob]) context.on("page", (page) => page.on("pageerror", () => errors.push(true)));
  const first = await login(alice, runtime, "alice");
  const second = await alice.newPage(); await second.goto(base + "/"); await ready(second);
  const other = await login(bob, runtime, "bob");
  const foreign = await api(bob);

  phase = "two own tabs display the same replacement";
  const before = await api(alice);
  await api(alice, "PUT", { expected_revision: before.revision,
    base: { label: "own-original.md", markdown: "# COORDINATION-ALICE-ORIGINAL\n" },
    candidate: { label: "own-proposal.md", markdown: "# COORDINATION-ALICE-PROPOSAL\n" } });
  for (const page of [first, second]) {
    await shows(page, "COORDINATION-ALICE-ORIGINAL");
    assert.equal(await page.locator("#base-name").textContent(), "own-original.md");
  }
  assert.deepEqual(await api(bob), foreign, "Another owner's work changed after replacement.");
  assert.ok(!(await other.locator("#rows").textContent()).includes("COORDINATION-ALICE"));

  phase = "real SSE reconnect catches a revision committed while disconnected";
  let entered, release;
  const waiting = new Promise((resolve) => { entered = resolve; });
  const held = new Promise((resolve) => { release = resolve; });
  const gate = async (route) => { entered(); await held; await route.continue(); };
  await first.route("**/api/events", gate);
  try {
    await first.evaluate(() => { window.coordinationOldStream = events; events.dispatchEvent(new Event("error")); });
    await Promise.race([waiting, new Promise((_, reject) => {
      const timer = setTimeout(() => reject(new Error("SSE reconnect never reached its controlled gate.")), 15000);
      timer.unref();
    })]);
    const current = await api(alice);
    await api(alice, "PUT", { expected_revision: current.revision,
      base: { label: "caught-up.md", markdown: "# COORDINATION-ALICE-AFTER-RECONNECT\n" }, candidate: current.candidate });
    await shows(second, "COORDINATION-ALICE-AFTER-RECONNECT");
    release();
    await first.waitForFunction(() => events !== window.coordinationOldStream && events.readyState === EventSource.OPEN);
    await ready(first);
    await shows(first, "COORDINATION-ALICE-AFTER-RECONNECT");
  } finally { release(); await first.unroute("**/api/events", gate); }

  phase = "deletion propagates to both own tabs without crossing accounts";
  const current = await api(alice);
  second.once("dialog", (dialog) => dialog.accept());
  await second.locator("#delete-work").click();
  for (const page of [first, second]) await page.waitForFunction(() => document.querySelector("#rows").childElementCount === 0 && document.querySelector("#base-name").textContent === "Sin archivo");
  assert.deepEqual(await api(alice), { revision: current.revision + 1, base: null, candidate: null });
  assert.deepEqual(await api(bob), foreign, "Another owner's work changed after deletion.");
  assert.ok(!(await other.locator("#rows").textContent()).includes("COORDINATION-ALICE"));

  phase = "logout closes both tabs of the same browser session";
  await first.locator("#logout-button").click();
  for (const page of [first, second]) {
    await page.waitForFunction(() => document.querySelector("#auth-user").hidden);
    assert.equal(await page.locator("#rows").textContent(), "");
  }
  assert.deepEqual(await api(bob), foreign, "Logging out one browser revoked another user's work.");

  phase = "a state response stays bound through an Alice to Bob to Alice cookie change";
  const switching = await browser.newContext();
  const oldTab = await login(switching, runtime, "alice");
  const ownWork = await api(switching);
  let otherWork = await api(bob);
  while (otherWork.revision <= ownWork.revision) {
    await api(bob, "PUT", { expected_revision: otherWork.revision,
      base: { label: "foreign-state.md", markdown: "# COORDINATION-FOREIGN-STATE\n" }, candidate: null });
    otherWork = await api(bob);
  }
  const ownCookie = (await switching.cookies(base)).find((cookie) => cookie.name === "mddiff-session");
  const otherCookie = (await bob.cookies(base)).find((cookie) => cookie.name === "mddiff-session");
  assert.ok(ownCookie && otherCookie && ownCookie.value !== otherCookie.value);
  await oldTab.evaluate(() => { events.close(); clearTimeout(reconnectTimer); });
  await oldTab.route("**/api/state", async (route) => {
    const response = await route.fetch();
    // Both SIDs remain valid; a post-response /auth/session check alone would miss this ABA change.
    await switching.addCookies([ownCookie]);
    await route.fulfill({ response });
  });
  await switching.addCookies([otherCookie]);
  await oldTab.evaluate(() => refresh());
  await oldTab.waitForFunction(() => document.querySelector("#auth-user").hidden);
  assert.equal(await oldTab.locator("#rows").textContent(), "", "A rebound response displayed another account's work.");
  assert.deepEqual(await api(bob), otherWork, "The rejected state read changed the other account's work.");
  await switching.close();
  assert.deepEqual(errors, [], "The coordination scenario raised a JavaScript error.");
  console.log("Browser coordination: two own tabs, controlled SSE reconnect, replace/delete/logout and session-bound ABA privacy passed.");
} catch {
  console.error(`Browser coordination failed during ${phase}; credential-bearing diagnostics were not logged.`);
  process.exitCode = 1;
} finally { await browser?.close(); }
