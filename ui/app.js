"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (sel) => document.querySelector(sel);

const ICONS = {
  homepod:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M6 6.5c0-1.7 2.7-3 6-3s6 1.3 6 3v10.3c0 2-2.7 3.7-6 3.7s-6-1.7-6-3.7V6.5Z"/><ellipse cx="12" cy="6.5" rx="6" ry="3"/><circle cx="12" cy="6.5" r="1.3" fill="currentColor" stroke="none"/></svg>',
  "homepod-mini":
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M4.5 11.5c0-2.4 3.4-4 7.5-4s7.5 1.6 7.5 4c0 4.5-3.3 8-7.5 8s-7.5-3.5-7.5-8Z"/><ellipse cx="12" cy="11" rx="7.5" ry="3.3"/><circle cx="12" cy="11" r="1.3" fill="currentColor" stroke="none"/></svg>',
  appletv:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><rect x="4" y="8" width="16" height="9" rx="2.5"/><path d="M7 20h10"/></svg>',
  mac:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><rect x="4.5" y="5" width="15" height="10.5" rx="1.5"/><path d="M2.5 18.5h19"/></svg>',
  speaker:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><rect x="6" y="3" width="12" height="18" rx="3"/><circle cx="12" cy="14.5" r="3"/><circle cx="12" cy="7.5" r="1" fill="currentColor" stroke="none"/></svg>',
};

const CHECK = '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="m3.5 8.5 3 3 6-7"/></svg>';
const VOL_LOW = '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M4 9.5h3.5L12 5.5v13l-4.5-4H4z"/></svg>';
const VOL_HIGH = '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M3 9.5h3.5L11 5.5v13l-4.5-4H3z"/><path d="M14.5 8.5a5 5 0 0 1 0 7M17.3 5.8a9 9 0 0 1 0 12.4" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/></svg>';

const KIND_ORDER = { homepod: 0, "homepod-mini": 1, speaker: 2, appletv: 3, mac: 4 };
const STATE_TEXT = { idle: "", connecting: "Connexion…", playing: "En lecture", error: "" };

let state = null;
const rows = new Map(); // id -> { el, slider, dragging }

function friendlyError(msg) {
  if (!msg) return "Erreur";
  if (/10060|timed out|did not properly respond|not properly respond/i.test(msg)) {
    $("#firewall").hidden = false;
    return "Pas de réponse : autorisez Ho My Pod dans le pare-feu (bouton ci-dessus).";
  }
  if (/10061|refused/i.test(msg)) return "Connexion refusée par l'enceinte.";
  if (/closed by device/i.test(msg)) return "L'enceinte a fermé la connexion (utilisée par un autre appareil ?).";
  return msg;
}

function setSliderFill(slider) {
  slider.style.setProperty("--p", `${slider.value}%`);
}

function createRow(dev) {
  const el = document.createElement("div");
  el.className = "device";
  el.innerHTML = `
    <div class="dev-main" role="button" tabindex="0">
      <div class="dev-icon"></div>
      <div class="dev-text">
        <div class="dev-name"></div>
        <div class="dev-sub"><span class="model"></span><span class="state"></span></div>
      </div>
      <div class="check">${CHECK}</div>
    </div>
    <div class="dev-volume"><div>${VOL_LOW}<input type="range" min="0" max="100" step="1" aria-label="Volume" />${VOL_HIGH}</div></div>`;

  const row = { el, slider: el.querySelector("input"), dragging: false, lastSent: 0, timer: null };

  const toggle = () => {
    const d = state.devices.find((x) => x.id === dev.id);
    if (!d) return;
    if (d.status === "playing" || d.status === "connecting") invoke("disconnect", { id: dev.id });
    else invoke("connect", { id: dev.id });
  };
  const main = el.querySelector(".dev-main");
  main.addEventListener("click", toggle);
  main.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      toggle();
    }
  });

  const send = () => {
    row.lastSent = Date.now();
    invoke("set_volume", { id: dev.id, volume: Number(row.slider.value) });
  };
  row.slider.addEventListener("pointerdown", () => (row.dragging = true));
  row.slider.addEventListener("pointerup", () => (row.dragging = false));
  row.slider.addEventListener("input", () => {
    setSliderFill(row.slider);
    // Throttle while dragging, always send the last value
    clearTimeout(row.timer);
    if (Date.now() - row.lastSent > 120) send();
    else row.timer = setTimeout(send, 120);
  });
  row.slider.addEventListener("change", send);
  rows.set(dev.id, row);
  return row;
}

function updateRow(row, dev) {
  const { el, slider } = row;
  el.classList.toggle("active", dev.status === "playing");
  el.classList.toggle("connecting", dev.status === "connecting");
  el.classList.toggle("error", dev.status === "error");
  el.querySelector(".dev-icon").innerHTML = ICONS[dev.kind] || ICONS.speaker;
  el.querySelector(".dev-name").textContent = dev.name;
  const model = el.querySelector(".model");
  const st = el.querySelector(".state");
  if (dev.status === "error") {
    model.textContent = "";
    st.textContent = friendlyError(dev.error);
  } else {
    model.textContent = dev.modelName;
    st.textContent = STATE_TEXT[dev.status] ? ` · ${STATE_TEXT[dev.status]}` : "";
  }
  if (!row.dragging && Date.now() - row.lastSent > 600) {
    slider.value = dev.volume;
    setSliderFill(slider);
  }
}

function render() {
  if (!state) return;
  const list = $("#devices");
  const devices = [...state.devices].sort(
    (a, b) => (KIND_ORDER[a.kind] ?? 9) - (KIND_ORDER[b.kind] ?? 9) || a.name.localeCompare(b.name)
  );
  const seen = new Set();
  devices.forEach((dev, i) => {
    seen.add(dev.id);
    const row = rows.get(dev.id) || createRow(dev);
    updateRow(row, dev);
    if (list.children[i] !== row.el) list.insertBefore(row.el, list.children[i] || null);
  });
  for (const [id, row] of rows) {
    if (!seen.has(id)) {
      row.el.remove();
      rows.delete(id);
    }
  }
  $("#empty").hidden = devices.length > 0;
  $("#empty").style.display = devices.length > 0 ? "none" : "";

  const playing = devices.filter((d) => d.status === "playing");
  document.body.classList.toggle("playing", playing.length > 0);
  $("#subtitle").textContent =
    playing.length === 0
      ? "Envoyez le son du PC sur vos HomePods"
      : playing.length === 1
        ? `En lecture sur ${playing[0].name}`
        : `En lecture sur ${playing.length} enceintes`;

  for (const b of document.querySelectorAll("#latency button")) {
    b.setAttribute("aria-checked", String(b.dataset.v === state.latency));
  }
  $("#latency-hint").textContent = `≈ ${(state.latencyMs / 1000).toLocaleString("fr-FR", { maximumFractionDigits: 2 })} s de décalage`;

  renderOutputs();
  $("#indep").checked = state.independentVolume;
  $("#mutepc").checked = state.mutePc;
}

function renderOutputs() {
  const sel = $("#output");
  if (document.activeElement === sel) return;
  const def = state.outputs.find((o) => o.isDefault);
  const current = state.outputDevice ? state.outputs.find((o) => o.id === state.outputDevice) : def;
  $("#output-hint").textContent = current ? current.name : "Ce que Windows joue sur cette sortie";
  const options = [
    { value: "", label: "Par défaut de Windows" },
    ...state.outputs.map((o) => ({ value: o.id, label: o.name })),
  ];
  const html = options.map((o) => `<option value="${escapeAttr(o.value)}">${escapeHtml(o.label)}</option>`).join("");
  if (sel.dataset.html !== html) {
    sel.innerHTML = html;
    sel.dataset.html = html;
  }
  sel.value = state.outputDevice || "";
}

function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}
const escapeAttr = escapeHtml;

// ---- Level meter ----
let shown = 0;
async function meterLoop() {
  if (document.body.classList.contains("playing") && !document.hidden) {
    try {
      const level = await invoke("level");
      // Perceptual scale, fast attack / slow release
      const target = Math.min(1, Math.sqrt(level) * 1.1);
      shown = target > shown ? target : shown * 0.85 + target * 0.15;
      $("#meter-fill").style.width = `${(shown * 100).toFixed(1)}%`;
    } catch (_) {}
  }
  setTimeout(meterLoop, 50);
}

// ---- Now playing (Windows media sessions: Spotify, browsers...) ----
let media = null;
let seeking = false;

function fmtTime(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

function mediaPosition() {
  if (!media) return 0;
  let pos = media.positionMs;
  if (media.playing) pos += Date.now() - media.sampledAtMs;
  return media.durationMs ? Math.min(pos, media.durationMs) : pos;
}

function renderMedia() {
  const card = $("#np");
  card.hidden = !media;
  if (!media) return;
  card.classList.toggle("paused", !media.playing);
  $("#np-title").textContent = media.title || "Lecture en cours";
  $("#np-artist").textContent = [media.artist, media.album].filter(Boolean).join(" — ");
  $("#np-app").textContent = media.app;
  const img = $("#np-art");
  if (media.artwork) {
    if (img.getAttribute("src") !== media.artwork) img.setAttribute("src", media.artwork);
    card.style.setProperty("--art", `url("${media.artwork}")`);
  } else {
    img.removeAttribute("src");
    card.style.removeProperty("--art");
  }
  $("#np-prev").disabled = !media.canPrevious;
  $("#np-next").disabled = !media.canNext;
  $("#np-play").disabled = !media.canPlayPause;
  $("#np-seek").disabled = !media.canSeek;
  renderProgress();
}

function renderProgress() {
  if (!media || seeking) return;
  const seek = $("#np-seek");
  const pos = mediaPosition();
  const dur = media.durationMs;
  seek.value = dur ? Math.round((pos / dur) * 1000) : 0;
  seek.style.setProperty("--p", `${seek.value / 10}%`);
  $("#np-cur").textContent = fmtTime(pos);
  $("#np-dur").textContent = dur ? `-${fmtTime(dur - pos)}` : "";
}
setInterval(renderProgress, 250);

$("#np-play").addEventListener("click", () => {
  if (!media) return;
  // Optimistic: flip now, the next update confirms
  media = { ...media, playing: !media.playing, positionMs: mediaPosition(), sampledAtMs: Date.now() };
  renderMedia();
  invoke("media_cmd", { action: "playpause" });
});
$("#np-next").addEventListener("click", () => invoke("media_cmd", { action: "next" }));
$("#np-prev").addEventListener("click", () => invoke("media_cmd", { action: "previous" }));

const seekEl = $("#np-seek");
seekEl.addEventListener("pointerdown", () => (seeking = true));
seekEl.addEventListener("input", () => {
  seeking = true;
  seekEl.style.setProperty("--p", `${seekEl.value / 10}%`);
  if (media) $("#np-cur").textContent = fmtTime((seekEl.value / 1000) * media.durationMs);
});
seekEl.addEventListener("change", () => {
  if (media) {
    const ms = Math.round((seekEl.value / 1000) * media.durationMs);
    media = { ...media, positionMs: ms, sampledAtMs: Date.now() };
    invoke("media_cmd", { action: "seek", positionMs: ms });
  }
  seeking = false;
  renderProgress();
});

// ---- Firewall ----
async function checkFirewall() {
  const ok = await invoke("firewall_status");
  $("#firewall").hidden = ok;
}

$("#firewall-btn").addEventListener("click", async () => {
  const btn = $("#firewall-btn");
  btn.disabled = true;
  btn.textContent = "…";
  const ok = await invoke("firewall_allow");
  btn.disabled = false;
  btn.textContent = "Autoriser";
  $("#firewall").hidden = ok;
});

// ---- Settings ----
for (const b of document.querySelectorAll("#latency button")) {
  b.addEventListener("click", () => invoke("set_latency", { preset: b.dataset.v }));
}
$("#output").addEventListener("change", (e) => invoke("set_output", { id: e.target.value || null }));
$("#output").addEventListener("focus", () => invoke("refresh_outputs"));
$("#indep").addEventListener("change", (e) => invoke("set_independent_volume", { on: e.target.checked }));
$("#mutepc").addEventListener("change", (e) => invoke("set_mute_pc", { on: e.target.checked }));

// ---- Boot ----
(async () => {
  await listen("state", (e) => {
    state = e.payload;
    render();
  });
  await listen("media", (e) => {
    media = e.payload;
    renderMedia();
  });
  state = await invoke("get_state");
  render();
  media = await invoke("get_media");
  renderMedia();
  meterLoop();
  checkFirewall();
})();
