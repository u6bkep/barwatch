// The public join page: /app/join/<session id>.

const id = location.pathname.replace(/\/+$/, "").split("/").pop();
const serverBase = new URL("../../", location.href).href;
const api = new URL(`app/v1/sessions/${id}`, serverBase).href;

function render(s) {
  const waiting = s.phase === "idle" || s.phase === "failed";
  $("party").replaceChildren(
    el("div", { class: "row top" },
      mapImage(s.replay, "mq", "mapimg"),
      el("div", { class: "grow" },
        el("div", { class: "row" }, el("h3", {}, s.replay.map), phaseBadge(s.phase)),
        teamsBlock(s.replay),
        replayFacts(s.replay),
        s.note && el("div", { class: `note ${s.phase === "failed" ? "bad" : ""}` }, s.note),
        el("div", { class: "row", style: "margin-top:14px" },
          el("a", { class: "button primary", href: joinLink(serverBase, s.id) }, "Join in barwatch"),
          waiting && el("button", { onclick: async () => {
            try { await postJson(`${api}/start`); } catch (e) { toast(e.message, true); }
          } }, s.run ? "Start again" : "Start for everyone")),
        el("p", { class: "muted" }, s.phase === "running"
          ? "The replay is playing. Joining now drops you in and catches you up."
          : "Join first; the replay starts for everyone when someone presses Start."))),
    el("h2", { style: "margin-top:18px" }, "Viewers"),
    viewersBlock(s));
}

follow(api, () => true, render, () => {
  $("party").replaceChildren(el("p", {}, "This watch party no longer exists."));
});
