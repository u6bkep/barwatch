// The public join page: /app/join/<room id>.

const id = location.pathname.replace(/\/+$/, "").split("/").pop();
const serverBase = new URL("../../", location.href).href;
const api = new URL(`app/v1/rooms/${id}`, serverBase).href;

function render(r) {
  const waiting = r.phase === "idle" || r.phase === "failed";
  const info = r.replay?.info;
  $("room").replaceChildren(
    el("div", { class: "row top" },
      info && mapImage(info, "mq", "mapimg"),
      el("div", { class: "grow" },
        el("div", { class: "row" }, el("h3", {}, r.name), phaseBadge(r.phase)),
        info
          ? el("div", {}, el("p", {}, el("b", {}, info.map)), teamsBlock(info), replayFacts(info))
          : el("p", { class: "muted" }, "No replay picked yet. Join anyway: you will be ready when one is."),
        r.note && el("div", { class: `note ${r.phase === "failed" ? "bad" : ""}` }, r.note),
        el("div", { class: "row", style: "margin-top:14px" },
          el("a", { class: "button primary", href: joinLink(serverBase, r.id) }, "Join in barwatch"),
          waiting && el("button", { onclick: async () => {
            try { await postJson(`${api}/start`); } catch (e) { toast(e.message, true); }
          } }, r.run ? "Start again" : "Start for everyone")),
        el("p", { class: "muted" }, r.phase === "running"
          ? "The replay is playing. Joining now drops you in and catches you up."
          : "Join first; the replay starts for everyone when someone presses Start."))),
    el("h2", { style: "margin-top:18px" }, "Here now"),
    membersBlock(r));
}

follow(api, () => true, render, () => {
  $("room").replaceChildren(el("p", {}, "This room was deleted, or it has a new link. Ask for the new one."));
});
