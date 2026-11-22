"use strict";
// Shared by the main page and the public join page.

const $ = (id) => document.getElementById(id);

/** Builds an element. Text always goes in as text: replay data (player names, file
 *  names) comes from files strangers made. */
function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === "class") node.className = v;
    else if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else if (v !== undefined && v !== null && v !== false) node.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    node.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return node;
}

async function request(url, options = {}) {
  const resp = await fetch(url, options);
  if (!resp.ok) {
    let message = `${resp.status} ${resp.statusText}`;
    try { message = (await resp.json()).error || message; } catch { /* not JSON */ }
    throw new Error(message);
  }
  return resp.status === 204 ? null : resp.json();
}

const postJson = (url, body) =>
  request(url, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body ?? {}) });

let toastTimer;
function toast(message, bad = false) {
  const t = $("toast");
  t.textContent = message;
  t.className = bad ? "bad" : "";
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.add("hidden"), bad ? 8000 : 3000);
}

function clock(seconds) {
  const s = Math.max(0, Math.round(seconds));
  const pad = (n) => String(n).padStart(2, "0");
  return s >= 3600 ? `${Math.floor(s / 3600)}:${pad(Math.floor(s / 60) % 60)}:${pad(s % 60)}` : `${Math.floor(s / 60)}:${pad(s % 60)}`;
}

/** "14:00", "1:02:03", "90", "5m" → seconds, or null. */
function parseClock(text) {
  const t = text.trim();
  if (/^\d+(:\d{1,2}){1,2}$/.test(t)) return t.split(":").reduce((acc, p) => acc * 60 + Number(p), 0);
  if (/^\d+m$/.test(t)) return Number(t.slice(0, -1)) * 60;
  if (/^\d+s?$/.test(t)) return Number(t.replace("s", ""));
  return null;
}

const when = (unix) => (unix ? new Date(unix * 1000).toLocaleString([], { dateStyle: "medium", timeStyle: "short" }) : "");

function mapImage(replay, size, cls) {
  const img = el("img", { class: cls, alt: "", loading: "lazy" });
  if (replay.map_file) {
    img.src = `https://api.bar-rts.com/maps/${encodeURIComponent(replay.map_file)}/texture-${size}.jpg`;
    img.addEventListener("error", () => img.removeAttribute("src"));
  }
  return img;
}

const PHASE_TEXT = { empty: "no replay", preparing: "fetching engine", idle: "ready", starting: "starting", running: "running", failed: "failed" };
const phaseBadge = (phase) => el("span", { class: `badge ${phase}` }, PHASE_TEXT[phase] || phase);

function teamsBlock(replay) {
  const teams = replay.teams.map((members, i) => el("div", { class: "team" }, el("b", {}, `Team ${i + 1}`), members.join(", ")));
  if (replay.spectators.length) teams.push(el("div", { class: "team" }, el("b", {}, "Spectators"), replay.spectators.join(", ")));
  return el("div", { class: "teams" }, teams);
}

function replayFacts(replay) {
  return el("dl", { class: "kv" },
    el("dt", {}, "Played"), el("dd", {}, when(replay.start_unix)),
    el("dt", {}, "Length"), el("dd", {}, clock(replay.duration_s)),
    el("dt", {}, "Game"), el("dd", {}, replay.game),
    el("dt", {}, "Engine"), el("dd", {}, replay.engine),
    el("dt", {}, "File"), el("dd", {}, replay.file_name));
}

/** What a room plays, in one line. */
function playsLine(room) {
  const r = room.replay?.info;
  if (!r) return "No replay yet";
  return `${r.map} · ${r.teams.map((t) => t.join(", ")).join("  vs  ")}`;
}

/** Who is in a room, in one line. */
const whoLine = (room) => (room.members.length ? room.members.map((m) => m.name).join(", ") : "Nobody is here");

function membersBlock(room) {
  if (!room.members.length) return el("p", { class: "muted" }, "Nobody is here yet.");
  return el("div", { class: "viewers" }, room.members.map((v) => {
    const label = v.in_game ? "in game" : v.state;
    const cls = v.in_game ? "playing" : v.state;
    return el("div", { class: "viewer" }, el("span", { class: `badge ${cls}` }, label), el("b", {}, v.name),
      v.engine_name !== v.name && el("span", { class: "muted" }, `as ${v.engine_name}`), el("span", { class: "muted" }, v.detail));
  }));
}

/** The link that opens the desktop app in this room. */
const joinLink = (serverBase, id) =>
  `barwatch://join?server=${encodeURIComponent(serverBase)}&room=${encodeURIComponent(id)}`;

/** Follows a room: calls `onChange(room)` now and after every change, until
 *  `alive()` turns false. `url` is the room's API address. */
async function follow(url, alive, onChange, onGone) {
  let rev = null;
  while (alive()) {
    try {
      const s = await request(rev === null ? url : `${url}?rev=${rev}&wait=25`);
      if (!alive()) return;
      rev = s.rev;
      onChange(s);
    } catch (e) {
      if (!alive()) return;
      if (/no such room/.test(e.message)) return onGone();
      await new Promise((r) => setTimeout(r, 3000));
    }
  }
}

async function copyText(text) {
  try { await navigator.clipboard.writeText(text); toast("Copied"); }
  catch { toast("Could not copy; select the text instead", true); }
}
