// The main page: make, list, run and control rooms, and pick what they play.

const serverBase = new URL(".", location.href).href;
let current = null;   // id of the room shown in detail
let epoch = 0;        // bumped to retire a detail view's long-poll loop
let rooms = [];       // the last list

const inviteUrl = (id) => new URL(`app/join/${id}`, serverBase).href;

async function act(promise, done) {
  try { const r = await promise; if (done) toast(done); return r; } catch (e) { toast(e.message, true); return null; }
}

async function deleteRoom(id) {
  if (!confirm("Delete this room? Its replays stay in the archive.")) return;
  await act(request(`api/rooms/${id}`, { method: "DELETE" }), "Deleted");
  if (current === id) open(null);
  refreshList();
}

async function relink(id) {
  const room = await act(postJson(`api/rooms/${id}/relink`), "The room has a new link; the old one no longer works");
  if (!room) return;
  if (current === id) open(room.id);
  refreshList();
}

function roomActions(room) {
  return el("div", { class: "row" },
    el("button", { onclick: () => copyText(inviteUrl(room.id)) }, "Copy link"),
    el("button", { onclick: () => relink(room.id) }, "New link"),
    el("button", { class: "danger", onclick: () => deleteRoom(room.id) }, "Delete"));
}

async function refreshList() {
  try {
    rooms = await request("api/rooms");
    const box = $("rooms");
    box.replaceChildren(...rooms.map((r) =>
      el("div", { class: "session-item" },
        el("button", { class: "session-open", onclick: () => open(r.id) },
          r.replay ? mapImage(r.replay.info, "thumb", "thumb") : el("div", { class: "thumb" }),
          el("div", { class: "grow" },
            el("div", { class: "row" }, el("b", {}, r.name), phaseBadge(r.phase)),
            el("div", { class: "muted" }, playsLine(r)),
            el("div", { class: "muted" }, whoLine(r)))),
        roomActions(r))));
    if (!rooms.length) box.append(el("p", { class: "muted" }, "None yet. Make one below."));
    renderChooser();
  } catch (e) { toast(e.message, true); }
}

function open(id) {
  current = id;
  history.replaceState(null, "", id ? `#${id}` : location.pathname + location.search);
  const mine = ++epoch;
  const box = $("detail");
  $("pick").classList.toggle("hidden", !id);
  if (!id) { box.classList.add("hidden"); return; }
  box.classList.remove("hidden");
  box.replaceChildren(el("p", { class: "muted" }, "Loading…"));
  box.scrollIntoView({ behavior: "smooth", block: "start" });
  follow(`api/rooms/${id}`, () => mine === epoch, renderDetail, () => {
    toast("That room was deleted or has a new link", true); open(null); refreshList();
  });
}

/** Has room `id` play a replay: `body` is a file, or JSON for `PUT …/replay`. */
async function pick(id, body, what) {
  toast(what);
  const options = body instanceof File
    ? { method: "PUT", headers: { "content-type": "application/octet-stream", "x-barwatch-filename": encodeURIComponent(body.name) }, body }
    : { method: "PUT", headers: { "content-type": "application/json" }, body: JSON.stringify(body) };
  const room = await act(request(`api/rooms/${id}/replay`, options), "Picked. Press Start when everyone is ready.");
  if (room) refreshList();
  return room;
}

function replayBlock(replay) {
  const info = replay.info;
  return el("div", { class: "row top" },
    mapImage(info, "mq", "mapimg"),
    el("div", { class: "grow" },
      el("div", { class: "row" }, el("h3", {}, info.map),
        replay.match_url && el("a", { class: "button", href: replay.match_url, target: "_blank", rel: "noopener" }, "View in Gex")),
      teamsBlock(info),
      replayFacts(info)));
}

function historyBlock(r) {
  if (!r.history.length) return el("p", { class: "muted" }, "Nothing played here before.");
  return el("div", { class: "sessions" }, r.history.map((h) =>
    el("div", { class: "session-item" },
      mapImage(h.info, "thumb", "thumb"),
      el("div", { class: "grow" },
        el("div", { class: "row" }, el("b", {}, h.info.map)),
        el("div", { class: "muted" }, h.info.teams.map((t) => t.join(", ")).join("  vs  ")),
        el("div", { class: "muted" }, `picked ${when(h.picked_unix)} · ${clock(h.info.duration_s)}`,
          !h.available && " · the server no longer keeps this file")),
      el("div", { class: "row" },
        el("button", { disabled: !h.available, onclick: () => pick(r.id, { key: h.key }, "Picking it again…") }, "Watch"),
        h.match_url && el("a", { class: "button", href: h.match_url, target: "_blank", rel: "noopener" }, "View in Gex")))));
}

function renderDetail(r) {
  const base = `api/rooms/${r.id}`;
  const running = r.phase === "running";
  const invite = inviteUrl(r.id);
  const control = (body, done) => act(postJson(`${base}/control`, body), done);

  // Keep what is being typed across re-renders.
  const typed = $("jump-to")?.value ?? "";
  const focused = document.activeElement?.id;

  const jumpTo = el("input", { type: "text", id: "jump-to", placeholder: "14:00", size: "7", value: typed, disabled: !running });
  const jump = () => {
    const seconds = parseClock(jumpTo.value);
    if (seconds === null) return toast("Give a game time like 14:00", true);
    control({ action: "skip", seconds, relative: false }, `Jumping to ${clock(seconds)}`);
  };
  jumpTo.addEventListener("keydown", (e) => { if (e.key === "Enter") jump(); });

  const speed = el("select", { disabled: !running, onchange: (e) => control({ action: "speed", speed: Number(e.target.value) }) },
    [0.5, 1, 1.5, 2, 3, 5, 10].map((v) => el("option", { value: v, selected: Math.abs(v - r.speed) < 0.01 }, `${v}×`)));

  $("pick").querySelector("h2").textContent = `Pick a replay for ${r.name}`;
  $("detail").replaceChildren(
    el("div", { class: "row" }, el("h3", {}, r.name), phaseBadge(r.phase), r.ended && el("span", { class: "badge" }, "replay finished"),
      el("span", { class: "grow" }), el("button", { onclick: () => open(null) }, "Close")),
    r.note ? el("div", { class: `note ${r.phase === "failed" ? "bad" : ""}` }, r.note) : "",

    el("h2", { style: "margin-top:18px" }, "Playing"),
    r.replay ? replayBlock(r.replay) : el("p", { class: "muted" }, "No replay yet. Pick one below."),

    el("h2", { style: "margin-top:18px" }, "Invite"),
    el("p", { class: "muted" }, "Send this link. It opens a page with a join button and needs no login. Anyone with it can pick, start and control what this room plays."),
    el("div", { class: "linkbox" },
      el("input", { type: "text", readonly: true, value: invite, onfocus: (e) => e.target.select() }),
      el("button", { onclick: () => copyText(invite) }, "Copy"),
      el("button", { onclick: () => relink(r.id) }, "New link"),
      el("a", { class: "button primary", href: joinLink(serverBase, r.id) }, "Join in barwatch")),

    el("h2", { style: "margin-top:18px" }, "Playback"),
    el("div", { class: "row" },
      running || r.phase === "starting"
        ? el("button", { class: "danger", onclick: () => act(postJson(`${base}/stop`)) }, "Stop")
        : el("button", { class: "primary", disabled: r.phase === "preparing" || r.phase === "empty", onclick: () => act(postJson(`${base}/start`)) },
            r.run ? "Start again" : "Start"),
      el("button", { disabled: !running, onclick: () => control({ action: "pause", paused: !r.paused }) }, r.paused ? "Resume" : "Pause"),
      speed,
      el("span", { class: "muted" }, "Jump ahead"),
      [[60, "+1 min"], [300, "+5 min"], [600, "+10 min"]].map(([seconds, label]) =>
        el("button", { disabled: !running, onclick: () => control({ action: "skip", seconds, relative: true }, `Jumping ahead ${clock(seconds)}`) }, label)),
      el("span", { class: "muted" }, "or to"), jumpTo, el("button", { disabled: !running, onclick: jump }, "Go")),
    el("p", { class: "muted" }, "In game chat: ", el("code", {}, "!skip 14:00"), " ", el("code", {}, "!skip +2m"), " ",
      el("code", {}, "!pause"), " ", el("code", {}, "!resume"), " ", el("code", {}, "!speed 2"),
      ". Jumps only go forward; to go back, stop and start again."),

    el("h2", { style: "margin-top:18px" }, "Here now"),
    membersBlock(r),

    el("h2", { style: "margin-top:18px" }, "Played before"),
    historyBlock(r),

    el("div", { class: "row", style: "margin-top:18px" }, el("span", { class: "grow" }),
      el("button", { class: "danger", onclick: () => deleteRoom(r.id) }, "Delete")));
  if (focused === "jump-to") $("jump-to").focus();
}

// Picking for the open room: a dropped file or a BAR link.
const drop = $("drop");
const pickFile = (file) => { if (file && current) pick(current, file, `Uploading ${file.name}…`); };
drop.addEventListener("click", () => $("file").click());
drop.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") $("file").click(); });
$("file").addEventListener("change", (e) => { pickFile(e.target.files[0]); e.target.value = ""; });
for (const type of ["dragenter", "dragover"]) drop.addEventListener(type, (e) => { e.preventDefault(); drop.classList.add("over"); });
for (const type of ["dragleave", "drop"]) drop.addEventListener(type, (e) => { e.preventDefault(); drop.classList.remove("over"); });
drop.addEventListener("drop", (e) => pickFile(e.dataTransfer.files[0]));

$("bar-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const link = $("bar-link").value.trim();
  if (!link || !current) return;
  if (await pick(current, { bar_replay: link }, "Fetching the replay…")) $("bar-link").value = "";
});

async function newRoom(name) {
  const room = await act(postJson("api/rooms", { name }), "Room made");
  if (room) await refreshList();
  return room;
}

$("new-room").addEventListener("submit", async (e) => {
  e.preventDefault();
  const room = await newRoom($("room-name").value);
  if (room) { $("room-name").value = ""; open(room.id); }
});

// `/?replay=<id>` (the stats service's "Watch together" link): asks which room to
// play it in, or makes a new one. Anything but a plain replay id is ignored.
let wanted = null;
{
  const asked = new URLSearchParams(location.search).get("replay");
  if (asked && /^[0-9a-fA-F]{32}$/.test(asked)) wanted = asked.toLowerCase();
}

function renderChooser() {
  const box = $("choose");
  if (!wanted) { box.classList.add("hidden"); return; }
  const done = async (room) => {
    if (!room) return;
    const id = wanted;
    wanted = null;
    history.replaceState(null, "", location.pathname);
    renderChooser();
    open(room.id);
    await pick(room.id, { bar_replay: id }, "Fetching the replay…");
  };
  box.classList.remove("hidden");
  box.replaceChildren(
    el("h2", {}, "Watch together"),
    el("p", {}, "Which room should play replay ", el("code", {}, wanted), "?"),
    el("div", { class: "row" },
      rooms.map((r) => el("button", { onclick: () => done(r) }, r.name)),
      el("button", { class: "primary", onclick: async () => done(await newRoom("Room")) }, "A new room"),
      el("button", { onclick: () => { wanted = null; history.replaceState(null, "", location.pathname); renderChooser(); } }, "Never mind")));
}

async function refreshTokens() {
  try {
    const tokens = await request("api/tokens");
    $("tokens").replaceChildren(...tokens.map((t) =>
      el("div", { class: "viewer" }, el("b", {}, t.label), el("span", { class: "muted" }, `paired ${when(t.created_unix)}`),
        el("button", { class: "danger", onclick: async () => { await act(request(`api/tokens/${t.id}`, { method: "DELETE" }), "Unpaired"); refreshTokens(); } }, "Unpair"))));
  } catch (e) { toast(e.message, true); }
}

$("pair").addEventListener("click", async () => {
  try {
    const made = await postJson("api/pair", {});
    refreshTokens();
    // Hands the token to the desktop app registered for barwatch:// links.
    location.href = `barwatch://pair?server=${encodeURIComponent(serverBase)}&token=${encodeURIComponent(made.token)}`;
    toast("Confirm in the desktop app. Nothing happened? Install it first.");
  } catch (e) { toast(e.message, true); }
});

refreshList();
refreshTokens();
setInterval(refreshList, 10000);
if (location.hash.length > 1) open(location.hash.slice(1));
if (wanted) $("choose").scrollIntoView({ block: "start" });
