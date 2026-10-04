/* ==========================================================================
   iPlayer — live transcoding (play a re-encode while it happens)

   A file the webview cannot decode has to be re-encoded; instead of waiting for
   all of it, iPlayer starts playing the first seconds and keeps feeding the rest
   into a MediaSource buffer:

     ffmpeg (fragmented MP4 on a pipe)
        └─ pull 256 KiB at a time  →  MediaSource  →  <video>

   The pulls are demand driven: the backend only reads the pipe when the media
   buffer runs low, so the encoder is paced by playback and no temporary file is
   ever written. Seeking restarts the encoder at the new position and drops its
   output into the same buffer at the right place on the timeline
   (`timestampOffset`), which keeps a scrub responsive on a long film.
   ========================================================================== */
(() => {
  const core = window.__TAURI__ && window.__TAURI__.core;
  if (!core) return;
  const invoke = core.invoke;

  const PULL = 262144; // bytes requested per backend pull
  const AHEAD = 30; // seconds of media to keep buffered ahead of the playhead
  const BEHIND = 30; // seconds of media to keep behind it
  const MAX_BUFFERED = 180; // trim once this many seconds are buffered
  const RESTART_GAP = 12; // seeking further than this past the buffer → restart
  const IDLE = 200; // poll while the buffer is comfortably ahead
  const PRIME = 0.35; // media needed before we let the player start
  const OPEN_TIMEOUT = 25000;

  let video = null;
  let notify = null;

  let ms = null;
  let sb = null;
  let url = null;
  let id = null;
  let path = "";
  let gen = 0;
  let offset = 0;
  let duration = 0;
  let pending = new Uint8Array(0);
  let eof = false;
  let failure = "";
  let warned = false;
  let restarting = false;
  let restartTimer = null;
  let restartTarget = 0;

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  function coded(code, message) {
    const e = new Error(message);
    e.code = code;
    return e;
  }

  /* ---------------------------------------------------------------------- */
  /* setup                                                                  */
  /* ---------------------------------------------------------------------- */

  const supported = () => typeof window.MediaSource !== "undefined";

  function mount(el, opts = {}) {
    video = el;
    notify = opts.notify || null;
  }

  const active = () => !!sb;

  function warn(message) {
    if (notify && message && !warned) {
      warned = true;
      notify(message);
    }
  }

  /* ---------------------------------------------------------------------- */
  /* fragmented-MP4 helpers                                                 */
  /* ---------------------------------------------------------------------- */

  function concat(a, b) {
    if (!a.length) return b;
    if (!b.length) return a;
    const out = new Uint8Array(a.length + b.length);
    out.set(a, 0);
    out.set(b, a.length);
    return out;
  }

  /// Walk the top-level boxes. Returns the end offset of the last *complete*
  /// box: a fragment must never be handed to `appendBuffer` half-written.
  function completeEnd(bytes) {
    let i = 0;
    while (i + 8 <= bytes.length) {
      let size = ((bytes[i] << 24) | (bytes[i + 1] << 16) | (bytes[i + 2] << 8) | bytes[i + 3]) >>> 0;
      let head = 8;
      if (size === 1) {
        if (i + 16 > bytes.length) break;
        size = ((bytes[i + 8] << 24) | (bytes[i + 9] << 16) | (bytes[i + 10] << 8) | bytes[i + 11]) * 4294967296 +
          (((bytes[i + 12] << 24) | (bytes[i + 13] << 16) | (bytes[i + 14] << 8) | bytes[i + 15]) >>> 0);
        head = 16;
      } else if (size === 0) {
        size = bytes.length - i;
      }
      if (size < head || i + size > bytes.length) break;
      i += size;
    }
    return i;
  }

  /// Offset just past the first complete box of `type` (0 when not seen yet).
  function endAfterBox(bytes, type) {
    let i = 0;
    while (i + 8 <= bytes.length) {
      let size = ((bytes[i] << 24) | (bytes[i + 1] << 16) | (bytes[i + 2] << 8) | bytes[i + 3]) >>> 0;
      let head = 8;
      if (size === 1) {
        if (i + 16 > bytes.length) return 0;
        size = ((bytes[i + 8] << 24) | (bytes[i + 9] << 16) | (bytes[i + 10] << 8) | bytes[i + 11]) * 4294967296 +
          (((bytes[i + 12] << 24) | (bytes[i + 13] << 16) | (bytes[i + 14] << 8) | bytes[i + 15]) >>> 0);
        head = 16;
      } else if (size === 0) {
        size = bytes.length - i;
      }
      if (size < head || i + size > bytes.length) return 0;
      const name = String.fromCharCode(bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]);
      if (name === type) return i + size;
      i += size;
    }
    return 0;
  }

  function ascii(bytes, start, end) {
    let s = "";
    for (let i = start; i < end; i += 4096) {
      s += String.fromCharCode.apply(null, bytes.subarray(i, Math.min(i + 4096, end)));
    }
    return s;
  }

  const hex2 = (n) => n.toString(16).padStart(2, "0");

  /// Build the `MediaSource` MIME type from the encoder's own initialisation
  /// segment — no guessing at profiles: `avcC` carries them verbatim.
  function initMime(init) {
    const text = ascii(init, 0, init.length);
    const avcC = text.indexOf("avcC");
    if (avcC < 0 || avcC + 8 > init.length) return "";
    const codec = `avc1.${hex2(init[avcC + 5])}${hex2(init[avcC + 6])}${hex2(init[avcC + 7])}`;
    const codecs = [codec];
    if (text.indexOf("mp4a") >= 0) codecs.push("mp4a.40.2"); // we always encode AAC-LC
    return `video/mp4; codecs="${codecs.join(",")}"`;
  }

  /* ---------------------------------------------------------------------- */
  /* source buffer plumbing                                                 */
  /* ---------------------------------------------------------------------- */

  function waitUpdateEnd() {
    if (!sb || !sb.updating) return Promise.resolve();
    return new Promise((resolve) => {
      const done = () => {
        sb.removeEventListener("updateend", done);
        resolve();
      };
      sb.addEventListener("updateend", done);
    });
  }

  async function appendBytes(bytes) {
    if (!sb || !bytes.length) return;
    await waitUpdateEnd();
    if (!sb) return;
    try {
      sb.appendBuffer(bytes);
    } catch (e) {
      if (e && e.name === "QuotaExceededError") {
        await dropOld();
        try {
          sb.appendBuffer(bytes);
        } catch (e2) {
          failure = `缓冲区写入失败：${e2 && e2.message ? e2.message : e2}`;
          return;
        }
      } else {
        failure = `缓冲区写入失败：${e && e.message ? e.message : e}`;
        return;
      }
    }
    await waitUpdateEnd();
  }

  function bufferEnd() {
    if (!sb || !sb.buffered || !sb.buffered.length) return 0;
    return sb.buffered.end(sb.buffered.length - 1);
  }

  /// End of the range the playhead sits in — what "how far ahead am I buffered"
  /// really means once a seek has left several ranges behind. When the playhead
  /// falls in a gap it returns a value *behind* it, which keeps the puller
  /// working instead of idling on some far-away future range.
  function bufferAhead() {
    if (!sb || !sb.buffered || !sb.buffered.length) return 0;
    const b = sb.buffered;
    const t = video ? video.currentTime : 0;
    for (let i = 0; i < b.length; i++) {
      if (t >= b.start(i) - 0.25 && t <= b.end(i) + 0.25) return b.end(i);
    }
    let before = 0;
    for (let i = 0; i < b.length; i++) {
      if (b.end(i) <= t) before = Math.max(before, b.end(i));
    }
    return before;
  }

  /// Should a seek to `t` re-encode from there, or will the running encoder
  /// reach it soon enough? Only meaningful for a position that is not buffered.
  function shouldRestart(t) {
    if (!sb || !sb.buffered || isBuffered(t)) return false;
    const b = sb.buffered;
    let nearestEnd = 0;
    for (let i = 0; i < b.length; i++) {
      if (b.end(i) <= t + 0.5) nearestEnd = Math.max(nearestEnd, b.end(i));
    }
    return t - nearestEnd > RESTART_GAP;
  }

  function isBuffered(t) {
    if (!sb || !sb.buffered) return false;
    const b = sb.buffered;
    for (let i = 0; i < b.length; i++) {
      if (t >= b.start(i) && t <= b.end(i) - 0.25) return true;
    }
    return false;
  }

  function bufferedTotal() {
    if (!sb || !sb.buffered) return 0;
    const b = sb.buffered;
    let total = 0;
    for (let i = 0; i < b.length; i++) total += b.end(i) - b.start(i);
    return total;
  }

  /// Release memory: drop everything well behind the playhead.
  async function trimIfNeeded(force) {
    if (!sb || sb.updating) return;
    if (!force && bufferedTotal() < MAX_BUFFERED) return;
    const t = video ? video.currentTime : 0;
    const b = sb.buffered;
    const ranges = [];
    for (let i = 0; i < b.length; i++) {
      if (b.end(i) < t - BEHIND) ranges.push([b.start(i), b.end(i)]);
    }
    if (!ranges.length) return;
    await waitUpdateEnd();
    for (const [s, e] of ranges) {
      if (!sb || sb.updating) break;
      try {
        sb.remove(s, e);
      } catch (_) {}
      await waitUpdateEnd();
    }
  }

  const dropOld = () => trimIfNeeded(true);

  /* ---------------------------------------------------------------------- */
  /* pulling                                                                */
  /* ---------------------------------------------------------------------- */

  async function statusText() {
    if (!id) return failure;
    try {
      const st = await invoke("stream_status", { id });
      if (st && st.error) failure = st.error;
      else if (st && st.failed) failure = failure || "ffmpeg 处理失败";
    } catch (_) {}
    return failure;
  }

  /// Pull once and append to `pending`. Returns the byte count (0 = the
  /// encoder is finished).
  async function pullInto(my) {
    let raw;
    try {
      raw = await invoke("stream_read", { id, max: PULL });
    } catch (e) {
      if (my !== gen) return 0; // the session was replaced mid-flight
      eof = true;
      failure = failure || String(e);
      return 0;
    }
    if (my !== gen) return 0;
    const bytes = new Uint8Array(raw);
    if (!bytes.length) {
      eof = true;
      await statusText();
      return 0;
    }
    pending = concat(pending, bytes);
    return bytes.length;
  }

  /// Hand every complete box to the SourceBuffer.
  async function flush(my) {
    while (my === gen && sb) {
      const end = completeEnd(pending);
      if (!end) return;
      const chunk = pending.slice(0, end);
      pending = pending.slice(end);
      await appendBytes(chunk);
      if (my !== gen) return;
      await trimIfNeeded(false);
    }
  }

  async function pump(my) {
    while (my === gen && sb) {
      await flush(my);
      if (my !== gen) return;
      if (failure) {
        warn(failure);
        return;
      }
      if (bufferAhead() - video.currentTime >= AHEAD) {
        await sleep(IDLE);
        continue;
      }
      const got = await pullInto(my);
      if (my !== gen) return;
      if (!got) {
        await finish(my);
        return;
      }
    }
  }

  async function finish(my) {
    if (my !== gen) return;
    await flush(my);
    const end = bufferEnd();
    if (duration && end > 0 && end < duration - 2.5) {
      warn(failure || "转码在结束前中断，视频可能不完整");
    }
    try {
      if (ms && ms.readyState === "open" && sb && !sb.updating) {
        // Pin the length to what actually got buffered, so the player knows it
        // has reached the end and fires `ended` (autoplay-next, repeat…).
        if (isFinite(ms.duration) && end > 0 && end < ms.duration) ms.duration = end;
        ms.endOfStream();
      }
    } catch (_) {}
  }

  async function readInit(my) {
    const deadline = Date.now() + OPEN_TIMEOUT;
    while (my === gen) {
      const end = endAfterBox(pending, "moov");
      if (end) {
        const init = pending.slice(0, end);
        pending = pending.slice(end);
        return init;
      }
      if (Date.now() > deadline) throw coded("TIMEOUT", "等待转码输出超时");
      if (!(await pullInto(my))) throw coded("NO_DATA", failure || "转码进程没有产出数据");
    }
    throw coded("ABORTED", "已取消");
  }

  function waitFor(pred, timeout, stopWhen) {
    return new Promise((resolve) => {
      const t0 = Date.now();
      const tick = () => {
        if (pred()) return resolve(true);
        if (stopWhen && stopWhen()) return resolve(false);
        if (Date.now() - t0 > timeout) return resolve(false);
        setTimeout(tick, 100);
      };
      tick();
    });
  }

  /* ---------------------------------------------------------------------- */
  /* public API                                                             */
  /* ---------------------------------------------------------------------- */

  /// Start streaming `path` from `start` seconds. Resolves with the blob URL the
  /// player should load, and only after enough media exists to actually play.
  async function begin(o) {
    stop();
    video = o.video || video;
    path = o.path;
    duration = o.duration || 0;
    gen++;
    const my = gen;
    offset = Math.max(0, o.start || 0);
    pending = new Uint8Array(0);
    eof = false;
    failure = "";
    warned = false;

    const res = await invoke("stream_start", { path, start: offset });
    if (my !== gen) {
      invoke("stream_stop", { id: res.id }).catch(() => {});
      throw coded("ABORTED", "已取消");
    }
    id = res.id;
    offset = res.start || 0;

    try {
      const init = await readInit(my);
      const mime = initMime(init);
      // Validate before touching the player: an unsupported source assigned to
      // the element would raise a bogus playback error on the way out.
      if (!mime || !supported() || !window.MediaSource.isTypeSupported(mime)) {
        throw coded("NO_MSE", "播放内核不支持该编码的边转码播放");
      }

      ms = new MediaSource();
      url = URL.createObjectURL(ms);
      // Attaching is what opens a MediaSource, so the element has to load it
      // before a SourceBuffer can be added.
      video.src = url;
      const opened = await waitFor(() => ms.readyState === "open", 10000);
      if (!opened || my !== gen) throw coded(opened ? "ABORTED" : "NO_MSE", "无法打开媒体缓冲区");

      sb = ms.addSourceBuffer(mime);
      sb.timestampOffset = offset;
      // Declare the real length up front: with an unknown duration the webview
      // only allows seeking inside the buffered range, which would make the
      // scrub bar useless. A second of slack keeps the last frames from being
      // rejected for running past the end; `finish` tightens it again.
      if (duration) ms.duration = duration + 1;
      await appendBytes(init);
      void pump(my);

      const ready = await waitFor(
        () => bufferEnd() >= PRIME,
        OPEN_TIMEOUT,
        () => (eof && !pending.length) || !!failure
      );
      if (!ready) throw coded("NO_DATA", failure || "转码没有产出可播放的数据");
      // Resuming, or opening at a position the user picked: the element is at 0
      // where there is nothing to play — move it onto the data we just buffered.
      if (offset > 0.05 && Math.abs(video.currentTime - offset) > 0.1) {
        try {
          video.currentTime = offset;
        } catch (_) {}
      }
      return url;
    } catch (e) {
      if (e && e.code === "ABORTED") throw e;
      const err = await statusText();
      stop();
      throw coded(e && e.code ? e.code : "FAILED", err || (e && e.message) || "边转码播放失败");
    }
  }

  /// Seeking outside the buffered range: re-encode from there instead of
  /// waiting for the running encoder to catch up. Returns false when a restart
  /// is already scheduled (scrubbing fires this on every pointer move).
  function requestRestart(t) {
    if (!sb || !path) return false;
    const fresh = !restartTimer;
    restartTarget = t;
    if (restartTimer) clearTimeout(restartTimer);
    restartTimer = setTimeout(() => {
      restartTimer = null;
      void doRestart(restartTarget);
    }, 300);
    return fresh;
  }

  async function doRestart(t) {
    if (!sb || !path || id === null) return;
    const target = Math.max(0, Math.min(t, duration ? duration - 1 : t));
    if (isBuffered(target)) return;

    gen++;
    const my = gen;
    restarting = true;
    let res;
    try {
      res = await invoke("stream_start", { path, start: target });
    } catch (e) {
      restarting = false;
      warn(`跳转失败：${e}`);
      return;
    }
    const old = id;
    if (old) invoke("stream_stop", { id: old }).catch(() => {});
    if (my !== gen) {
      invoke("stream_stop", { id: res.id }).catch(() => {});
      restarting = false;
      return;
    }
    id = res.id;
    offset = res.start || target;
    pending = new Uint8Array(0);
    eof = false;
    failure = "";
    warned = false;

    try {
      const init = await readInit(my);
      if (my !== gen) {
        restarting = false;
        return;
      }
      await waitUpdateEnd();
      // Place this run at its real position on the timeline; the gap before it
      // is simply unbuffered (the browser plays from where we seeked).
      if (sb) sb.timestampOffset = offset;
      await appendBytes(init);
      // The element can be left sitting where the browser clamped its earlier
      // seek (it was outside the seekable range at the time); now that the
      // target is real, put it there.
      if (Math.abs(video.currentTime - offset) > 0.5) {
        try {
          video.currentTime = offset;
        } catch (_) {}
      }
    } catch (e) {
      restarting = false;
      failure = (e && e.message) || String(e);
      return;
    }
    restarting = false;
    void pump(my);
  }

  function stop() {
    gen++;
    if (restartTimer) {
      clearTimeout(restartTimer);
      restartTimer = null;
    }
    // A restart belonging to the session we are dropping must not keep the next
    // file's seeks on hold.
    restarting = false;
    const old = id;
    id = null;
    if (old) invoke("stream_stop", { id: old }).catch(() => {});
    if (video && url && video.src === url) {
      try {
        video.removeAttribute("src");
        video.load();
      } catch (_) {}
    }
    if (sb) {
      try {
        sb.abort();
      } catch (_) {}
    }
    sb = null;
    ms = null;
    if (url) {
      URL.revokeObjectURL(url);
      url = null;
    }
    pending = new Uint8Array(0);
    eof = false;
    failure = "";
    warned = false;
    path = "";
    offset = 0;
    duration = 0;
  }

  window.Streamer = {
    supported,
    mount,
    active,
    begin,
    stop,
    requestRestart,
    isBuffered,
    shouldRestart,
    bufferedEnd: bufferEnd,
    repositioning: () => restarting,
  };
})();
