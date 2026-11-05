// The desktop app's window: the accounts this computer syncs, how each is
// going, and adding one. The app itself does the syncing, so this page
// only shows what it says and passes on what the person asks for.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

const STATES = {
  starting: ["Starting", "waiting"],
  synced: ["Up to date", "ok"],
  offline: ["Can't reach the server", "warning"],
  signed_out: ["Signed out", "bad"],
  problem: ["Problem", "bad"],
};

let accounts = [];
let byCode = false;
let busy = false;
// What was drawn last, so the same accounts aren't drawn again.
let drawn = "";
// Accounts whose Recent changes are expanded, which stay expanded when the
// window redraws.
const expanded = new Set();

function element(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === "class") node.className = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value);
  }
  for (const child of children.flat()) {
    if (child == null || child === false) continue;
    node.append(typeof child === "string" ? document.createTextNode(child) : child);
  }
  return node;
}

function errorText(error) {
  return typeof error === "string" ? error : "Something went wrong. Try again.";
}

function card(view) {
  const [label, tone] = STATES[view.status.state] ?? STATES.starting;
  const status = view.status;
  const who = `${view.display_name} on ${view.server.replace(/^https?:\/\//, "")}`;
  const away = status.via === "tunnel" ? " Reaching it away from home." : "";
  return element(
    "article",
    { class: "card", "aria-labelledby": `name-${view.id}` },
    element(
      "div",
      { class: "card-head" },
      element("h2", { id: `name-${view.id}` }, view.name),
      element("span", { class: `state ${tone}` }, label),
    ),
    element("p", { class: "who" }, who + away),
    status.message && element("p", { class: tone === "bad" ? "error" : "hint" }, status.message),
    status.away && element("p", { class: "hint" }, status.away),
    status.paused_deletes > 0 &&
      element(
        "div",
        { class: "notice", role: "alert" },
        element(
          "p",
          {},
          `You deleted ${status.paused_deletes} items here, so Accordion is waiting before it ` +
            "deletes them in Accordion too. Nothing in Accordion was deleted.",
        ),
        element(
          "div",
          { class: "buttons" },
          element(
            "button",
            { type: "button", "data-key": `${view.id}:allow`, onclick: () => invoke("allow_deletes", { id: view.id }) },
            "Move them to Accordion's trash",
          ),
          element(
            "button",
            {
              type: "button",
              class: "secondary",
              "data-key": `${view.id}:bring-back`,
              onclick: () => invoke("bring_back", { id: view.id }),
            },
            "Bring them back here",
          ),
        ),
      ),
    status.notices.length > 0 &&
      element(
        "div",
        { class: "notice" },
        element("ul", {}, status.notices.map((notice) => element("li", {}, notice))),
        element(
          "button",
          {
            type: "button",
            class: "link",
            "data-key": `${view.id}:dismiss`,
            onclick: () => invoke("dismiss_notices", { id: view.id }),
          },
          "Dismiss",
        ),
      ),
    element(
      "div",
      { class: "buttons" },
      element(
        "button",
        { type: "button", "data-key": `${view.id}:open`, onclick: () => invoke("open_folder", { id: view.id }) },
        "Open in File Explorer",
      ),
      element(
        "button",
        { type: "button", class: "secondary", "data-key": `${view.id}:sync`, onclick: () => invoke("sync_now", { id: view.id }) },
        "Sync now",
      ),
      element("button", { type: "button", class: "quiet", "data-key": `${view.id}:remove`, onclick: () => confirmRemove(view) }, "Remove"),
    ),
    status.recent.length > 0 &&
      element(
        "details",
        {
          ...(expanded.has(view.id) ? { open: "" } : {}),
          ontoggle: (event) => {
            if (event.target.open) expanded.add(view.id);
            else expanded.delete(view.id);
          },
        },
        element("summary", { "data-key": `${view.id}:recent` }, "Recent changes"),
        element("ul", { class: "recent" }, status.recent.map((line) => element("li", {}, line))),
      ),
  );
}

function render() {
  $("loading").hidden = true;
  const now = JSON.stringify(accounts);
  if (now !== drawn) {
    drawn = now;
    // Keyboard focus stays on the same button, such as Sync now.
    const focused = document.activeElement?.closest("#accounts [data-key]")?.dataset.key;
    $("accounts").replaceChildren(...accounts.map(card));
    if (focused) $("accounts").querySelector(`[data-key="${CSS.escape(focused)}"]`)?.focus();
  }
  const adding = !$("add").hidden || !$("waiting").hidden;
  $("add-another").hidden = accounts.length === 0 || adding;
  if (accounts.length === 0 && $("waiting").hidden) showAdd();
}

async function refresh() {
  accounts = await invoke("list");
  render();
}

async function showAdd() {
  const form = $("add");
  if (!form.hidden) return;
  form.hidden = false;
  $("add-another").hidden = true;
  $("add-cancel").hidden = accounts.length === 0;
  $("add-title").textContent =
    accounts.length === 0 ? "Add your Accordion server" : "Add another account";
  $("name").value = await invoke("suggested_name");
  updateNameHint();
  $("server").focus();
}

function hideAdd() {
  $("add").hidden = true;
  $("add").reset();
  $("add-error").textContent = "";
  setByCode(false);
  render();
}

function setByCode(on) {
  byCode = on;
  $("by-code").hidden = !on;
  $("add-submit").textContent = on ? "Add account" : "Sign in with your browser";
  $("toggle-code").textContent = on ? "Sign in with your browser instead" : "Use a code instead";
}

function updateNameHint() {
  const name = $("name").value.trim() || "Home";
  $("name-hint").textContent = `File Explorer shows it as Accordion (${name}).`;
}

async function add(event) {
  event.preventDefault();
  if (busy) return;
  const server = $("server").value.trim();
  const name = $("name").value.trim();
  const code = $("code").value.trim();
  if (!byCode && !server) {
    $("add-error").textContent = "Enter your server's address, such as ourhome.dedyn.io.";
    $("server").focus();
    return;
  }
  if (byCode && !code) {
    $("add-error").textContent = "Enter the code from Accordion, or paste its whole setup.";
    $("code").focus();
    return;
  }
  busy = true;
  $("add-submit").disabled = true;
  $("add-error").textContent = "";
  try {
    if (byCode) {
      await invoke("use_code", { server, code, name });
    } else {
      $("add").hidden = true;
      $("waiting").hidden = false;
      await invoke("sign_in", { server, name });
    }
    $("waiting").hidden = true;
    accounts = await invoke("list");
    hideAdd();
  } catch (error) {
    $("waiting").hidden = true;
    $("add").hidden = false;
    $("add-error").textContent = errorText(error);
  } finally {
    busy = false;
    $("add-submit").disabled = false;
    await refresh();
  }
}

function confirmRemove(view) {
  const dialog = $("remove-dialog");
  $("remove-title").textContent = `Remove ${view.name} from this computer?`;
  $("remove-body").textContent =
    `This computer stops syncing ${view.name} and signs out of it. Files you kept on this ` +
    `computer stay in ${view.folder}. Files that are only in Accordion go from this computer, ` +
    "and stay in Accordion.";
  dialog.returnValue = "";
  dialog.onclose = async () => {
    if (dialog.returnValue !== "remove") return;
    try {
      await invoke("remove", { id: view.id });
    } catch (error) {
      window.alert(errorText(error));
    }
    await refresh();
  };
  dialog.showModal();
}

$("add").addEventListener("submit", add);
$("add-cancel").addEventListener("click", hideAdd);
$("add-another").addEventListener("click", showAdd);
$("toggle-code").addEventListener("click", () => setByCode(!byCode));
$("name").addEventListener("input", updateNameHint);
$("stop-waiting").addEventListener("click", () => invoke("cancel_sign_in"));

listen("changed", refresh);
refresh();
