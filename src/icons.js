/* Inline SVG icon set (Lucide-style, 24x24 grid). */
(() => {
  const S = (body) =>
    `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
  const F = (body) =>
    `<svg viewBox="0 0 24 24" fill="currentColor" stroke="none" aria-hidden="true">${body}</svg>`;

  const ICONS = {
    panelLeft: S('<rect x="3" y="3" width="18" height="18" rx="2.5"/><path d="M9.5 3v18"/>'),
    pin: S(
      '<path d="M12 17v5"/><path d="M5 17h14v-1.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V6h1a2 2 0 0 0 0-4H8a2 2 0 0 0 0 4h1v4.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24Z"/>'
    ),
    theme: S(
      '<path d="M12 8a2.83 2.83 0 0 0 4 4 4 4 0 1 1-4-4"/><path d="M12 2v2"/><path d="M12 20v2"/><path d="m4.9 4.9 1.4 1.4"/><path d="m17.7 17.7 1.4 1.4"/><path d="M2 12h2"/><path d="M20 12h2"/><path d="m6.3 17.7-1.4 1.4"/><path d="m19.1 4.9-1.4 1.4"/>'
    ),
    sun: S('<circle cx="12" cy="12" r="4"/><path d="M12 2v2"/><path d="M12 20v2"/><path d="m4.9 4.9 1.4 1.4"/><path d="m17.7 17.7 1.4 1.4"/><path d="M2 12h2"/><path d="M20 12h2"/><path d="m6.3 17.7-1.4 1.4"/><path d="m19.1 4.9-1.4 1.4"/>'),
    moon: S('<path d="M20 14.5A8.5 8.5 0 0 1 9.5 4a7 7 0 1 0 10.5 10.5"/>'),
    minimize: S('<path d="M5 12h14"/>'),
    maximize: S('<rect x="5" y="5" width="14" height="14" rx="2.5"/>'),
    restore: S('<rect x="7" y="3" width="14" height="14" rx="2.5"/><path d="M17 17v2.5A1.5 1.5 0 0 1 15.5 21H5A1.5 1.5 0 0 1 3.5 19.5V9A1.5 1.5 0 0 1 5 7.5h2.5"/>'),
    close: S('<path d="M18 6 6 18"/><path d="m6 6 12 12"/>'),
    folder: S('<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>'),
    folderOpen: S('<path d="m4 20 2.5-8h15L19 20z"/><path d="M4 20a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h4l2 2h7a2 2 0 0 1 2 2v4"/>'),
    film: S('<rect x="3" y="4" width="18" height="16" rx="2.5"/><path d="M7.5 4v16"/><path d="M16.5 4v16"/><path d="M3 9.5h4.5"/><path d="M3 14.5h4.5"/><path d="M16.5 9.5H21"/><path d="M16.5 14.5H21"/>'),
    refresh: S('<path d="M21 12a9 9 0 1 1-2.6-6.4L21 8"/><path d="M21 3v5h-5"/>'),
    external: S('<path d="M15 3h6v6"/><path d="M10.5 13.5 21 3"/><path d="M19 13.5V19a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h5.5"/>'),
    search: S('<circle cx="11" cy="11" r="7"/><path d="m20.5 20.5-4-4"/>'),
    play: F('<path d="M7.5 4.6v14.8a1 1 0 0 0 1.53.85l11.2-7.4a1 1 0 0 0 0-1.7L9.03 3.75A1 1 0 0 0 7.5 4.6"/>'),
    pause: F('<rect x="6.5" y="4.5" width="4" height="15" rx="1.3"/><rect x="13.5" y="4.5" width="4" height="15" rx="1.3"/>'),
    prev: F('<path d="M18.5 4.9v14.2a1 1 0 0 1-1.55.83L8 13.83v5.67a1 1 0 0 1-2 0V4.5a1 1 0 0 1 2 0v5.67l8.95-6.1a1 1 0 0 1 1.55.83"/>'),
    next: F('<path d="M5.5 4.9v14.2a1 1 0 0 0 1.55.83L16 13.83v5.67a1 1 0 0 0 2 0V4.5a1 1 0 0 0-2 0v5.67L7.05 4.07a1 1 0 0 0-1.55.83"/>'),
    stop: F('<rect x="6" y="6" width="12" height="12" rx="2"/>'),
    volume: S('<path d="M11 5 6.5 8.8H3.4v6.4h3.1L11 19z"/><path d="M15.4 8.9a4.6 4.6 0 0 1 0 6.2"/><path d="M18.3 6a8.6 8.6 0 0 1 0 12"/>'),
    volume1: S('<path d="M11 5 6.5 8.8H3.4v6.4h3.1L11 19z"/><path d="M15.4 8.9a4.6 4.6 0 0 1 0 6.2"/>'),
    volumeX: S('<path d="M11 5 6.5 8.8H3.4v6.4h3.1L11 19z"/><path d="m16 9.5 5 5"/><path d="m21 9.5-5 5"/>'),
    info: S('<circle cx="12" cy="12" r="9"/><path d="M12 16.5v-5"/><path d="M12 8.2h.01"/>'),
    repeat: S('<path d="m17 2.5 3.5 3.5L17 9.5"/><path d="M3.5 11.5V10a4 4 0 0 1 4-4h13"/><path d="m7 21.5-3.5-3.5L7 14.5"/><path d="M20.5 12.5V14a4 4 0 0 1-4 4h-13"/>'),
    repeat1: S('<path d="m17 2.5 3.5 3.5L17 9.5"/><path d="M3.5 11.5V10a4 4 0 0 1 4-4h13"/><path d="m7 21.5-3.5-3.5L7 14.5"/><path d="M20.5 12.5V14a4 4 0 0 1-4 4h-13"/><path d="M11.5 10.8h1.4v3.4"/>'),
    camera: S('<path d="M14.6 4h-5.2L7.8 6.2H4.3A2.3 2.3 0 0 0 2 8.5v9.2A2.3 2.3 0 0 0 4.3 20h15.4a2.3 2.3 0 0 0 2.3-2.3V8.5A2.3 2.3 0 0 0 19.7 6.2h-3.5z"/><circle cx="12" cy="13" r="3.6"/>'),
    sliders: S('<path d="M4 21v-6.5"/><path d="M4 10.5V3"/><path d="M12 21v-9"/><path d="M12 8V3"/><path d="M20 21v-4"/><path d="M20 13V3"/><path d="M1.6 14.5h4.8"/><path d="M9.6 8h4.8"/><path d="M17.6 17h4.8"/>'),
    toolbox: S(
      '<rect x="2.5" y="8.5" width="19" height="11.5" rx="2.4"/><path d="M9 8.5V6.7a2.2 2.2 0 0 1 2.2-2.2h1.6A2.2 2.2 0 0 1 15 6.7v1.8"/><path d="M2.5 13.2h5.2"/><path d="M16.3 13.2h5.2"/><path d="M10.4 13.2v2.2a1 1 0 0 0 1 1h1.2a1 1 0 0 0 1-1v-2.2"/>'
    ),
    gif: S(
      '<rect x="2.5" y="4.5" width="19" height="15" rx="2.5"/><path d="M10.5 10.2a2 2 0 0 0-3.4 1.4v1a2 2 0 0 0 3.4 1.4"/><path d="M13 10v4"/><path d="M16.5 14v-4h2.2"/><path d="M16.5 12.2h1.8"/>'
    ),
    expand: S('<path d="M8.5 3H5.5A2.5 2.5 0 0 0 3 5.5v3"/><path d="M15.5 3h3A2.5 2.5 0 0 1 21 5.5v3"/><path d="M15.5 21h3a2.5 2.5 0 0 0 2.5-2.5v-3"/><path d="M8.5 21h-3A2.5 2.5 0 0 1 3 18.5v-3"/>'),
    compress: S('<path d="M8.5 3v3a2.5 2.5 0 0 1-2.5 2.5H3"/><path d="M15.5 3v3a2.5 2.5 0 0 0 2.5 2.5h3"/><path d="M15.5 21v-3a2.5 2.5 0 0 1 2.5-2.5h3"/><path d="M8.5 21v-3A2.5 2.5 0 0 0 6 15.5H3"/>'),
    music: S('<path d="M9 18.5V5.2l11-1.9v13.2"/><circle cx="6.2" cy="18.5" r="2.8"/><circle cx="17.2" cy="16.5" r="2.8"/>'),
    image: S('<rect x="3" y="3" width="18" height="18" rx="2.5"/><circle cx="8.8" cy="8.8" r="1.9"/><path d="m21 15.5-4.6-4.6L5 21"/>'),
    settings: S('<path d="M20 7h-8.5"/><path d="M14.5 17H4"/><circle cx="8.5" cy="7" r="2.5"/><circle cx="17" cy="17" r="2.5"/>'),
    copy: S('<rect x="9" y="9" width="12" height="12" rx="2.5"/><path d="M4.5 15H4a1.5 1.5 0 0 1-1.5-1.5V4A1.5 1.5 0 0 1 4 2.5h9.5A1.5 1.5 0 0 1 15 4v.5"/>'),
    download: S('<path d="M12 3v12"/><path d="m7.2 10.2 4.8 4.8 4.8-4.8"/><path d="M4.5 21h15"/>'),
    logo: F('<rect x="2" y="2" width="20" height="20" rx="5.5"/><path d="M9.8 7.6v8.8a.8.8 0 0 0 1.22.68l6.8-4.4a.8.8 0 0 0 0-1.36l-6.8-4.4A.8.8 0 0 0 9.8 7.6" fill="#fff"/>'),
    clock: S('<circle cx="12" cy="12" r="9"/><path d="M12 7.2V12l3.2 1.9"/>'),
    crop: S('<path d="M6 2.5v13.5a2 2 0 0 0 2 2h13.5"/><path d="M18 21.5V8a2 2 0 0 0-2-2H2.5"/>'),
    reset: S('<path d="M3 12a9 9 0 1 0 3-6.7"/><path d="M3 4v5h5"/>'),
    check: S('<path d="m5 13 4.5 4.5L19.5 7"/>'),
    alert: S('<path d="M12 3.5 21.5 20H2.5z"/><path d="M12 10v4"/><path d="M12 17.2h.01"/>'),
  };

  window.ICONS = ICONS;

  window.hydrateIcons = function hydrateIcons(root = document) {
    root.querySelectorAll("[data-icon]").forEach((el) => {
      const name = el.dataset.icon;
      const svg = ICONS[name];
      if (!svg || el.dataset.iconDone === name) return;
      const label = el.dataset.label ? `<span class="ico-label">${el.dataset.label}</span>` : "";
      el.innerHTML = svg + label;
      el.dataset.iconDone = name;
    });
  };
})();
