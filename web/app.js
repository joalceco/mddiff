"use strict";

const $ = (id) => document.getElementById(id);
function applyTheme(theme) {
  document.documentElement.dataset.theme = theme;
  $("theme-toggle").textContent = theme === "dark" ? "Modo claro" : "Modo oscuro";
}
try { applyTheme(localStorage.getItem("mddiff-theme") === "light" ? "light" : "dark"); }
catch { applyTheme("dark"); }
$("theme-toggle").addEventListener("click", () => {
  const theme = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  applyTheme(theme);
  try { localStorage.setItem("mddiff-theme", theme); } catch {}
});

let session = null;
let acceptedRevision = -1;
let view = "reading";
let format = "rendered";
let changes = [];
let changeIndex = -1;
let busy = false;
let openTimer = null;
let inputVersion = 0;
let pathsDirty = false;
let authentication = null;
let hosted = false;
let events = null;

async function request(path, body) {
  const response = await fetch(path, body === undefined ? {} : {
    method: "POST", headers: { "Content-Type": "application/json", ...(authentication ? { "X-CSRF-Token": authentication.csrf_token } : {}) }, body: JSON.stringify(body),
  });
  const text = await response.text();
  let data;
  try { data = JSON.parse(text); } catch { data = null; }
  if (!response.ok) {
    if (hosted && [401, 403].includes(response.status)) endSession();
    throw new Error(data?.error || data?.message || text || `Error ${response.status}`);
  }
  return data;
}

function showError(error) {
  $("error").textContent = error.message || String(error);
  $("error").hidden = false;
}

function setBusy(value) {
  busy = value;
  for (const id of ["reload-button", "example-button"]) {
    $(id).disabled = value;
    $(id).setAttribute("aria-busy", String(value));
  }
  $("reload-button").disabled = value || !$("base-path").value.trim();
}

async function openFiles(example = false) {
  if (hosted) return;
  clearTimeout(openTimer);
  openTimer = null;
  if (busy) return;
  if (example) {
    $("base-path").value = "examples/base.md";
    $("candidate-path").value = "examples/propuesta.md";
    pathsDirty = true;
    inputVersion++;
  }
  const base = $("base-path").value.trim();
  const candidate = $("candidate-path").value.trim();
  if (!base) { pathsDirty = false; return; }
  const version = inputVersion;
  const compare = Boolean(candidate);
  setBusy(true);
  $("error").hidden = true;
  try {
    const data = await request(compare ? "/api/compare" : "/api/open", compare
      ? { base_path: base, candidate_path: candidate } : { path: base });
    if (data?.comparison) applySession(data);
    else await refresh();
  } catch (error) { showError(error); }
  finally {
    setBusy(false);
    if (inputVersion === version) pathsDirty = false;
    else if (!openTimer) openFiles();
  }
}

function applySession(data) {
  if (!data || data.revision <= acceptedRevision) return;
  acceptedRevision = data.revision;
  session = data;
  view = data.candidate_path ? "diff" : "reading";
  if (!pathsDirty) {
    $("base-path").value = data.base_path || "";
    $("candidate-path").value = data.candidate_path || "";
  }
  $("error").hidden = true;
  $("document-scroll").scrollTop = 0;
  render();
}

async function refresh() {
  try {
    const data = await request("/api/state");
    if (!hosted || authentication) applySession(data);
  }
  catch (error) { showError(error); }
}

function fileName(path) { return path?.split(/[\\/]/).pop() || ""; }

function cell(block, status) {
  const node = document.createElement("div");
  node.className = `cell ${status}`;
  if (!block) { node.classList.add("placeholder"); node.setAttribute("aria-hidden", "true"); return node; }
  if (status) {
    const sign = document.createElement("span");
    sign.className = "change-sign";
    sign.textContent = status === "added" ? "+" : "−";
    sign.setAttribute("aria-label", status === "added" ? "Bloque añadido" : "Bloque eliminado");
    node.append(sign);
  }
  const content = document.createElement("div");
  content.className = "cell-content";
  if (format === "source") {
    const label = document.createElement("span");
    label.className = "line-label";
    label.textContent = `L${block.start_line}–${block.end_line}`;
    const pre = document.createElement("pre");
    const code = document.createElement("code");
    if (status && block.highlighted_source) {
      for (const part of block.highlighted_source) {
        if (!part.changed) { code.append(document.createTextNode(part.text)); continue; }
        const mark = document.createElement("mark");
        mark.className = "diff-change";
        mark.textContent = part.text;
        code.append(mark);
      }
    } else code.textContent = block.source;
    pre.append(code);
    content.append(label, pre);
  } else {
    // Rust sanitizes both HTML variants; source stays in text nodes.
    content.innerHTML = status && block.highlighted_html ? block.highlighted_html : block.html;
    for (const link of content.querySelectorAll("a")) {
      link.target = "_blank";
      link.rel = "noopener noreferrer";
    }
    if (!block.html.trim() && status) {
      const note = document.createElement("span");
      note.className = "source-note";
      note.textContent = "Espacios o referencias · consulta Fuente";
      content.append(note);
    }
  }
  node.append(content);
  return node;
}

function render() {
  const comparison = session?.comparison;
  const hasDocument = Boolean(comparison);
  const isDiff = hasDocument && view === "diff" && Boolean(session.candidate_path);
  $("empty").hidden = hasDocument;
  $("review-bar").hidden = !hasDocument;
  $("document-view").hidden = !hasDocument;
  $("document-view").classList.toggle("reading", !isDiff);
  $("rows").classList.toggle("source", format === "source");
  setBusy(busy);
  $("diff-tab").disabled = !session?.candidate_path;
  for (const [id, selected] of [["reading-tab", !isDiff], ["diff-tab", isDiff], ["rendered-tab", format === "rendered"], ["source-tab", format === "source"]]) {
    $(id).classList.toggle(["reading-tab", "diff-tab"].includes(id) ? "active" : "selected", selected);
    $(id).setAttribute("aria-pressed", String(selected));
  }
  $("base-name").textContent = fileName(session?.base_path) || "Sin archivo";
  $("candidate-name").textContent = fileName(session?.candidate_path) || "Sin archivo";
  if (!hasDocument) return;
  const fragment = document.createDocumentFragment();
  let rowGroup;
  changes = [];
  changeIndex = -1;
  for (const row of comparison.rows) {
    if (!isDiff && !row.left) continue;
    const node = document.createElement("div");
    node.className = "diff-row";
    const changed = row.kind !== "equal";
    node.append(cell(row.left, isDiff && changed && row.left ? "removed" : ""));
    if (isDiff) node.append(cell(row.right, changed && row.right ? "added" : ""));
    if (isDiff && changed) { node.tabIndex = -1; changes.push(node); }
    if (!rowGroup || rowGroup.childElementCount === 64) {
      rowGroup = document.createElement("div");
      rowGroup.className = "row-group";
      fragment.append(rowGroup);
    }
    rowGroup.append(node);
  }
  const visibleRows = fragment.childElementCount;
  $("rows").replaceChildren(fragment);
  $("change-badge").hidden = !session.candidate_path;
  $("change-badge").textContent = String(comparison.rows.filter((r) => r.kind !== "equal").length);
  $("review-summary").replaceChildren();
  if (isDiff) {
    if (comparison.source_equal) $("review-summary").textContent = "Sin cambios · los archivos son idénticos";
    else {
      const removed = document.createElement("span"); removed.className = "stat-removed"; removed.textContent = `−${comparison.removed}`;
      const added = document.createElement("span"); added.className = "stat-added"; added.textContent = `+${comparison.added}`;
      $("review-summary").append(removed, added, document.createTextNode("  bloques · original → propuesta"));
    }
  } else $("review-summary").textContent = "Modo lectura · documento original";
  $("change-navigation").hidden = !isDiff || changes.length === 0;
  updatePosition();
  $("document-status").textContent = isDiff ? "Dos versiones · desplazamiento alineado · J / K para navegar" : "Solo lectura · los archivos no se modifican";
  $("timing").textContent = `${Number(session.processing_ms || 0).toFixed(1)} ms en Rust`;
  if (visibleRows === 0) {
    const empty = document.createElement("p"); empty.className = "cell source-note"; empty.textContent = "Documento vacío."; $("rows").append(empty);
  }
}

function updatePosition() { $("change-position").textContent = `${changeIndex + 1} / ${changes.length}`; }
function navigateChange(delta) {
  if (!changes.length) return;
  if (changeIndex >= 0) changes[changeIndex].classList.remove("current");
  changeIndex = changeIndex < 0 ? (delta > 0 ? 0 : changes.length - 1) : (changeIndex + delta + changes.length) % changes.length;
  const target = changes[changeIndex];
  target.classList.add("current");
  target.scrollIntoView({ block: "center" });
  target.focus({ preventScroll: true });
  updatePosition();
}

for (const id of ["base-path", "candidate-path"]) {
  $(id).addEventListener("input", () => {
    pathsDirty = true;
    inputVersion++;
    clearTimeout(openTimer);
    openTimer = setTimeout(() => openFiles(), 350);
    setBusy(busy);
  });
  $(id).addEventListener("keydown", (event) => {
    if (event.key === "Enter") { event.preventDefault(); openFiles(); }
  });
}
$("reload-button").addEventListener("click", () => openFiles());
$("example-button").addEventListener("click", () => openFiles(true));
$("reading-tab").addEventListener("click", () => { if (view === "reading") return; view = "reading"; render(); });
$("diff-tab").addEventListener("click", () => { if (view === "diff") return; view = "diff"; render(); });
$("rendered-tab").addEventListener("click", () => { if (format === "rendered") return; format = "rendered"; render(); });
$("source-tab").addEventListener("click", () => { if (format === "source") return; format = "source"; render(); });
$("previous-change").addEventListener("click", () => navigateChange(-1));
$("next-change").addEventListener("click", () => navigateChange(1));
document.addEventListener("keydown", (event) => {
  if (event.ctrlKey || event.metaKey || event.altKey || event.target.closest("input,textarea,[contenteditable=true]")) return;
  if (event.key === "j" || event.key === "k") { event.preventDefault(); navigateChange(event.key === "j" ? 1 : -1); }
});

function hostedView(title, description) {
  $("pane-headers").hidden = true;
  $("reload-button").hidden = true;
  $("example-button").hidden = true;
  $("empty").querySelector("ol").hidden = true;
  $("empty").querySelector("h1").textContent = title;
  $("empty").querySelector("p").textContent = description;
  $("empty").querySelector(".small-label").textContent = "MDDIFF";
  $("timing").textContent = "Espacio privado";
}

function endSession() {
  authentication = null;
  events?.close();
  clearTimeout(openTimer);
  session = null;
  acceptedRevision = -1;
  changes = [];
  $("rows").replaceChildren();
  $("base-path").value = $("candidate-path").value = "";
  $("auth-user").textContent = "";
  $("auth-user").hidden = $("logout-button").hidden = true;
  $("login-button").hidden = false;
  $("connection").textContent = "Sin sesión";
  $("connection").classList.remove("connected");
  $("document-status").textContent = "Inicia sesión para continuar.";
  $("error").hidden = true;
  render();
  hostedView("Sesión finalizada", "Vuelve a iniciar sesión para acceder a tus documentos.");
}

function connectEvents() {
  events = new EventSource("/api/events");
  events.onopen = () => { acceptedRevision = -1; $("connection").textContent = "Conectado"; $("connection").classList.add("connected"); };
  events.addEventListener("revision", () => refresh());
  events.onerror = async () => {
    $("connection").textContent = "Reconectando…";
    $("connection").classList.remove("connected");
    if (hosted) {
      try {
        const response = await fetch("/auth/session");
        if (!response.ok) endSession();
      } catch { endSession(); }
    }
  };
}

$("login-button").addEventListener("click", () => { window.location.assign("/auth/login"); });
$("logout-button").addEventListener("click", async () => {
  $("logout-button").disabled = true;
  try {
    await request("/auth/logout", {});
    endSession();
  } catch (error) { showError(error); }
  finally { $("logout-button").disabled = false; }
});
window.addEventListener("pageshow", (event) => { if (event.persisted) window.location.reload(); });

async function start() {
  try {
    const response = await fetch("/auth/session");
    if (response.status === 404) { connectEvents(); return; }
    hosted = true;
    if (!response.ok) { endSession(); return; }
    authentication = await response.json();
    $("auth-user").textContent = authentication.user_label;
    $("auth-user").hidden = $("logout-button").hidden = false;
    hostedView("Tu espacio privado", "No hay documentos abiertos en esta cuenta.");
    $("document-status").textContent = "Solo tú puedes acceder a este espacio.";
    connectEvents();
  } catch (error) { showError(error); }
}
start();
