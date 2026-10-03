/* ==========================================================================
   iPlayer — frontend
   ========================================================================== */
(() => {
  if (!window.__TAURI__ || !window.__TAURI__.core) {
    document.body.innerHTML =
      '<div style="display:grid;place-items:center;height:100vh;font-family:sans-serif;' +
      'color:#98a2b3;background:#0e1014">Tauri API 未加载 — 请通过 `npm run dev` 或打包后的应用启动</div>';
    return;
  }

  const core = window.__TAURI__.core;
  const invoke = core.invoke;
  const convertFileSrc = core.convertFileSrc;
  const listen = window.__TAURI__.event.listen;
  const ICONS = window.ICONS;

  const $ = (id) => document.getElementById(id);
  const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));

  const video = $("video");

  /* ---------------------------------------------------------------------- */
  /* state                                                                  */
  /* ---------------------------------------------------------------------- */

  const DEFAULTS = {
    theme: "dark",
    alwaysOnTop: false,
    fixedSize: false,
    autoplay: true,
    autonext: true,
    resume: true,
    fit: "contain",
    volume: 1,
    muted: false,
    loop: "off",
    speed: 1,
    sidebarOpen: true,
  };

  let settings = { ...DEFAULTS };
  const state = {
    dir: "",
    files: [],
    visible: [],
    index: -1,
    current: null,
    info: null,
    plan: "direct",
    filter: "all",
    recursive: false,
    search: "",
    pickingTime: false,
    toolBusy: false,
    busyKind: null,
    resume: {},
    lastVolume: 1,
  };

  const LS_SETTINGS = "iplayer.settings.v1";
  const LS_RESUME = "iplayer.resume.v1";
  const LS_DIR = "iplayer.lastDir.v1";
  // bumped when a stored default changes and older installs must be migrated once
  const LS_THEME_DEFAULT = "iplayer.themeDefault.v2";

  /* ---------------------------------------------------------------------- */
  /* small helpers                                                          */
  /* ---------------------------------------------------------------------- */

  function pad(n, w = 2) {
    return String(Math.floor(n)).padStart(w, "0");
  }

  function formatTime(sec, forceHours = false) {
    if (!isFinite(sec) || sec < 0) sec = 0;
    const h = Math.floor(sec / 3600);
    const m = Math.floor((sec % 3600) / 60);
    const s = Math.floor(sec % 60);
    if (h > 0 || forceHours) return `${pad(h)}:${pad(m)}:${pad(s)}`;
    return `${pad(m)}:${pad(s)}`;
  }

  function formatTimecode(sec) {
    if (!isFinite(sec) || sec < 0) sec = 0;
    const h = Math.floor(sec / 3600);
    const m = Math.floor((sec % 3600) / 60);
    const s = Math.floor(sec % 60);
    const ms = Math.round((sec - Math.floor(sec)) * 1000);
    return `${pad(h)}:${pad(m)}:${pad(s)}.${pad(ms, 3)}`;
  }

  function parseTimecode(str) {
    if (str == null) return NaN;
    const t = String(str).trim().replace(",", ".");
    if (!t) return NaN;
    if (/^\d+(\.\d+)?$/.test(t)) return parseFloat(t);
    const parts = t.split(":").map((p) => p.trim());
    if (parts.some((p) => p === "" || isNaN(parseFloat(p)))) return NaN;
    let total = 0;
    for (const p of parts) total = total * 60 + parseFloat(p);
    return total;
  }

  function formatBytes(bytes) {
    if (!bytes) return "—";
    const units = ["B", "KB", "MB", "GB", "TB"];
    let i = 0;
    let v = bytes;
    while (v >= 1024 && i < units.length - 1) {
      v /= 1024;
      i++;
    }
    return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
  }

  function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
    );
  }

  function toast(message, kind = "ok", ms = 3200) {
    const host = $("toast-host");
    const el = document.createElement("div");
    el.className = `toast ${kind}`;
    const icon = kind === "err" ? "alert" : kind === "warn" ? "alert" : "check";
    el.innerHTML = ICONS[icon] + `<span>${escapeHtml(message)}</span>`;
    host.appendChild(el);
    setTimeout(() => {
      el.classList.add("leaving");
      setTimeout(() => el.remove(), 220);
    }, ms);
  }

  let osdTimer = null;
  function osd(text) {
    const el = $("osd");
    el.textContent = text;
    el.classList.add("show");
    clearTimeout(osdTimer);
    osdTimer = setTimeout(() => el.classList.remove("show"), 850);
  }

  function saveSettings() {
    try {
      localStorage.setItem(LS_SETTINGS, JSON.stringify(settings));
    } catch (_) {}
  }

  function loadSettings() {
    let hadStored = false;
    try {
      const raw = localStorage.getItem(LS_SETTINGS);
      if (raw) {
        settings = { ...DEFAULTS, ...JSON.parse(raw) };
        hadStored = true;
      }
    } catch (_) {}

    // One-time migration: earlier builds shipped "system" as the default theme.
    // The shipped default is now "dark", so installs still sitting on that old
    // default are moved over once; afterwards the user's own choice is respected.
    try {
      if (!localStorage.getItem(LS_THEME_DEFAULT)) {
        if (hadStored && settings.theme === "system") settings.theme = "dark";
        localStorage.setItem(LS_THEME_DEFAULT, settings.theme);
      }
    } catch (_) {}

    try {
      const raw = localStorage.getItem(LS_RESUME);
      if (raw) state.resume = JSON.parse(raw) || {};
    } catch (_) {
      state.resume = {};
    }
  }

  let resumeFlush = null;
  function rememberPosition(path, time) {
    if (!settings.resume || !path) return;
    state.resume[path] = time;
    clearTimeout(resumeFlush);
    resumeFlush = setTimeout(() => {
      try {
        localStorage.setItem(LS_RESUME, JSON.stringify(state.resume));
      } catch (_) {}
    }, 900);
  }

  /* ---------------------------------------------------------------------- */
  /* theme                                                                  */
  /* ---------------------------------------------------------------------- */

  const darkQuery = window.matchMedia("(prefers-color-scheme: dark)");

  function resolveTheme() {
    if (settings.theme === "system") return darkQuery.matches ? "dark" : "light";
    return settings.theme;
  }

  function applyTheme() {
    document.documentElement.dataset.theme = resolveTheme();

    const btn = $("btn-theme");
    const icon = settings.theme === "system" ? "theme" : settings.theme === "dark" ? "moon" : "sun";
    if (btn.dataset.icon !== icon) {
      btn.dataset.icon = icon;
      delete btn.dataset.iconDone;
      window.hydrateIcons();
    }
    btn.title = `主题：${settings.theme === "system" ? "跟随系统" : settings.theme === "dark" ? "深色" : "浅色"} (Ctrl+T)`;

    invoke("set_theme", { theme: settings.theme === "system" ? null : settings.theme }).catch(() => {});

    const seg = $("set-theme");
    $$(".seg-item", seg).forEach((el) =>
      el.classList.toggle("active", el.dataset.value === settings.theme)
    );
  }

  darkQuery.addEventListener("change", () => {
    if (settings.theme === "system") applyTheme();
  });

  /* ---------------------------------------------------------------------- */
  /* platform & window                                                      */
  /* ---------------------------------------------------------------------- */

  function detectPlatform() {
    const p = navigator.userAgentData?.platform || navigator.platform || "";
    document.documentElement.dataset.platform = /mac/i.test(p) ? "mac" : "other";
  }

  async function applyWindowSettings() {
    try {
      await invoke("set_always_on_top", { value: !!settings.alwaysOnTop });
      await invoke("set_resizable", { value: !settings.fixedSize });
    } catch (e) {
      console.warn(e);
    }
    $("btn-pin").classList.toggle("active", !!settings.alwaysOnTop);
    $("set-ontop").checked = !!settings.alwaysOnTop;
    $("set-fixed").checked = !!settings.fixedSize;
  }

  function setFit(mode) {
    settings.fit = mode;
    document.documentElement.dataset.fit = mode;
    $$("#set-fit .seg-item").forEach((el) =>
      el.classList.toggle("active", el.dataset.value === mode)
    );
    saveSettings();
  }

  /* ---------------------------------------------------------------------- */
  /* sidebar / file list                                                    */
  /* ---------------------------------------------------------------------- */

  function setSidebar(open) {
    settings.sidebarOpen = open;
    $("app").classList.toggle("sidebar-hidden", !open);
    $("btn-sidebar").classList.toggle("active", open);
    saveSettings();
  }

  function applyFilter() {
    const q = state.search.trim().toLowerCase();
    state.visible = state.files.filter((f) => {
      if (state.filter !== "all" && f.kind !== state.filter) return false;
      if (q && !f.name.toLowerCase().includes(q)) return false;
      return true;
    });
    renderList();
  }

  function highlight(name) {
    const q = state.search.trim();
    if (!q) return escapeHtml(name);
    const idx = name.toLowerCase().indexOf(q.toLowerCase());
    if (idx < 0) return escapeHtml(name);
    return (
      escapeHtml(name.slice(0, idx)) +
      `<span class="hl">${escapeHtml(name.slice(idx, idx + q.length))}</span>` +
      escapeHtml(name.slice(idx + q.length))
    );
  }

  function renderList() {
    const ul = $("file-list");
    ul.innerHTML = "";

    if (!state.files.length) {
      ul.innerHTML = `<li class="list-empty">这个文件夹里没有可播放的媒体文件<br>试试「打开文件夹」或直接把文件拖进来</li>`;
      return;
    }
    if (!state.visible.length) {
      ul.innerHTML = `<li class="list-empty">没有匹配的文件</li>`;
      return;
    }

    const frag = document.createDocumentFragment();
    state.visible.forEach((f) => {
      const li = document.createElement("li");
      li.className = "file-item";
      li.dataset.path = f.path;
      if (state.current && state.current.path === f.path) li.classList.add("active");

      const saved = state.resume[f.path];
      const pct =
        saved && state.info && state.current && state.current.path === f.path && state.info.duration
          ? Math.min(100, (saved / state.info.duration) * 100)
          : 0;

      const icon = f.kind === "image" ? ICONS.image : f.kind === "audio" ? ICONS.music : ICONS.film;
      li.innerHTML =
        `<span class="fi-icon">${icon}</span>` +
        `<span class="fi-name" title="${escapeHtml(f.name)}">${highlight(f.name)}</span>` +
        `<span class="fi-meta">${formatBytes(f.size)}</span>` +
        (pct > 1 ? `<span class="fi-progress"><i style="width:${pct.toFixed(1)}%"></i></span>` : "");

      li.addEventListener("click", () => {
        const i = state.files.findIndex((x) => x.path === f.path);
        if (i >= 0) playIndex(i);
      });
      frag.appendChild(li);
    });
    ul.appendChild(frag);
  }

  function scrollActiveIntoView() {
    const el = $("file-list").querySelector(".file-item.active");
    if (el) el.scrollIntoView({ block: "nearest" });
  }

  async function scanFolder(dir, { keepCurrent = true } = {}) {
    try {
      const files = await invoke("scan_folder", { dir, recursive: state.recursive });
      state.dir = dir;
      state.files = files;
      try {
        localStorage.setItem(LS_DIR, dir);
      } catch (_) {}
      const el = $("side-path-text");
      el.textContent = dir;
      $("side-path").title = dir;
      if (!keepCurrent) state.current = null;
      applyFilter();
      return files;
    } catch (e) {
      toast(String(e), "err");
      return [];
    }
  }

  /* ---------------------------------------------------------------------- */
  /* playback                                                               */
  /* ---------------------------------------------------------------------- */

  function showEmpty(show) {
    $("empty").classList.toggle("hidden", !show);
  }

  function setBusy(show, label = "", pct = 0, note = "") {
    const el = $("busy");
    el.classList.toggle("hidden", !show);
    if (show) {
      $("busy-label").textContent = label;
      $("busy-bar").style.width = `${Math.max(0, Math.min(100, pct))}%`;
      $("busy-pct").textContent = pct > 0 ? `${pct.toFixed(0)}%` : "";
      $("busy-note").textContent = note;
    }
  }

  function planLabel(plan) {
    return plan === "remux" ? "转封装" : plan === "transcode" ? "格式转换" : "直接播放";
  }

  async function playIndex(i) {
    if (i < 0 || i >= state.files.length) return;
    state.index = i;
    await openFile(state.files[i]);
  }

  async function openFile(file, { autoplay = null } = {}) {
    if (file.kind === "image") return openImage(file);
    const shouldPlay = autoplay == null ? settings.autoplay : autoplay;

    // remember where we were in the outgoing file
    if (state.current && !video.paused && state.info?.duration) {
      rememberPosition(state.current.path, video.currentTime);
    }

    state.current = file;
    state.info = null;
    state.plan = "direct";
    renderList();
    scrollActiveIntoView();
    showEmpty(false);
    $("tb-sub").textContent = file.name;
    document.title = `${file.name} — iPlayer`;

    setBusy(true, `正在打开 ${file.name}`, 0, "");
    state.busyKind = "media";

    let res;
    try {
      res = await invoke("prepare_playback", { path: file.path });
    } catch (e) {
      setBusy(false);
      state.busyKind = null;
      toast(String(e), "err", 5200);
      return;
    }
    setBusy(false);
    state.busyKind = null;

    state.info = res.info;
    state.plan = res.plan;

    $("app").classList.remove("is-image");
    const imgEl = $("image");
    imgEl.removeAttribute("src");
    imgEl.classList.add("hidden");
    video.src = convertFileSrc(res.path);
    video.playbackRate = settings.speed;

    // restore position
    const saved = settings.resume ? state.resume[file.path] : 0;
    if (saved && saved > 3 && res.info.duration && saved < res.info.duration - 5) {
      const seekTo = () => {
        video.currentTime = saved;
        video.removeEventListener("loadedmetadata", seekTo);
      };
      video.addEventListener("loadedmetadata", seekTo);
    }

    video.load();
    if (shouldPlay) {
      const p = video.play();
      if (p && p.catch) p.catch(() => {});
    }
    renderInfo();
    if (res.plan !== "direct") {
      osd(res.cached ? `${planLabel(res.plan)} · 已用缓存` : planLabel(res.plan));
    }
    refreshWindowState();
  }

  function openImage(file) {
    if (state.current && !video.paused) {
      video.pause();
    }
    state.current = file;
    state.info = null;
    state.plan = "direct";
    renderList();
    scrollActiveIntoView();
    showEmpty(false);
    $("tb-sub").textContent = file.name;
    document.title = `${file.name} — iPlayer`;

    // stop any ongoing playback
    video.pause();
    video.removeAttribute("src");
    video.load();

    const img = $("image");
    img.src = convertFileSrc(file.path);
    img.classList.remove("hidden");
    $("app").classList.add("is-image");

    $("t-cur").textContent = "—";
    $("t-dur").textContent = "—";
    seekPlayed.style.width = "0%";
    seekHandle.style.left = "0%";
    seekBuffer.style.width = "0%";
    updatePlayIcon();
    renderInfo();
    refreshWindowState();
  }

  function nextFile(step = 1) {
    if (!state.files.length) return;
    let i = state.index;
    for (let n = 0; n < state.files.length; n++) {
      i = (i + step + state.files.length) % state.files.length;
      if (state.files[i]) {
        playIndex(i);
        return;
      }
    }
  }

  /* ---------------------------------------------------------------------- */
  /* transport controls                                                     */
  /* ---------------------------------------------------------------------- */

  function updatePlayIcon() {
    const btn = $("btn-play");
    const icon = video.paused ? "play" : "pause";
    if (btn.dataset.icon !== icon) {
      btn.dataset.icon = icon;
      delete btn.dataset.iconDone;
      window.hydrateIcons();
    }
    btn.title = video.paused ? "播放 (空格)" : "暂停 (空格)";
  }

  function togglePlay() {
    if (!state.current || state.current.kind === "image") return;
    if (video.paused) {
      video.play().catch(() => {});
    } else {
      video.pause();
    }
  }

  function updateVolumeIcon() {
    const btn = $("btn-mute");
    const v = video.muted ? 0 : video.volume;
    const icon = v <= 0.001 ? "volumeX" : v < 0.5 ? "volume1" : "volume";
    if (btn.dataset.icon !== icon) {
      btn.dataset.icon = icon;
      delete btn.dataset.iconDone;
      window.hydrateIcons();
    }
    btn.classList.toggle("active", video.muted || v <= 0.001);
  }

  function setVolume(v, { persist = true } = {}) {
    video.volume = Math.max(0, Math.min(1, v));
    if (video.volume > 0) video.muted = false;
    $("volume").value = String(Math.round(video.volume * 100));
    updateVolumeIcon();
    if (persist) {
      settings.volume = video.volume;
      settings.muted = video.muted;
      saveSettings();
    }
  }

  function cycleLoop() {
    settings.loop = settings.loop === "off" ? "all" : settings.loop === "all" ? "one" : "off";
    const btn = $("btn-loop");
    const icon = settings.loop === "one" ? "repeat1" : "repeat";
    btn.dataset.icon = icon;
    delete btn.dataset.iconDone;
    window.hydrateIcons();
    btn.classList.toggle("active", settings.loop !== "off");
    btn.title =
      settings.loop === "off" ? "循环：关闭" : settings.loop === "all" ? "循环：列表循环" : "循环：单曲循环";
    osd(`循环：${settings.loop === "off" ? "关闭" : settings.loop === "all" ? "列表" : "单曲"}`);
    saveSettings();
  }

  /* seek bar ---------------------------------------------------------------- */

  const seek = $("seek");
  const seekPlayed = $("seek-played");
  const seekBuffer = $("seek-buffer");
  const seekHandle = $("seek-handle");
  const seekTip = $("seek-tip");
  let seekDragging = false;

  function seekRatio() {
    const d = video.duration;
    if (!d || !isFinite(d)) return 0;
    return Math.min(1, Math.max(0, video.currentTime / d));
  }

  function paintSeek() {
    const r = seekRatio();
    seekPlayed.style.width = `${r * 100}%`;
    seekHandle.style.left = `${r * 100}%`;
    if (video.buffered && video.buffered.length && video.duration) {
      let end = 0;
      for (let i = 0; i < video.buffered.length; i++) {
        if (video.buffered.start(i) <= video.currentTime + 0.5) {
          end = Math.max(end, video.buffered.end(i));
        }
      }
      seekBuffer.style.width = `${Math.min(100, (end / video.duration) * 100)}%`;
    }
    $("t-cur").textContent = formatTime(video.currentTime, video.duration >= 3600);
  }

  function ratioFromEvent(ev) {
    const rect = seek.getBoundingClientRect();
    return Math.min(1, Math.max(0, (ev.clientX - rect.left) / rect.width));
  }

  seek.addEventListener("pointerdown", (ev) => {
    if (!state.current) return;
    seekDragging = true;
    seek.classList.add("dragging");
    seek.setPointerCapture(ev.pointerId);
    const r = ratioFromEvent(ev);
    if (video.duration) video.currentTime = r * video.duration;
    paintSeek();
    seekTip.style.left = `${r * 100}%`;
    seekTip.textContent = formatTime((video.duration || 0) * r);
  });

  seek.addEventListener("pointermove", (ev) => {
    const r = ratioFromEvent(ev);
    seekTip.style.left = `${r * 100}%`;
    seekTip.textContent = formatTime((video.duration || 0) * r, video.duration >= 3600);
    if (seekDragging && video.duration) {
      video.currentTime = r * video.duration;
      paintSeek();
    }
  });

  const endSeek = (ev) => {
    if (!seekDragging) return;
    seekDragging = false;
    seek.classList.remove("dragging");
    try {
      seek.releasePointerCapture(ev.pointerId);
    } catch (_) {}
  };
  seek.addEventListener("pointerup", endSeek);
  seek.addEventListener("pointercancel", endSeek);

  /* ---------------------------------------------------------------------- */
  /* video events                                                           */
  /* ---------------------------------------------------------------------- */

  video.addEventListener("play", () => {
    updatePlayIcon();
    osd("▶ 播放");
  });
  video.addEventListener("pause", () => {
    updatePlayIcon();
    if (state.current) rememberPosition(state.current.path, video.currentTime);
  });
  video.addEventListener("timeupdate", () => {
    paintSeek();
    if (state.current && Math.floor(video.currentTime) % 5 === 0) {
      rememberPosition(state.current.path, video.currentTime);
    }
  });
  video.addEventListener("progress", paintSeek);
  video.addEventListener("durationchange", () => {
    $("t-dur").textContent = formatTime(video.duration, video.duration >= 3600);
    paintSeek();
  });
  video.addEventListener("volumechange", updateVolumeIcon);
  video.addEventListener("ratechange", () => {
    $("speed").value = String(video.playbackRate);
  });
  video.addEventListener("ended", () => {
    if (state.current) rememberPosition(state.current.path, 0);
    if (settings.loop === "one") {
      video.currentTime = 0;
      video.play().catch(() => {});
      return;
    }
    if (settings.autonext) {
      if (state.index >= state.files.length - 1 && settings.loop === "off") {
        updatePlayIcon();
        return;
      }
      nextFile(1);
    }
  });
  video.addEventListener("error", () => {
    const err = video.error;
    if (!err || !state.current) return;
    const map = {
      1: "加载被中断",
      2: "网络错误",
      3: "解码失败——该编码可能不受支持",
      4: "当前格式无法播放",
    };
    toast(`播放失败：${map[err.code] || "未知错误"}`, "err", 5000);
  });

  /* ---------------------------------------------------------------------- */
  /* media info pane                                                        */
  /* ---------------------------------------------------------------------- */

  function renderInfo() {
    const grid = $("info-grid");
    const f = state.current;
    const i = state.info;
    if (!f) {
      grid.innerHTML = `<div class="k">状态</div><div class="v">尚未打开文件</div>`;
      return;
    }
    const rows = [
      ["文件名", f.name],
      ["路径", f.path],
      ["播放方式", i ? planLabel(i.plan) + (state.plan !== "direct" ? "（已生成可播放副本）" : "") : "—"],
      ["容器格式", i ? i.format_name || i.ext : "—"],
      ["时长", i ? formatTime(i.duration, true) : "—"],
      ["分辨率", i && i.width ? `${i.width} × ${i.height}` : "—"],
      ["帧率", i && i.fps ? `${i.fps.toFixed(3)} fps` : "—"],
      ["视频编码", (i && i.vcodec) || "无"],
      ["音频编码", (i && i.acodec) || "无"],
      ["声道 / 采样率", i && i.has_audio ? `${i.channels} 声道 · ${i.sample_rate} Hz` : "—"],
      ["总码率", i && i.bitrate ? `${(i.bitrate / 1e6).toFixed(2)} Mbps` : "—"],
      ["文件大小", formatBytes(f.size)],
    ];
    grid.innerHTML = rows
      .map(([k, v]) => `<div class="k">${escapeHtml(k)}</div><div class="v">${escapeHtml(v)}</div>`)
      .join("");
  }

  function infoAsText() {
    const f = state.current;
    if (!f) return "";
    const i = state.info || {};
    return [
      `文件名: ${f.name}`,
      `路径: ${f.path}`,
      `容器格式: ${i.format_name || f.ext}`,
      `时长: ${formatTime(i.duration || 0, true)}`,
      `分辨率: ${i.width ? `${i.width}x${i.height}` : "—"}`,
      `帧率: ${i.fps ? i.fps.toFixed(3) : "—"}`,
      `视频编码: ${i.vcodec || "无"}`,
      `音频编码: ${i.acodec || "无"}`,
      `码率: ${i.bitrate ? (i.bitrate / 1e6).toFixed(2) + " Mbps" : "—"}`,
      `大小: ${formatBytes(f.size)}`,
    ].join("\n");
  }

  /* ---------------------------------------------------------------------- */
  /* modal                                                                  */
  /* ---------------------------------------------------------------------- */

  function openModal(tab = "shot") {
    $("modal").classList.remove("hidden");
    selectTab(tab);
  }
  function closeModal() {
    $("modal").classList.add("hidden");
  }
  function selectTab(name) {
    $$("#modal-tabs .tab").forEach((t) => t.classList.toggle("active", t.dataset.tab === name));
    $$(".pane").forEach((p) => p.classList.toggle("active", p.dataset.pane === name));
    if (name === "info") renderInfo();
    if (name === "gif") syncGifLength();
  }

  function modalProgress(show, pct = 0, note = "") {
    const el = $("modal-progress");
    if (!el) return;
    el.classList.toggle("hidden", !show);
    $("modal-progress-fill").style.width = `${Math.max(0, Math.min(100, pct))}%`;
    $("modal-progress-note").textContent = note;
  }

  async function withToolButton(btn, fn) {
    if (state.toolBusy) return;
    state.toolBusy = true;
    const original = btn.innerHTML;
    btn.disabled = true;
    btn.innerHTML = ICONS.reset + `<span class="ico-label">处理中…</span>`;
    modalProgress(true, 0, "正在启动 ffmpeg…");
    try {
      const out = await fn();
      modalProgress(false);
      if (out) toast(`已保存：${out.split("/").pop()}`, "ok", 6000);
    } catch (e) {
      modalProgress(false);
      toast(String(e).replace(/^Error:\s*/, ""), "err", 6000);
    } finally {
      state.toolBusy = false;
      btn.disabled = false;
      btn.innerHTML = original;
      $("busy-note").textContent = "";
    }
  }

  async function pickSavePath(defaultName) {
    try {
      return await invoke("plugin:dialog|save", {
        options: { title: "保存到", defaultPath: defaultName || undefined },
      });
    } catch (e) {
      console.warn(e);
      return null;
    }
  }

  /* ---------------------------------------------------------------------- */
  /* tool actions                                                           */
  /* ---------------------------------------------------------------------- */

  function currentPath() {
    return state.current ? state.current.path : null;
  }

  async function doSnapshot() {
    const p = currentPath();
    if (!p) return toast("请先打开一个视频", "warn");
    const t = parseTimecode($("shot-time").value);
    const time = isNaN(t) ? video.currentTime : t;
    const out = $("shot-out").value.trim();
    await withToolButton($("shot-run"), () =>
      invoke("snapshot", { path: p, time, out: out || null })
    );
  }

  async function doAudio() {
    const p = currentPath();
    if (!p) return toast("请先打开一个视频", "warn");
    const format = $("#audio-format .seg-item.active")?.dataset.value || "m4a";
    const out = $("audio-out").value.trim();
    await withToolButton($("audio-run"), () =>
      invoke("extract_audio", { path: p, format, out: out || null })
    );
  }

  async function doGif() {
    const p = currentPath();
    if (!p) return toast("请先打开一个视频", "warn");
    const start = parseTimecode($("gif-start").value);
    const end = parseTimecode($("gif-end").value);
    if (isNaN(start) || isNaN(end)) return toast("时间格式不正确，应为 HH:MM:SS.mmm", "err");
    if (end <= start) return toast("结束时间必须大于开始时间", "err");
    const fps = parseInt($("gif-fps").value, 10) || 12;
    const width = parseInt($("gif-width").value, 10) || 480;
    const dither = $("gif-dither").checked;
    const out = $("gif-out").value.trim();
    await withToolButton($("gif-run"), () =>
      invoke("make_gif", {
        path: p,
        start,
        end,
        fps,
        width,
        dither,
        out: out || null,
      })
    );
  }

  function syncGifLength() {
    const s = parseTimecode($("gif-start").value);
    const e = parseTimecode($("gif-end").value);
    const el = $("gif-len");
    if (isNaN(s) || isNaN(e)) {
      el.textContent = "时间格式：HH:MM:SS.mmm";
      return;
    }
    const d = e - s;
    const fps = parseInt($("gif-fps").value, 10) || 12;
    el.textContent =
      d > 0
        ? `区间长度 ${d.toFixed(2)} 秒 ≈ ${Math.round(d * fps)} 帧${d > 30 ? "（较长，转换会比较慢）" : ""}`
        : "结束时间必须大于开始时间";
  }

  /* ---------------------------------------------------------------------- */
  /* env / tool status                                                      */
  /* ---------------------------------------------------------------------- */

  async function refreshToolStatus() {
    const box = $("env-box");
    box.innerHTML = "正在检测 ffmpeg…";
    try {
      const st = await invoke("tool_status");
      const ok = st.ffmpeg ? "good" : "bad";
      box.innerHTML =
        `<div>ffmpeg : <span class="${ok}">${escapeHtml(st.ffmpeg || "未找到")}</span></div>` +
        `<div>ffprobe: <span class="${st.ffprobe ? "good" : "bad"}">${escapeHtml(st.ffprobe || "未找到")}</span></div>` +
        `<div>版本   : ${escapeHtml(st.version || "—")}</div>` +
        `<div>编码器 : ${st.encoders.length} 个可用${st.encoders.includes("libx264") ? "（含 libx264）" : ""}</div>`;
      if (!st.ffmpeg) {
        toast("未检测到 ffmpeg，转封装 / 截图 / GIF 等功能不可用", "warn", 6000);
      }
    } catch (e) {
      box.innerHTML = `<span class="bad">检测失败：${escapeHtml(String(e))}</span>`;
    }
  }

  /// Window state refresh (maximized icon etc.)
  async function refreshWindowState() {
    const btn = $("btn-max");
    if (!btn) return;
    // Best effort: the browser API is available when the capability is granted.
    try {
      const w = window.__TAURI__.window.getCurrentWindow();
      const max = await w.isMaximized();
      if (btn.dataset.icon !== (max ? "restore" : "maximize")) {
        btn.dataset.icon = max ? "restore" : "maximize";
        delete btn.dataset.iconDone;
        window.hydrateIcons();
      }
    } catch (_) {}
  }

  /* ---------------------------------------------------------------------- */
  /* drag & drop                                                            */
  /* ---------------------------------------------------------------------- */

  function bindDragDrop() {
    const zone = $("dropzone");
    const show = (v) => zone.classList.toggle("hidden", !v);

    listen("tauri://drag-enter", () => show(true));
    listen("tauri://drag-leave", () => show(false));
    listen("tauri://drag-drop", async (ev) => {
      show(false);
      const paths = ev.payload?.paths || [];
      if (!paths.length) return;
      const first = paths[0];
      if (paths.length > 1) {
        await openPathOrFolder(first);
        return;
      }
      await openPathOrFolder(first);
    });

    // HTML5 fallback (used when the native handler is unavailable)
    window.addEventListener("dragover", (e) => {
      e.preventDefault();
      show(true);
    });
    window.addEventListener("dragleave", () => show(false));
    window.addEventListener("drop", async (e) => {
      e.preventDefault();
      show(false);
      const f = e.dataTransfer?.files?.[0];
      if (f && f.path) await openPathOrFolder(f.path);
    });
  }

  const IMAGE_RE = /\.(jpe?g|png|gif|webp|bmp|svg|ico|avif)$/i;
  const AUDIO_RE = /\.(mp3|m4a|aac|flac|wav|ogg|oga|opus|wma|ape|mka)$/i;

  async function openPathOrFolder(path) {
    const isImage = IMAGE_RE.test(path);
    const isMedia = isImage || /\.(mp4|m4v|mov|mkv|avi|flv|wmv|webm|mpg|mpeg|ts|m2ts|mts|rmvb|rm|3gp|ogv|vob|mp3|m4a|aac|flac|wav|ogg|oga|opus|wma|ape|mka)$/i.test(
      path
    );
    if (isMedia) {
      const dir = path.replace(/[\\/][^\\/]*$/, "");
      await scanFolder(dir, { keepCurrent: true });
      const kind = isImage ? "image" : AUDIO_RE.test(path) ? "audio" : "video";
      const i = state.files.findIndex((f) => f.path === path);
      if (i >= 0) await playIndex(i);
      else await openFile({ name: path.split(/[\\/]/).pop(), path, kind });
      return;
    }
    await scanFolder(path, { keepCurrent: false });
    if (state.files.length) await playIndex(0);
    else toast("该文件夹里没有可播放的媒体文件", "warn");
  }

  /* ---------------------------------------------------------------------- */
  /* shortcuts                                                              */
  /* ---------------------------------------------------------------------- */

  function bindShortcuts() {
    window.addEventListener("keydown", (e) => {
      const tag = (e.target.tagName || "").toLowerCase();
      if (tag === "input" || tag === "textarea" || tag === "select") {
        if (e.key === "Escape") e.target.blur();
        return;
      }
      if (tag === "button" && (e.key === " " || e.key === "Enter")) {
        return; // let the focused button handle activation itself
      }
      const modalOpen = !$("modal").classList.contains("hidden");
      if (e.key === "Escape") {
        if (modalOpen) {
          closeModal();
          return;
        }
      }
      if (modalOpen) return; // ignore player shortcuts while a dialog is up

      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.key.toLowerCase() === "b") {
        e.preventDefault();
        setSidebar(!settings.sidebarOpen);
        return;
      }
      if (mod && e.key.toLowerCase() === "t") {
        e.preventDefault();
        settings.theme =
          settings.theme === "system" ? "dark" : settings.theme === "dark" ? "light" : "system";
        applyTheme();
        saveSettings();
        osd(`主题：${settings.theme === "system" ? "跟随系统" : settings.theme === "dark" ? "深色" : "浅色"}`);
        return;
      }
      if (mod && e.key === ".") {
        e.preventDefault();
        video.pause();
        video.currentTime = 0;
        return;
      }

      switch (e.key) {
        case " ":
        case "k":
          e.preventDefault();
          togglePlay();
          break;
        case "ArrowRight":
          e.preventDefault();
          video.currentTime = Math.min(video.duration || 0, video.currentTime + (e.shiftKey ? 1 : 5));
          osd(`+${e.shiftKey ? 1 : 5}s`);
          break;
        case "ArrowLeft":
          e.preventDefault();
          video.currentTime = Math.max(0, video.currentTime - (e.shiftKey ? 1 : 5));
          osd(`-${e.shiftKey ? 1 : 5}s`);
          break;
        case "ArrowUp":
          e.preventDefault();
          setVolume(video.volume + 0.05);
          osd(`音量 ${Math.round(video.volume * 100)}%`);
          break;
        case "ArrowDown":
          e.preventDefault();
          setVolume(video.volume - 0.05);
          osd(`音量 ${Math.round(video.volume * 100)}%`);
          break;
        case "m":
          video.muted = !video.muted;
          osd(video.muted ? "静音" : "取消静音");
          break;
        case "f":
          toggleFullscreen();
          break;
        case "s":
          doSnapshot();
          break;
        case "g":
          openModal("gif");
          break;
        case "i":
          openModal("info");
          break;
        case "n":
          nextFile(1);
          break;
        case "p":
          nextFile(-1);
          break;
        case "[":
          setSpeed(Math.max(0.5, +(video.playbackRate - 0.5).toFixed(2)));
          break;
        case "]":
          setSpeed(Math.min(3, +(video.playbackRate + 0.5).toFixed(2)));
          break;
        default:
          break;
      }
    });
  }

  function setSpeed(rate) {
    video.playbackRate = rate;
    settings.speed = rate;
    $("speed").value = String(rate);
    saveSettings();
    osd(`${rate}x`);
  }

  async function toggleFullscreen() {
    try {
      const isFull = await invoke("set_fullscreen", { value: !(await isFullscreen()) });
      $("btn-full").dataset.icon = isFull ? "compress" : "expand";
      delete $("btn-full").dataset.iconDone;
      window.hydrateIcons();
    } catch (e) {
      console.warn(e);
    }
  }

  async function isFullscreen() {
    try {
      return await window.__TAURI__.window.getCurrentWindow().isFullscreen();
    } catch (_) {
      return false;
    }
  }

  /* ---------------------------------------------------------------------- */
  /* wiring                                                                 */
  /* ---------------------------------------------------------------------- */

  function bindUI() {
    // titlebar
    $("btn-sidebar").addEventListener("click", () => setSidebar(!settings.sidebarOpen));
    $("btn-pin").addEventListener("click", async () => {
      settings.alwaysOnTop = !settings.alwaysOnTop;
      await applyWindowSettings();
      saveSettings();
      osd(settings.alwaysOnTop ? "窗口置顶：开" : "窗口置顶：关");
    });
    $("btn-theme").addEventListener("click", () => {
      settings.theme =
        settings.theme === "system" ? "dark" : settings.theme === "dark" ? "light" : "system";
      applyTheme();
      saveSettings();
      osd(`主题：${settings.theme === "system" ? "跟随系统" : settings.theme === "dark" ? "深色" : "浅色"}`);
    });
    const w = window.__TAURI__.window.getCurrentWindow();
    $("btn-min").addEventListener("click", () => w.minimize());
    $("btn-max").addEventListener("click", async () => {
      await w.toggleMaximize();
      refreshWindowState();
    });
    $("btn-close").addEventListener("click", () => w.close());

    // sidebar
    $("btn-open-folder").addEventListener("click", pickFolder);
    $("btn-empty-folder").addEventListener("click", pickFolder);
    $("btn-open-file").addEventListener("click", pickFile);
    $("btn-empty-file").addEventListener("click", pickFile);
    $("btn-refresh").addEventListener("click", () => {
      if (!state.dir) return toast("尚未选择文件夹", "warn");
      scanFolder(state.dir).then(() => toast("已刷新列表", "ok", 1500));
    });
    $("btn-reveal-dir").addEventListener("click", () => {
      if (state.dir) invoke("reveal_in_finder", { path: state.dir }).catch((e) => toast(String(e), "err"));
    });
    $("btn-clear-list").addEventListener("click", () => {
      state.files = [];
      state.visible = [];
      state.dir = "";
      $("side-path-text").textContent = "";
      try {
        localStorage.removeItem(LS_DIR);
      } catch (_) {}
      applyFilter();
    });

    $("search").addEventListener("input", (e) => {
      state.search = e.target.value;
      $("btn-clear-search").classList.toggle("hidden", !state.search);
      applyFilter();
    });
    $("btn-clear-search").addEventListener("click", () => {
      $("search").value = "";
      state.search = "";
      $("btn-clear-search").classList.add("hidden");
      applyFilter();
    });

    $$(".side-filter .chip").forEach((chip) => {
      chip.addEventListener("click", () => {
        state.filter = chip.dataset.filter;
        $$(".side-filter .chip").forEach((c) => c.classList.toggle("active", c === chip));
        applyFilter();
      });
    });
    $("chk-recursive").addEventListener("change", async (e) => {
      state.recursive = e.target.checked;
      if (state.dir) await scanFolder(state.dir);
    });

    // transport
    $("btn-play").addEventListener("click", togglePlay);
    $("btn-prev").addEventListener("click", () => nextFile(-1));
    $("btn-next").addEventListener("click", () => nextFile(1));
    $("btn-stop").addEventListener("click", () => {
      video.pause();
      video.currentTime = 0;
    });
    $("btn-mute").addEventListener("click", () => {
      if (video.muted || video.volume === 0) {
        setVolume(state.lastVolume || 0.6);
      } else {
        state.lastVolume = video.volume;
        video.muted = true;
        settings.muted = true;
        saveSettings();
        updateVolumeIcon();
      }
      osd(video.muted ? "静音" : `音量 ${Math.round(video.volume * 100)}%`);
      $("volume").value = String(Math.round((video.muted ? 0 : video.volume) * 100));
    });
    $("volume").addEventListener("input", (e) => setVolume(parseInt(e.target.value, 10) / 100));
    $("speed").addEventListener("change", (e) => setSpeed(parseFloat(e.target.value)));
    $("btn-loop").addEventListener("click", cycleLoop);
    $("btn-shot").addEventListener("click", () => {
      $("shot-time").value = formatTimecode(video.currentTime || 0);
      openModal("shot");
    });
    $("btn-tools").addEventListener("click", () => {
      $("shot-time").value = formatTimecode(video.currentTime || 0);
      $("gif-start").value = formatTimecode(video.currentTime || 0);
      $("gif-end").value = formatTimecode(
        Math.min(video.duration || (video.currentTime || 0) + 5, (video.currentTime || 0) + 5)
      );
      openModal("shot");
    });
    $("btn-summary").addEventListener("click", () => openModal("info"));
    $("btn-full").addEventListener("click", toggleFullscreen);

    // stage interactions
    let clickTimer = null;
    $("stage").addEventListener("click", (e) => {
      if (e.target.closest("button, input, select, .modal")) return;
      if (clickTimer) {
        clearTimeout(clickTimer);
        clickTimer = null;
        toggleFullscreen();
        return;
      }
      clickTimer = setTimeout(() => {
        clickTimer = null;
        if (state.current) togglePlay();
      }, 220);
    });

    // modal
    $$("#modal [data-close]").forEach((el) => el.addEventListener("click", closeModal));
    $$("#modal-tabs .tab").forEach((t) =>
      t.addEventListener("click", () => selectTab(t.dataset.tab))
    );

    // tool panes
    $("shot-use-current").addEventListener("click", () => {
      $("shot-time").value = formatTimecode(video.currentTime || 0);
    });
    $("shot-set-start").addEventListener("click", () => {
      $("gif-start").value = formatTimecode(video.currentTime || 0);
      syncGifLength();
      osd("已设为 GIF 起点");
    });
    $("shot-pick").addEventListener("click", async () => {
      const p = state.current
        ? state.current.path.replace(/\.[^.]+$/, "") + `_shot_${Date.now()}.png`
        : "snapshot.png";
      const out = await pickSavePath(p);
      if (out) $("shot-out").value = out;
    });
    $("shot-run").addEventListener("click", doSnapshot);

    $$("#audio-format .seg-item").forEach((el) =>
      el.addEventListener("click", () => {
        $$("#audio-format .seg-item").forEach((x) => x.classList.toggle("active", x === el));
      })
    );
    $("audio-pick").addEventListener("click", async () => {
      const ext = $("#audio-format .seg-item.active")?.dataset.value || "m4a";
      const realExt = ext === "copy" ? "mka" : ext;
      const p = state.current ? state.current.path.replace(/\.[^.]+$/, "") + "." + realExt : "audio." + realExt;
      const out = await pickSavePath(p);
      if (out) $("audio-out").value = out;
    });
    $("audio-run").addEventListener("click", doAudio);

    $("gif-use-current").addEventListener("click", () => {
      $("gif-start").value = formatTimecode(video.currentTime || 0);
      syncGifLength();
    });
    $("gif-use-end").addEventListener("click", () => {
      $("gif-end").value = formatTimecode(video.currentTime || 0);
      syncGifLength();
    });
    ["gif-start", "gif-end", "gif-fps"].forEach((id) =>
      $(id).addEventListener("input", syncGifLength)
    );
    $("gif-pick").addEventListener("click", async () => {
      const p = state.current
        ? state.current.path.replace(/\.[^.]+$/, "") + "_clip.gif"
        : "clip.gif";
      const out = await pickSavePath(p);
      if (out) $("gif-out").value = out;
    });
    $("gif-run").addEventListener("click", doGif);

    $("info-copy").addEventListener("click", async () => {
      const text = infoAsText();
      if (!text) return;
      try {
        await navigator.clipboard.writeText(text);
        toast("媒体信息已复制", "ok", 2000);
      } catch (_) {
        toast("复制失败", "err");
      }
    });
    $("info-reveal").addEventListener("click", () => {
      if (state.current) invoke("reveal_in_finder", { path: state.current.path }).catch(() => {});
    });

    // settings
    $$("#set-theme .seg-item").forEach((el) =>
      el.addEventListener("click", () => {
        settings.theme = el.dataset.value;
        applyTheme();
        saveSettings();
      })
    );
    $("set-ontop").addEventListener("change", async (e) => {
      settings.alwaysOnTop = e.target.checked;
      await applyWindowSettings();
      saveSettings();
    });
    $("set-fixed").addEventListener("change", async (e) => {
      settings.fixedSize = e.target.checked;
      await applyWindowSettings();
      saveSettings();
    });
    $("set-autoplay").addEventListener("change", (e) => {
      settings.autoplay = e.target.checked;
      saveSettings();
    });
    $("set-autonext").addEventListener("change", (e) => {
      settings.autonext = e.target.checked;
      saveSettings();
    });
    $("set-resume").addEventListener("change", (e) => {
      settings.resume = e.target.checked;
      saveSettings();
    });
    $$("#set-fit .seg-item").forEach((el) =>
      el.addEventListener("click", () => setFit(el.dataset.value))
    );

    window.addEventListener("beforeunload", () => {
      if (state.current) rememberPosition(state.current.path, video.currentTime);
      try {
        localStorage.setItem(LS_RESUME, JSON.stringify(state.resume));
      } catch (_) {}
    });

    window.addEventListener("resize", refreshWindowState);
    document.addEventListener("contextmenu", (e) => {
      if (!e.target.closest("input, textarea")) e.preventDefault();
    });
  }

  async function pickFolder() {
    try {
      const dir = await invoke("plugin:dialog|open", {
        options: { title: "选择文件夹", directory: true, multiple: false },
      });
      if (!dir || Array.isArray(dir)) return;
      const files = await scanFolder(dir, { keepCurrent: false });
      if (files.length) await playIndex(0);
      else toast("该文件夹里没有可播放的媒体文件", "warn");
    } catch (e) {
      toast(String(e), "err");
    }
  }

  async function pickFile() {
    try {
      const path = await invoke("plugin:dialog|open", {
        options: {
          title: "选择媒体文件",
          multiple: false,
          filters: [
            {
              name: "媒体文件",
              extensions: [
                "mp4", "m4v", "mov", "mkv", "avi", "flv", "wmv", "webm", "mpg", "mpeg", "ts",
                "m2ts", "mts", "rmvb", "rm", "3gp", "ogv", "vob", "mp3", "m4a", "aac", "flac",
                "wav", "ogg", "oga", "opus", "wma", "ape", "mka",
                "jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "ico", "avif",
              ],
            },
            { name: "所有文件", extensions: ["*"] },
          ],
        },
      });
      if (!path || Array.isArray(path)) return;
      await openPathOrFolder(path);
    } catch (e) {
      toast(String(e), "err");
    }
  }

  /* ---------------------------------------------------------------------- */
  /* progress events                                                        */
  /* ---------------------------------------------------------------------- */

  function bindProgress() {
    listen("media-progress", (ev) => {
      if (state.busyKind !== "media") return;
      const { percent, label } = ev.payload || {};
      setBusy(true, label || "正在处理…", percent || 0);
    });
    listen("task-progress", (ev) => {
      const { percent, label } = ev.payload || {};
      modalProgress(true, percent || 0, label || "");
    });
  }

  /* ---------------------------------------------------------------------- */
  /* init                                                                   */
  /* ---------------------------------------------------------------------- */

  async function init() {
    detectPlatform();
    loadSettings();
    window.hydrateIcons();
    applyTheme();
    setFit(settings.fit);
    setSidebar(settings.sidebarOpen);

    video.volume = settings.volume;
    video.muted = settings.muted;
    settings.speed = Math.min(3, Math.max(0.5, Math.round(settings.speed * 2) / 2));
    video.playbackRate = settings.speed;
    $("volume").value = String(Math.round(settings.volume * 100));
    $("speed").value = String(settings.speed);
    updateVolumeIcon();

    // settings checkboxes
    $("set-autoplay").checked = settings.autoplay;
    $("set-autonext").checked = settings.autonext;
    $("set-resume").checked = settings.resume;

    // loop icon
    if (settings.loop !== "off") {
      $("btn-loop").dataset.icon = settings.loop === "one" ? "repeat1" : "repeat";
      delete $("btn-loop").dataset.iconDone;
      $("btn-loop").classList.add("active");
      window.hydrateIcons();
    }

    bindUI();
    bindShortcuts();
    bindDragDrop();
    bindProgress();
    renderInfo();
    renderList();
    updatePlayIcon();
    showEmpty(true);
    await applyWindowSettings();
    refreshWindowState();
    refreshToolStatus();

    // restore the last folder
    try {
      const last = localStorage.getItem(LS_DIR);
      if (last) {
        const files = await scanFolder(last, { keepCurrent: false });
        if (files.length) {
          showEmpty(true);
        }
      }
    } catch (_) {}
  }

  init();
})();
