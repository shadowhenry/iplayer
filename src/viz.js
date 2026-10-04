/* ==========================================================================
   iPlayer — audio visualiser
   --------------------------------------------------------------------------
   Canvas equaliser shown instead of the empty (black) stage while an audio
   file is loaded: spectrum bars anchored at the bottom, plus note glyphs of
   varying size drifting upwards and fading in / out behind them.

   The spectrum comes from a Web Audio AnalyserNode when one is attached.
   Some webview / protocol combinations hand out decoded samples as silence
   (or refuse to expose them at all); in that case we fall back to a
   procedural spectrum with the same visual character, so the equaliser is
   never dead. `mode` reports which source is in use.
   ========================================================================== */

(() => {
  "use strict";

  const INK = "255, 255, 255";
  const FONT =
    '-apple-system, BlinkMacSystemFont, "Helvetica Neue", "Segoe UI", Arial, sans-serif';
  const GLYPHS = ["\u266A", "\u266B", "\u266C", "\u2669"]; // ♪ ♫ ♬ ♩

  let host = null;
  let canvas = null;
  let ctx = null;
  let W = 0;
  let H = 0;

  let raf = 0;
  let running = false;
  let playing = false;
  let audible = true;

  let analyser = null;
  let freqData = null;
  let sawSignal = false;
  let fallback = false;
  let frames = 0;

  let levels = [];
  let targets = [];
  let peaks = [];
  let notes = [];
  let last = 0;
  let clock = 0;
  let spawnAcc = 0;

  /* ---------------------------------------------------------------------- */
  /* canvas plumbing                                                        */
  /* ---------------------------------------------------------------------- */

  function mount(el) {
    host = el;
    canvas = el.querySelector("canvas");
    if (!canvas) return;
    ctx = canvas.getContext("2d");
    resize();
    if (window.ResizeObserver) {
      new ResizeObserver(resize).observe(host);
    } else {
      window.addEventListener("resize", resize);
    }
  }

  function resize() {
    if (!host || !canvas || !ctx) return;
    const w = Math.max(1, host.clientWidth);
    const h = Math.max(1, host.clientHeight);
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    W = w;
    H = h;
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
    canvas.style.width = `${w}px`;
    canvas.style.height = `${h}px`;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    // one bar per ~15px, kept in a sane range
    const n = Math.max(20, Math.min(72, Math.round(w / 15)));
    if (levels.length !== n) {
      levels = new Array(n).fill(0);
      targets = new Array(n).fill(0);
      peaks = new Array(n).fill(0);
    }
  }

  /* ---------------------------------------------------------------------- */
  /* state                                                                  */
  /* ---------------------------------------------------------------------- */

  function attachAnalyser(node, data) {
    analyser = node || null;
    freqData = data || null;
    sawSignal = false;
    fallback = false;
    frames = 0;
    reset();
  }

  function reset() {
    levels.fill(0);
    targets.fill(0);
    peaks.fill(0);
    notes = [];
    clock = 0;
    spawnAcc = 0;
    last = 0;
    if (H) seedNotes();
  }

  function setPlaying(v) {
    playing = !!v;
    if (playing) {
      last = 0; // restart the frame clock so dt stays sane
    }
  }

  function setAudible(v) {
    audible = !!v;
  }

  function show() {
    if (!host) return;
    host.classList.remove("hidden");
    resize();
    reset();
    if (!running) {
      running = true;
      raf = requestAnimationFrame(frame);
    }
  }

  function hide() {
    if (host) host.classList.add("hidden");
    running = false;
    if (raf) cancelAnimationFrame(raf);
    raf = 0;
    if (ctx && W) ctx.clearRect(0, 0, W, H);
  }

  /* ---------------------------------------------------------------------- */
  /* spectrum                                                               */
  /* ---------------------------------------------------------------------- */

  function readLive() {
    analyser.getByteFrequencyData(freqData);
    const bins = freqData.length;
    const n = levels.length;
    // log-spaced bands across the part of the spectrum music actually uses
    const lo = 1;
    const hi = Math.max(16, Math.min(bins - 1, Math.round(bins * 0.4)));
    const ratio = Math.pow(hi / lo, 1 / n);
    let energy = 0;

    for (let i = 0; i < n; i++) {
      const a = Math.floor(lo * Math.pow(ratio, i));
      const b = Math.max(a + 1, Math.floor(lo * Math.pow(ratio, i + 1)));
      let acc = 0;
      let mx = 0;
      let cnt = 0;
      for (let k = a; k < b && k < bins; k++) {
        const s = freqData[k];
        acc += s;
        if (s > mx) mx = s;
        cnt++;
      }
      energy += acc;
      // highs get smeared across many near-silent bins, so blend the band
      // peak in and tilt the curve up towards the right
      const v = cnt ? (acc / cnt) * 0.42 + mx * 0.58 : 0;
      const tilt = 0.78 + 0.75 * (i / Math.max(1, n - 1));
      targets[i] = Math.min(1, Math.pow((v / 255) * tilt, 0.72) * 1.6);
    }
    return energy;
  }

  function readSynthetic(t) {
    const n = levels.length;
    for (let i = 0; i < n; i++) {
      const pos = i / Math.max(1, n - 1);
      const w1 = 0.5 + 0.5 * Math.sin(t * 1.9 + i * 0.55);
      const w2 = 0.5 + 0.5 * Math.sin(t * 3.7 + i * 1.7);
      const beat = 0.5 + 0.5 * Math.sin(t * 5.3);
      const swell = 0.35 + 0.65 * Math.pow(0.5 + 0.5 * Math.sin(t * 0.31 + i * 0.07), 1.6);
      const tilt = 1.18 - 0.5 * pos;
      targets[i] = Math.min(
        1,
        swell * (0.42 * w1 + 0.34 * w2 + 0.24 * beat) * tilt * 1.45
      );
    }
  }

  function step(dt) {
    const n = levels.length;
    if (!playing) {
      // settle back down while paused
      for (let i = 0; i < n; i++) {
        targets[i] = 0;
        levels[i] += (0 - levels[i]) * Math.min(1, dt * 3.2);
        peaks[i] = Math.max(levels[i], peaks[i] - dt * 0.9);
      }
      return;
    }

    if (analyser && freqData) {
      const energy = readLive();
      if (energy > 0) sawSignal = true;
    }

    // No decoded audio to read? After a moment of playback, switch to the
    // procedural spectrum so the stage is never a dead rectangle.
    if (!sawSignal) {
      frames++;
      if (audible && frames > 60) fallback = true;
    } else {
      fallback = false;
    }

    if (fallback || !analyser || !freqData) {
      if (audible) readSynthetic(clock);
      else for (let i = 0; i < n; i++) targets[i] = 0;
    }

    for (let i = 0; i < n; i++) {
      const t = targets[i];
      // fast attack, slow release — reads as "music" rather than a strobe
      const k = t > levels[i] ? Math.min(1, dt * 14) : Math.min(1, dt * 3.6);
      levels[i] += (t - levels[i]) * k;
      peaks[i] = Math.max(levels[i], peaks[i] - dt * 0.55);
    }
  }

  /* ---------------------------------------------------------------------- */
  /* notes                                                                  */
  /* ---------------------------------------------------------------------- */

  function newNote(seeded) {
    const size = Math.max(15, Math.min(H * 0.22, H * (0.05 + Math.random() * 0.17)));
    return {
      glyph: GLYPHS[(Math.random() * GLYPHS.length) | 0],
      x: W * (0.05 + Math.random() * 0.87),
      y: seeded ? H * (0.1 + Math.random() * 0.9) : H + size * 0.6,
      size,
      alpha: 0.1 + Math.random() * 0.2,
      vy: -(H * (0.035 + Math.random() * 0.08)),
      rot: (Math.random() - 0.5) * 0.5,
      wob: 5 + Math.random() * 22,
      wobSpeed: 0.4 + Math.random() * 0.9,
      phase: Math.random() * Math.PI * 2,
    };
  }

  function seedNotes() {
    if (!W || !H) return;
    const n = Math.max(6, Math.round(W / 150));
    for (let i = 0; i < n; i++) notes.push(newNote(true));
  }

  function stepNotes(dt) {
    const max = Math.max(8, Math.round(W / 95));

    if (playing) {
      spawnAcc += dt * (1.1 + Math.random() * 0.9);
      while (spawnAcc >= 1) {
        spawnAcc -= 1;
        if (notes.length >= max) break;
        notes.push(newNote(false));
      }
    } else {
      spawnAcc = 0;
    }

    for (let i = notes.length - 1; i >= 0; i--) {
      const nt = notes[i];
      nt.y += nt.vy * dt;
      nt.phase += nt.wobSpeed * dt;
      if (nt.y < -nt.size * 1.6 || nt.y > H + Math.max(H * 0.5, nt.size * 3)) {
        notes.splice(i, 1);
      }
    }
  }

  /* ---------------------------------------------------------------------- */
  /* drawing                                                                */
  /* ---------------------------------------------------------------------- */

  function roundRect(x, y, w, h, r) {
    const rr = Math.min(r, w / 2, h / 2);
    ctx.beginPath();
    ctx.moveTo(x + rr, y);
    ctx.arcTo(x + w, y, x + w, y + h, rr);
    ctx.arcTo(x + w, y + h, x, y + h, rr);
    ctx.arcTo(x, y + h, x, y, rr);
    ctx.arcTo(x, y, x + w, y, rr);
    ctx.closePath();
  }

  function drawNotes() {
    for (const nt of notes) {
      // 0 while just off the bottom, 1 once it has climbed past the top
      const prog = (H + nt.size - nt.y) / (H + nt.size * 3);
      if (prog <= 0 || prog >= 1) continue;
      const a = nt.alpha * Math.sin(Math.PI * prog);
      if (a <= 0.004) continue;

      const x = nt.x + Math.sin(nt.phase) * nt.wob;
      const g = ctx.createLinearGradient(0, nt.y - nt.size * 0.6, 0, nt.y + nt.size * 0.5);
      g.addColorStop(0, `rgba(${INK},${a.toFixed(3)})`);
      g.addColorStop(1, `rgba(${INK},0)`);

      ctx.save();
      ctx.translate(x, nt.y);
      ctx.rotate(nt.rot);
      ctx.font = `${nt.size.toFixed(1)}px ${FONT}`;
      ctx.textAlign = "center";
      ctx.textBaseline = "middle";
      ctx.fillStyle = g;
      ctx.fillText(nt.glyph, 0, 0);
      ctx.restore();
    }
  }

  function drawBars() {
    const n = levels.length;
    if (!n) return;

    const pad = Math.max(8, W * 0.025);
    const gap = Math.max(2, W * 0.0045);
    const bw = Math.max(2, (W - pad * 2 - gap * (n - 1)) / n);
    const baseY = H - Math.max(10, H * 0.07);
    const maxH = H * 0.34;
    const r = Math.min(bw / 2, 4);

    // one gradient reused by every bar (top of the tallest possible bar -> floor)
    const grad = ctx.createLinearGradient(0, baseY - maxH, 0, baseY);
    grad.addColorStop(0, `rgba(${INK},0.14)`);
    grad.addColorStop(0.45, `rgba(${INK},0.5)`);
    grad.addColorStop(1, `rgba(${INK},0.92)`);

    // soft halo pass first, then the bars themselves
    ctx.fillStyle = `rgba(${INK},0.07)`;
    for (let i = 0; i < n; i++) {
      const h = Math.max(bw * 0.55, levels[i] * maxH);
      roundRect(pad + i * (bw + gap) - 2, baseY - h, bw + 4, h + 2, r + 2);
      ctx.fill();
    }

    ctx.fillStyle = grad;
    for (let i = 0; i < n; i++) {
      const h = Math.max(bw * 0.55, levels[i] * maxH);
      roundRect(pad + i * (bw + gap), baseY - h, bw, h, r);
      ctx.fill();
    }

    // peak caps
    ctx.fillStyle = `rgba(${INK},0.5)`;
    for (let i = 0; i < n; i++) {
      const h = Math.max(bw * 0.55, peaks[i] * maxH);
      roundRect(pad + i * (bw + gap), baseY - h - 2.5, bw, 2.5, 1.25);
      ctx.fill();
    }
  }

  function frame(now) {
    if (!running) return;
    raf = requestAnimationFrame(frame);
    if (document.hidden || !ctx) return;

    const dt = last ? Math.min(0.05, (now - last) / 1000) : 0.016;
    last = now;
    clock += dt;

    step(dt);
    stepNotes(dt);

    ctx.clearRect(0, 0, W, H);
    drawNotes();
    drawBars();
  }

  window.Viz = {
    /** "idle" | "live" (real spectrum) | "synthetic" (procedural fallback) */
    get mode() {
      if (!running) return "idle";
      return analyser && !fallback ? "live" : "synthetic";
    },
    mount,
    attachAnalyser,
    setPlaying,
    setAudible,
    show,
    hide,
    __dbg: () => ({ n: levels.length, lv: levels.slice(0, 6).map(v => +v.toFixed(3)), tg: targets.slice(0, 6).map(v => +v.toFixed(3)), clock: +clock.toFixed(2), fallback, sawSignal, frames, playing, audible }),
  };
})();
