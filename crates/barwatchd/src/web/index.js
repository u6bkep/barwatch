// The main page: create, list, run and control watch parties.

const serverBase = new URL(".", location.href).href;
let current = null;   // id of the session shown in detail
let epoch = 0;        // bumped to retire a detail view's long-poll loop

async function refreshList() {
  try {
    const sessions = await request("api/sessions");
    const box = $("sessions");
    box.replaceChildren(...sessions.map((s) =>
      el("button", { class: "session-item", onclick: () => open(s.id) },
        mapImage(s.replay, "thumb", "thumb"),
        el("div", { class: "grow" },
          el("div", { class: "row" }, el("b", {}, s.replay.map), phaseBadge(s.phase)),
          el("div", { class: "muted" }, s.replay.teams.map((t) => t.join(", ")).join("  vs  ")),
          el("div", { class: "muted" }, `${when(s.replay.start_unix)} · ${clock(s.replay.duration_s)} · ${s.viewers.length} joined`)))));
    if (!sessions.length) box.append(el("p", { class: "muted" }, "None yet. Add a replay above."));
  } catch (e) { toast(e.message, true); }
}

function open(id) {
  current = id;
  history.replaceState(null, "", id ? `#${id}` : location.pathname);
  const mine = ++epoch;
  const box = $("detail");
  if (!id) { box.classList.add("hidden"); return; }
  box.classList.remove("hidden");
  box.replaceChildren(el("p", { class: "muted" }, "Loading…"));
  box.scrollIntoView({ behavior: "smooth", block: "start" });
  follow(`api/sessions/${id}`, () => mine === epoch, renderDetail, () => { toast("That watch party no longer exists", true); open(null); refreshList(); });
}

async function act(promise, done) {
  try { await promise; if (done) toast(done); } catch (e) { toast(e.message, true); }
}

function renderDetail(s) {
  const base = `api/sessions/${s.id}`;
  const running = s.phase === "running";
  const invite = new URL(`app/join/${s.id}`, serverBase).href;
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
    [0.5, 1, 1.5, 2, 3, 5, 10].map((v) => el("option", { value: v, selected: Math.abs(v - s.speed) < 0.01 }, `${v}×`)));

  $("detail").replaceChildren(
    el("div", { class: "row top" },
      mapImage(s.replay, "mq", "mapimg"),
      el("div", { class: "grow" },
        el("div", { class: "row" }, el("h3", {}, s.replay.map), phaseBadge(s.phase), s.ended && el("span", { class: "badge" }, "replay finished"),
          el("span", { class: "grow" }), el("button", { onclick: () => open(null) }, "Close")),
        teamsBlock(s.replay),
        replayFacts(s.replay),
        s.note && el("div", { class: `note ${s.phase === "failed" ? "bad" : ""}` }, s.note))),

    el("h2", { style: "margin-top:18px" }, "Invite"),
    el("p", { class: "muted" }, "Send this link. It opens a page with a join button and needs no login."),
    el("div", { class: "linkbox" },
      el("input", { type: "text", readonly: true, value: invite, onfocus: (e) => e.target.select() }),
      el("button", { onclick: () => copyText(invite) }, "Copy"),
      el("a", { class: "button primary", href: joinLink(serverBase, s.id) }, "Join in barwatch")),

    el("h2", { style: "margin-top:18px" }, "Playback"),
    el("div", { class: "row" },
      running || s.phase === "starting"
        ? el("button", { class: "danger", onclick: () => act(postJson(`${base}/stop`)) }, "Stop")
        : el("button", { class: "primary", disabled: s.phase === "preparing", onclick: () => act(postJson(`${base}/start`)) },
            s.run ? "Start again" : "Start"),
      el("button", { disabled: !running, onclick: () => control({ action: "pause", paused: !s.paused }) }, s.paused ? "Resume" : "Pause"),
      speed,
      el("span", { class: "muted" }, "Jump ahead"),
      [[60, "+1 min"], [300, "+5 min"], [600, "+10 min"]].map(([seconds, label]) =>
        el("button", { disabled: !running, onclick: () => control({ action: "skip", seconds, relative: true }, `Jumping ahead ${clock(seconds)}`) }, label)),
      el("span", { class: "muted" }, "or to"), jumpTo, el("button", { disabled: !running, onclick: jump }, "Go")),
    el("p", { class: "muted" }, "In game chat: ", el("code", {}, "!skip 14:00"), " ", el("code", {}, "!skip +2m"), " ",
      el("code", {}, "!pause"), " ", el("code", {}, "!resume"), " ", el("code", {}, "!speed 2"),
      ". Jumps only go forward; to go back, stop and start again."),

    el("h2", { style: "margin-top:18px" }, "Viewers"),
    viewersBlock(s),

    el("div", { class: "row", style: "margin-top:18px" }, el("span", { class: "grow" }),
      el("button", { class: "danger", onclick: async () => {
        if (!confirm("Delete this watch party and its replay from the server?")) return;
        await act(request(base, { method: "DELETE" }), "Deleted");
        open(null); setTimeout(refreshList, 600);
      } }, "Delete")));
  if (focused === "jump-to") $("jump-to").focus();
}

async function upload(file) {
  if (!file) return;
  toast(`Uploading ${file.name}…`);
  try {
    const made = await request("api/sessions", {
      method: "POST",
      headers: { "content-type": "application/octet-stream", "x-barwatch-filename": encodeURIComponent(file.name) },
      body: file,
    });
    await refreshList();
    open(made.id);
  } catch (e) { toast(e.message, true); }
}

const drop = $("drop");
drop.addEventListener("click", () => $("file").click());
drop.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") $("file").click(); });
$("file").addEventListener("change", (e) => { upload(e.target.files[0]); e.target.value = ""; });
for (const type of ["dragenter", "dragover"]) drop.addEventListener(type, (e) => { e.preventDefault(); drop.classList.add("over"); });
for (const type of ["dragleave", "drop"]) drop.addEventListener(type, (e) => { e.preventDefault(); drop.classList.remove("over"); });
drop.addEventListener("drop", (e) => upload(e.dataTransfer.files[0]));

$("bar-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const link = $("bar-link").value.trim();
  if (!link) return;
  toast("Fetching the replay from BAR…");
  try {
    const made = await postJson("api/sessions", { bar_replay: link });
    $("bar-link").value = "";
    await refreshList();
    open(made.id);
  } catch (e) { toast(e.message, true); }
});

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
