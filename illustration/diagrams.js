/* MagickVoice Sentinel — diagram behaviour.
   No framework, no build step: this is served straight off GitHub Pages.
   Every threshold, close code and byte count below is the one the source uses. */

'use strict';

const NS = 'http://www.w3.org/2000/svg';
const REDUCED = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

function svg(tag, attrs, text) {
  const el = document.createElementNS(NS, tag);
  for (const k in attrs) el.setAttribute(k, attrs[k]);
  if (text !== undefined) el.textContent = text;
  return el;
}

function clear(el) { while (el.firstChild) el.removeChild(el.firstChild); }

/* ------------------------------------------------------------------ chrome */

(function theme() {
  const btn = document.getElementById('theme');
  const root = document.documentElement;
  const stored = (() => { try { return localStorage.getItem('sentinel-theme'); } catch { return null; } })();
  if (stored) root.setAttribute('data-theme', stored);

  const label = () => {
    const dark = root.getAttribute('data-theme') === 'dark' ||
      (!root.getAttribute('data-theme') && window.matchMedia('(prefers-color-scheme: dark)').matches);
    btn.textContent = dark ? 'Light' : 'Dark';
  };
  label();

  btn.addEventListener('click', () => {
    const dark = root.getAttribute('data-theme') === 'dark' ||
      (!root.getAttribute('data-theme') && window.matchMedia('(prefers-color-scheme: dark)').matches);
    const next = dark ? 'light' : 'dark';
    root.setAttribute('data-theme', next);
    try { localStorage.setItem('sentinel-theme', next); } catch { /* private window */ }
    label();
  });
})();

(function scrollspy() {
  const links = [...document.querySelectorAll('#nav a')];
  const sections = links.map((a) => document.querySelector(a.getAttribute('href'))).filter(Boolean);
  if (!sections.length) return;
  const spy = new IntersectionObserver((entries) => {
    for (const e of entries) {
      if (!e.isIntersecting) continue;
      links.forEach((a) => a.classList.toggle('on', a.getAttribute('href') === '#' + e.target.id));
    }
  }, { rootMargin: '-10% 0px -70% 0px' });
  sections.forEach((s) => spy.observe(s));
})();

/* ------------------------------------------------- 01 · service map flow */

(function serviceMap() {
  const btn = document.querySelector('[data-flow-toggle]');
  if (!btn) return;
  const edges = [...document.querySelectorAll('[data-flow]')];
  const apply = (on) => {
    edges.forEach((e) => e.classList.toggle('flow-dash', on));
    btn.setAttribute('aria-pressed', String(on));
    btn.textContent = on ? 'Pause flow' : 'Resume flow';
  };
  apply(!REDUCED);
  btn.addEventListener('click', () => apply(btn.getAttribute('aria-pressed') !== 'true'));
})();

/* ------------------------------------------------------- 02 · build steps */

(function build() {
  const host = document.getElementById('build-steps');
  if (!host) return;
  const steps = [...host.querySelectorAll('.step')];
  const readout = document.querySelector('[data-build-readout]');
  const labels = ['cargo build', 'binaries produced', 'signtool · binaries', 'wix build',
    'tier gate compiled in', 'signtool · MSI, then deploy'];
  let at = -1;
  let timer = null;

  function render() {
    steps.forEach((s, i) => {
      s.classList.toggle('on', i === at);
      s.classList.toggle('done', i < at);
    });
    readout.textContent = at < 0 ? 'idle' : `${at + 1}/6 · ${labels[at]}`;
    if (at >= 0) steps[at].scrollIntoView({ block: 'nearest', behavior: REDUCED ? 'auto' : 'smooth' });
  }

  function stop() { clearInterval(timer); timer = null; }

  document.querySelector('[data-build="step"]').addEventListener('click', () => {
    stop();
    at = at >= steps.length - 1 ? -1 : at + 1;
    render();
  });
  document.querySelector('[data-build="reset"]').addEventListener('click', () => {
    stop(); at = -1; render();
  });
  document.querySelector('[data-build="play"]').addEventListener('click', () => {
    stop();
    at = -1; render();
    timer = setInterval(() => {
      at += 1;
      render();
      if (at >= steps.length - 1) stop();
    }, 1600);
  });

  render();
})();

/* -------------------------------------------------- 03 · supervisor ladder */

(function supervisor() {
  const ladder = document.getElementById('sup-ladder');
  if (!ladder) return;

  // sentinel-service/src/supervisor.rs: doubling from 1 s, capped at 60 s, and reset
  // by a run that stays healthy for HEALTHY_RUN_MS (120 s).
  const DELAYS = [1, 2, 4, 8, 16, 32, 60];
  const agent = document.getElementById('sup-agent');
  const agentTitle = document.getElementById('sup-agent-title');
  const agentSub = document.getElementById('sup-agent-sub');
  const readout = document.querySelector('[data-sup-readout]');
  const note = document.getElementById('sup-note');
  const ladderNote = document.getElementById('sup-ladder-note');
  const BASE_NOTE = note.textContent;

  let level = -1;      // index into DELAYS; -1 = no crashes on record
  let countdown = null;

  DELAYS.forEach((d, i) => {
    const y = 62 + i * 36;
    ladder.appendChild(svg('rect', {
      x: 820, y, width: 86, height: 26, rx: 13,
      fill: 'var(--surface-2)', stroke: 'var(--line)', 'data-pill': i,
    }));
    ladder.appendChild(svg('text', {
      x: 863, y: y + 17, 'text-anchor': 'middle',
      class: 'n-sub', 'data-pill-text': i,
    }, `${d} s`));
  });

  function paint() {
    DELAYS.forEach((_, i) => {
      const pill = ladder.querySelector(`[data-pill="${i}"]`);
      const on = i === level;
      pill.setAttribute('stroke', on ? 'var(--sev)' : 'var(--line)');
      pill.setAttribute('fill', on ? 'color-mix(in srgb, var(--sev) 16%, var(--surface-2))' : 'var(--surface-2)');
    });
  }

  function setAgent(state) {
    // Inline style, not the attribute: `.n-box` sets `stroke` in the stylesheet, and a
    // presentation attribute loses to any CSS rule that names the same property.
    if (state === 'running') {
      agent.style.stroke = 'var(--line)';
      agentTitle.textContent = 'SentinelAgent.exe';
      agentSub.textContent = 'capture · spool · uplink';
    } else if (state === 'dead') {
      agent.style.stroke = 'var(--sev)';
      agentTitle.textContent = 'SentinelAgent.exe — gone';
      agentSub.textContent = 'spool keeps unacked audio on disk';
    } else {
      agent.style.stroke = 'var(--tier-b)';
      agentTitle.textContent = 'SentinelAgent.exe — relaunching';
      agentSub.textContent = 'session mutex prevents a double start';
    }
  }

  function reset() {
    clearInterval(countdown); countdown = null;
    level = -1;
    setAgent('running');
    paint();
    readout.textContent = 'agent running';
    note.textContent = BASE_NOTE;
    ladderNote.textContent = 'a healthy 120 s run resets to 1 s';
  }

  document.querySelector('[data-sup="crash"]').addEventListener('click', () => {
    clearInterval(countdown);
    level = Math.min(level + 1, DELAYS.length - 1);
    paint();
    setAgent('dead');
    let left = DELAYS[level];
    readout.textContent = `crash ${level + 1} · relaunch in ${left} s`;
    note.textContent = `Crash ${level + 1}. The watchdog waits ${DELAYS[level]} s before relaunching. `
      + 'The spool is untouched — unacked audio sits encrypted on disk and replays when the new '
      + 'agent reconnects, so a crash costs the tail of one call, not the call.';
    countdown = setInterval(() => {
      left -= 1;
      if (left > 0) {
        readout.textContent = `crash ${level + 1} · relaunch in ${left} s`;
        return;
      }
      clearInterval(countdown); countdown = null;
      setAgent('relaunch');
      readout.textContent = 'relaunching…';
      setTimeout(() => {
        setAgent('running');
        readout.textContent = `agent running · ${level + 1} restart${level ? 's' : ''} this window`;
        ladderNote.textContent = 'restart count rides along in the heartbeat';
      }, 600);
    }, 1000);
  });

  document.querySelector('[data-sup="healthy"]').addEventListener('click', () => {
    clearInterval(countdown); countdown = null;
    level = -1;
    paint();
    setAgent('running');
    readout.textContent = 'healthy 120 s · backoff reset';
    note.textContent = 'A run that stays healthy for 120 s resets the ladder to 1 s. Without that '
      + 'floor, an agent crashing after 55 s of work would never clear its backoff and would inch '
      + 'toward a one-second retry loop that looks like a working fleet.';
    ladderNote.textContent = 'a healthy 120 s run resets to 1 s';
  });

  document.querySelector('[data-sup="reset"]').addEventListener('click', reset);
  reset();
})();

/* ------------------------------------------------------- 04 · the detector */

(function detector() {
  const statesG = document.getElementById('det-states');
  if (!statesG) return;

  // sentinel-core/src/config.rs → VadConfig::default()
  const SPEECH_MS_TO_CONFIRM = 300;
  const ARMED_TIMEOUT_MS = 20000;
  const HANGUP_SILENCE_MS = 8000;
  const WRAP_MS = 3000;

  const STATES = ['IDLE', 'ARMED', 'IN_CALL', 'WRAP', 'FINALIZE'];
  const DT = 100;            // ms of simulated time per tick
  const SPEED = 6;           // simulated seconds per real second
  const TICKS = 460;
  const X0 = 40, X1 = 1000, MID = 178, HALF = 46;

  const barsG = document.getElementById('det-bars');
  const metersG = document.getElementById('det-meters');
  const cursor = document.getElementById('det-cursor');
  const readout = document.querySelector('[data-det-readout]');
  const note = document.getElementById('det-note');
  const playBtn = document.querySelector('[data-det="play"]');

  // ---- the scripted call ------------------------------------------------
  // Energy per tick for each channel, 0..1, plus whether the softphone session is up.
  const near = new Float32Array(TICKS);
  const far = new Float32Array(TICKS);
  const session = new Uint8Array(TICKS);
  (function script() {
    const talk = (arr, fromS, toS, level) => {
      for (let i = Math.round(fromS * 1000 / DT); i < Math.round(toS * 1000 / DT) && i < TICKS; i++) {
        // A little shape so it reads as speech rather than a tone.
        arr[i] = Math.max(0.06, level * (0.55 + 0.45 * Math.abs(Math.sin(i / 2.1))));
      }
    };
    for (let i = Math.round(4000 / DT); i < TICKS; i++) session[i] = 1;
    talk(near, 5.5, 8.2, 0.9);
    talk(far, 8.6, 12.4, 0.8);
    talk(near, 12.9, 16.0, 0.85);
    talk(far, 16.4, 18.0, 0.7);
    talk(near, 20.5, 24.4, 0.9);   // a 2.5 s gap: under the 8 s hang-up threshold
    talk(far, 24.9, 28.6, 0.75);
    talk(near, 29.0, 30.0, 0.6);
    // then silence from 30.0 s — 8 s later the call is judged over
  })();

  const SPEECH_FLOOR = 0.12;

  function run(upto) {
    // A straight transcription of the Rust detector's rules, run over the script.
    let state = 'IDLE', speechMs = 0, silenceMs = 0, armedFor = 0, wrapFor = 0;
    let armExhausted = false, finalizeFor = 0;
    const trace = new Array(upto);
    for (let i = 0; i < upto; i++) {
      const loud = near[i] > SPEECH_FLOOR || far[i] > SPEECH_FLOOR;
      const active = session[i] === 1;
      if (!active) { state = 'IDLE'; armExhausted = false; speechMs = 0; }

      switch (state) {
        case 'IDLE':
          if (active && !armExhausted) { state = 'ARMED'; armedFor = 0; speechMs = 0; }
          break;
        case 'ARMED':
          armedFor += DT;
          speechMs = loud ? speechMs + DT : 0;
          if (speechMs >= SPEECH_MS_TO_CONFIRM) { state = 'IN_CALL'; silenceMs = 0; }
          else if (armedFor >= ARMED_TIMEOUT_MS) { state = 'IDLE'; armExhausted = true; }
          break;
        case 'IN_CALL':
          silenceMs = loud ? 0 : silenceMs + DT;
          if (silenceMs >= HANGUP_SILENCE_MS) { state = 'WRAP'; wrapFor = 0; }
          break;
        case 'WRAP':
          wrapFor += DT;
          if (loud) { state = 'IN_CALL'; silenceMs = 0; }       // hold that came back
          else if (wrapFor >= WRAP_MS) { state = 'FINALIZE'; finalizeFor = 0; }
          break;
        case 'FINALIZE':
          finalizeFor += DT;
          if (finalizeFor >= 500) { state = 'IDLE'; armExhausted = true; }
          break;
      }
      trace[i] = { state, speechMs, silenceMs, armedFor, loud, active };
    }
    return trace[upto - 1] || { state: 'IDLE', speechMs: 0, silenceMs: 0, armedFor: 0, loud: false, active: false };
  }

  // ---- static furniture -------------------------------------------------
  const chipW = 168, chipGap = 32;
  STATES.forEach((s, i) => {
    const x = 40 + i * (chipW + chipGap);
    statesG.appendChild(svg('rect', {
      x, y: 30, width: chipW, height: 46, rx: 8,
      fill: 'var(--surface-2)', stroke: 'var(--line)', 'data-chip': s,
    }));
    statesG.appendChild(svg('text', {
      x: x + chipW / 2, y: 58, 'text-anchor': 'middle', class: 'n-title', 'data-chip-text': s,
    }, s));
    if (i < STATES.length - 1) {
      statesG.appendChild(svg('path', {
        d: `M${x + chipW},53 L${x + chipW + chipGap - 6},53`,
        class: 'edge', 'marker-end': 'url(#ah)',
      }));
    }
  });

  const meterLabels = ['', '', ''];
  const meterNodes = meterLabels.map((_, i) => {
    const t = svg('text', { x: 40, y: 262 + i * 20, class: 'n-sub' }, '');
    metersG.appendChild(t);
    return t;
  });

  const bars = [];
  const bw = (X1 - X0) / TICKS;
  for (let i = 0; i < TICKS; i++) {
    const n = svg('rect', { x: X0 + i * bw, y: MID, width: Math.max(bw - 0.4, 0.6), height: 0, fill: 'var(--flow)', opacity: 0.85 });
    const f = svg('rect', { x: X0 + i * bw, y: MID, width: Math.max(bw - 0.4, 0.6), height: 0, fill: 'var(--tier-a)', opacity: 0.7 });
    barsG.appendChild(n); barsG.appendChild(f);
    bars.push([n, f]);
  }
  barsG.appendChild(svg('line', { x1: X0, y1: MID, x2: X1, y2: MID, stroke: 'var(--line)', 'stroke-width': 1 }));
  barsG.appendChild(svg('text', { x: X1 - 4, y: MID - 34, 'text-anchor': 'end', class: 'n-tiny' }, 'near · agent'));
  barsG.appendChild(svg('text', { x: X1 - 4, y: MID + 42, 'text-anchor': 'end', class: 'n-tiny' }, 'far · borrower'));

  // ---- playback ---------------------------------------------------------
  let at = 0, raf = null, last = 0;

  function paint() {
    for (let i = 0; i < TICKS; i++) {
      const on = i < at;
      const [n, f] = bars[i];
      const nh = on ? near[i] * HALF : 0;
      const fh = on ? far[i] * HALF : 0;
      n.setAttribute('y', MID - nh); n.setAttribute('height', nh);
      f.setAttribute('y', MID); f.setAttribute('height', fh);
    }
    cursor.setAttribute('x1', X0 + at * bw);
    cursor.setAttribute('x2', X0 + at * bw);

    // `at` advances by fractional ticks between frames; the simulation is integral.
    const s = run(Math.min(TICKS, Math.max(1, Math.floor(at))));
    STATES.forEach((name) => {
      const on = name === s.state;
      const chip = statesG.querySelector(`[data-chip="${name}"]`);
      const text = statesG.querySelector(`[data-chip-text="${name}"]`);
      chip.setAttribute('stroke', on ? 'var(--flow)' : 'var(--line)');
      chip.setAttribute('fill', on ? 'color-mix(in srgb, var(--flow) 18%, var(--surface-2))' : 'var(--surface-2)');
      text.setAttribute('fill', on ? 'var(--flow)' : 'var(--muted)');
    });

    const secs = (at * DT / 1000).toFixed(1);
    readout.textContent = `${s.state} · ${secs} s`;

    meterNodes[0].textContent = `softphone audio session: ${s.active ? 'active' : 'inactive'}`;
    if (s.state === 'ARMED') {
      meterNodes[1].textContent = `speech confirmed: ${s.speechMs} / ${SPEECH_MS_TO_CONFIRM} ms`;
      meterNodes[2].textContent = `arm timeout: ${(s.armedFor / 1000).toFixed(1)} / 20.0 s — discard the buffer if it expires`;
    } else if (s.state === 'IN_CALL' || s.state === 'WRAP') {
      meterNodes[1].textContent = `two-sided silence: ${(s.silenceMs / 1000).toFixed(1)} / 8.0 s`;
      meterNodes[2].textContent = s.state === 'WRAP'
        ? 'wrap: 3 s of grace in which speech returns the call to IN_CALL'
        : 'uploading: every segment spooled first, deleted only when acked';
    } else {
      meterNodes[1].textContent = '';
      meterNodes[2].textContent = '';
    }

    if (at >= TICKS) {
      note.textContent = 'Call over. Total tail cost: 8 s of silence to be sure the call ended, '
        + 'plus 3 s of wrap in which a returning voice would have cancelled the whole thing. '
        + 'The 2.5 s gap at 18 s did not end the call, and neither did the 0.4 s gaps between turns.';
    }
  }

  function frame(ts) {
    if (!last) last = ts;
    const dt = ts - last;
    last = ts;
    at = Math.min(TICKS, at + (dt * SPEED) / DT);
    paint();
    if (at >= TICKS) { stop(); return; }
    raf = requestAnimationFrame(frame);
  }

  function stop() {
    cancelAnimationFrame(raf); raf = null; last = 0;
    playBtn.textContent = 'Play';
    playBtn.setAttribute('aria-pressed', 'false');
  }

  playBtn.addEventListener('click', () => {
    if (raf) { stop(); return; }
    if (at >= TICKS) at = 0;
    playBtn.textContent = 'Pause';
    playBtn.setAttribute('aria-pressed', 'true');
    raf = requestAnimationFrame(frame);
  });

  document.querySelector('[data-det="reset"]').addEventListener('click', () => {
    stop(); at = 0; paint();
    note.textContent = 'Press play. The bars are simulated frame energy; the state chips above are '
      + 'driven by the same thresholds the Rust detector uses.';
  });

  paint();
})();

/* ------------------------------------------------------- 05 · audio path */

(function audioPath() {
  const stagesG = document.getElementById('aud-stages');
  if (!stagesG) return;

  const STAGES = [
    ['WASAPI capture', 'two streams, device rate', '2 ch · 48 kHz'],
    ['Resample', 'to 16 kHz mono, per channel', '16 kHz · mono'],
    ['Opus encode', '20 ms frame at 24 kbps', '60 B / frame'],
    ['Segment', '50 frames = one second', '3 000 B'],
    ['Record header', '34 B little-endian', '3 034 B'],
    ['Spool → uplink', 'SQLCipher, then WSS', 'deleted on ack'],
  ];

  const tokenG = document.getElementById('aud-token');
  const meterG = document.getElementById('aud-meter');
  const readout = document.querySelector('[data-aud-readout]');
  const note = document.getElementById('aud-note');
  const BASE_NOTE = note.textContent;
  const playBtn = document.querySelector('[data-aud="play"]');

  const W = 150, GAP = 20, Y = 90, H = 84;
  const xOf = (i) => 20 + i * (W + GAP);

  function twoLines(text, max) {
    const words = text.split(' ');
    let a = '', b = '';
    for (const w of words) {
      if (!b && (a ? a.length + 1 + w.length : w.length) <= max) a = a ? `${a} ${w}` : w;
      else b = b ? `${b} ${w}` : w;
    }
    return [a, b];
  }

  STAGES.forEach(([title, sub, num], i) => {
    const x = xOf(i);
    stagesG.appendChild(svg('rect', { x, y: Y, width: W, height: H, rx: 8, fill: 'var(--surface-2)', stroke: 'var(--line)', 'data-stage': i }));
    stagesG.appendChild(svg('text', { x: x + 12, y: Y + 26, class: 'n-title', 'font-size': 12.5 }, title));
    // Two short lines, so the caption never overflows the 150 px box.
    const [l1, l2] = twoLines(sub, 21);
    stagesG.appendChild(svg('text', { x: x + 12, y: Y + 45, class: 'n-tiny' }, l1));
    stagesG.appendChild(svg('text', { x: x + 12, y: Y + 58, class: 'n-tiny' }, l2));
    stagesG.appendChild(svg('text', { x: x + 12, y: Y + 76, class: 'n-sub', 'data-stage-num': i, fill: 'var(--flow)' }, num));
    if (i < STAGES.length - 1) {
      stagesG.appendChild(svg('path', { d: `M${x + W},${Y + H / 2} L${x + W + GAP - 4},${Y + H / 2}`, class: 'edge', 'marker-end': 'url(#ah)' }));
    }
  });

  stagesG.appendChild(svg('text', { x: 20, y: 40, class: 'n-lane' }, 'ONE CHANNEL · ONE SECOND'));
  stagesG.appendChild(svg('text', { x: 20, y: 288, class: 'n-tiny' },
    'The far channel runs the identical path in parallel. The two are never summed — every'));
  stagesG.appendChild(svg('text', { x: 20, y: 303, class: 'n-tiny' },
    'per-speaker result downstream, and the absence of any diarization step, depends on that.'));

  const token = svg('circle', { cx: xOf(0) + W / 2, cy: Y + H + 24, r: 7, fill: 'var(--flow)' });
  tokenG.appendChild(token);
  const tokenLabel = svg('text', { x: xOf(0) + W / 2, y: Y + H + 44, 'text-anchor': 'middle', class: 'n-tiny', fill: 'var(--flow)' }, '');
  tokenG.appendChild(tokenLabel);

  const frameDots = [];
  for (let i = 0; i < 50; i++) {
    const d = svg('rect', { x: 20 + (i % 25) * 7, y: 244 + Math.floor(i / 25) * 8, width: 5, height: 5, rx: 1, fill: 'var(--line)' });
    meterG.appendChild(d); frameDots.push(d);
  }
  meterG.appendChild(svg('text', { x: 20, y: 236, class: 'n-tiny' }, '50 Opus frames per segment'));

  let stage = 0, frames = 0, timer = null;

  function paint() {
    STAGES.forEach((_, i) => {
      const on = i === stage;
      const box = stagesG.querySelector(`[data-stage="${i}"]`);
      box.setAttribute('stroke', on ? 'var(--flow)' : 'var(--line)');
      box.setAttribute('fill', on ? 'color-mix(in srgb, var(--flow) 14%, var(--surface-2))' : 'var(--surface-2)');
    });
    const cx = xOf(stage) + W / 2;
    token.setAttribute('cx', cx);
    tokenLabel.setAttribute('x', cx);
    tokenLabel.textContent = STAGES[stage][2];
    frameDots.forEach((d, i) => d.setAttribute('fill', i < frames ? 'var(--flow)' : 'var(--line)'));
    readout.textContent = `${frames} frame${frames === 1 ? '' : 's'} · stage ${stage + 1}/6`;
  }

  function stop() {
    clearInterval(timer); timer = null;
    playBtn.textContent = 'Play';
    playBtn.setAttribute('aria-pressed', 'false');
  }

  playBtn.addEventListener('click', () => {
    if (timer) { stop(); return; }
    playBtn.textContent = 'Pause';
    playBtn.setAttribute('aria-pressed', 'true');
    timer = setInterval(() => {
      if (stage === 2 && frames < 50) { frames = Math.min(50, frames + 5); paint(); return; }
      stage += 1;
      if (stage >= STAGES.length) {
        stage = STAGES.length - 1;
        note.textContent = 'That segment is now a row in the SQLCipher spool and a frame on the '
          + 'socket. It is deleted from disk when — and only when — an ack covers its sequence '
          + 'number, which is why pulling the network cable costs bandwidth rather than audio.';
        stop();
        return;
      }
      paint();
    }, 520);
  });

  document.querySelector('[data-aud="reset"]').addEventListener('click', () => {
    stop(); stage = 0; frames = 0; paint(); note.textContent = BASE_NOTE;
  });

  paint();
})();

/* -------------------------------------------------- 06 + 07 · sequences */

const SEQUENCES = {
  wire: {
    lanes: [
      { id: 'a', label: 'SentinelAgent', sub: 'spool + uplink' },
      { id: 'g', label: 'Gateway', sub: '/v1/ingest' },
      { id: 'o', label: 'Object store', sub: 'Opus segments' },
      { id: 'p', label: 'PostgreSQL', sub: 'as sentinel_app' },
    ],
    rows: [
      { from: 'a', to: 'g', text: 'WSS upgrade · Sec-WebSocket-Protocol: sentinel.v1' },
      { note: 'tenant_id + device_id come from the client certificate; user_uid + role from the bearer token. If the two disagree the gateway closes with 4403.' },
      { from: 'a', to: 'g', text: 'call.start { call_id (ULID), started_at, capture_tier }' },
      { from: 'g', to: 'p', text: "INSERT calls … status = 'ingesting'" },
      { from: 'a', to: 'g', text: 'media record: 34 B header + 1 s of Opus, seq 1…12', kind: 'async' },
      { from: 'g', to: 'o', text: 'PUT tenant/day/call/channel/seq' },
      { from: 'g', to: 'p', text: 'INSERT media_segments … ON CONFLICT (call_id, channel, seq) DO NOTHING' },
      { from: 'g', to: 'a', text: 'ack { channel: 0, through_seq: 12 }', kind: 'return' },
      { note: 'the spool deletes rows ≤ 12 on channel 0, and nothing else ever deletes them. An ack whose watermark write failed is suppressed rather than sent.' },
      { divider: 'the network drops mid-call' },
      { from: 'a', to: 'a', text: 'backoff = random(0, min(60 s, 1 s · 2ⁿ)) — full jitter' },
      { from: 'a', to: 'g', text: 'call.start (same call_id, verbatim)' },
      { from: 'g', to: 'a', text: 'resume { acked: { "0": 12, "1": 11 } }', kind: 'return' },
      { from: 'a', to: 'g', text: 'replay from acked + 1 on each channel', kind: 'async' },
      { from: 'a', to: 'g', text: 'call.end { ended_at, reason }' },
      { from: 'g', to: 'p', text: "UPDATE calls SET status = 'transcribing', duration_ms = …" },
    ],
  },

  life: {
    lanes: [
      { id: 's', label: 'SentinelService', sub: 'SYSTEM' },
      { id: 'a', label: 'SentinelAgent', sub: 'user session' },
      { id: 'g', label: 'Gateway', sub: 'Go' },
      { id: 'w', label: 'Pipeline', sub: 'Python' },
      { id: 'p', label: 'Portal', sub: 'React' },
    ],
    rows: [
      { divider: 'machine boot · first run' },
      { from: 's', to: 's', text: 'generate a non-exportable P-256 key in CNG — never in a file, never in process memory' },
      { from: 's', to: 'g', text: 'POST /v1/devices/enroll { single-use token, CSR }' },
      { from: 'g', to: 's', text: 'device certificate · 1 year, renewed with 30 days left', kind: 'return' },
      { from: 's', to: 'g', text: 'GET /v1/policy (mTLS) — pinned device, VAD config, retention' },
      { divider: 'an agent logs on' },
      { from: 's', to: 'a', text: 'CreateProcessAsUser on WTS_SESSION_LOGON — not a Run key' },
      { from: 'a', to: 's', text: 'GetConfig over the named pipe (one writer for machine state)' },
      { from: 'a', to: 'g', text: 'PKCE in the system browser → POST /v1/sessions' },
      { note: 'capture stays BLOCKED until both identities exist: a device certificate and a signed-in user. Without a policy the pinned device is unknown, and capture must not start.' },
      { divider: 'a call' },
      { from: 'a', to: 'a', text: 'softphone session active → ARMED → 300 ms of speech → IN_CALL' },
      { from: 'a', to: 'g', text: 'WSS /v1/ingest — call.start, media, acks, call.end', kind: 'async' },
      { from: 'a', to: 'g', text: 'POST /v1/heartbeat every 30 s — state, spool depth, restarts. No PII.' },
      { divider: 'finalize' },
      { from: 'g', to: 'w', text: 'publish sentinel.call.finalize', kind: 'todo' },
      { from: 'w', to: 'w', text: 'ASR per channel → analysis → 10 deterministic rules on 100% of calls' },
      { from: 'w', to: 'w', text: 'LLM judge on flagged calls plus a deterministic sample; analysis failing must not stop compliance' },
      { from: 'w', to: 'g', text: 'transcript, summary, PTP in paise, per-speaker sentiment, flags' },
      { divider: 'review' },
      { from: 'p', to: 'g', text: 'GET /v1/calls · /v1/compliance/flags — scoped by row-level security' },
      { note: 'every read of call content is audited, not only writes: a reviewer paging through borrower summaries leaves a trail.' },
    ],
  },
};

(function sequences() {
  const W = 1040, TOP = 74, ROW = 40, PAD = 34;

  for (const key in SEQUENCES) {
    const host = document.querySelector(`svg[data-seq="${key}"]`);
    if (!host) continue;
    const spec = SEQUENCES[key];
    const readout = document.querySelector(`[data-seq-readout="${key}"]`);
    const noteEl = document.querySelector(`[data-seq-note="${key}"]`);
    const playBtn = document.querySelector(`[data-seq-play="${key}"]`);
    const baseNote = noteEl.textContent;

    const n = spec.lanes.length;
    const laneW = W / n;
    const cx = (id) => {
      const i = spec.lanes.findIndex((l) => l.id === id);
      return laneW * i + laneW / 2;
    };

    // Rows are not all the same height: a wrapped note needs the room its lines take,
    // and a fixed pitch would have it sitting on top of the message below it.
    const noteLines = spec.rows.map((r) => (r.note ? wrap(r.note, 118) : null));
    const heights = spec.rows.map((r, i) => {
      if (r.note) return noteLines[i].length * 15 + 26;
      if (r.kind === 'todo') return ROW + 14;
      return ROW;
    });
    const tops = [];
    let acc = TOP;
    for (const h of heights) { tops.push(acc); acc += h; }
    const height = acc + PAD;
    host.setAttribute('viewBox', `0 0 ${W} ${height}`);
    clear(host);

    const defs = svg('defs');
    for (const [id, color] of [['sq', 'var(--line)'], ['sq-flow', 'var(--flow)'], ['sq-todo', 'var(--tier-b)']]) {
      const m = svg('marker', { id: `${key}-${id}`, viewBox: '0 0 10 10', refX: 9, refY: 5, markerWidth: 7, markerHeight: 7, orient: 'auto-start-reverse' });
      m.appendChild(svg('path', { d: 'M0,0 L10,5 L0,10 z', fill: color }));
      defs.appendChild(m);
    }
    host.appendChild(defs);

    // lifelines + headers
    spec.lanes.forEach((l, i) => {
      const x = laneW * i + laneW / 2;
      host.appendChild(svg('line', {
        x1: x, y1: 58, x2: x, y2: height - 12,
        stroke: 'var(--line)', 'stroke-dasharray': '3 5',
      }));
      const bw = Math.min(laneW - 20, 190);
      host.appendChild(svg('rect', { x: x - bw / 2, y: 10, width: bw, height: 44, rx: 8, fill: 'var(--surface-2)', stroke: 'var(--line)' }));
      host.appendChild(svg('text', { x, y: 30, 'text-anchor': 'middle', class: 'n-title', 'font-size': 12.5 }, l.label));
      host.appendChild(svg('text', { x, y: 46, 'text-anchor': 'middle', class: 'n-tiny' }, l.sub));
    });

    // rows
    const nodes = spec.rows.map((r, i) => {
      const y = tops[i] + 20;
      const g = svg('g', { opacity: 0 });

      if (r.divider) {
        g.appendChild(svg('line', { x1: 8, y1: y, x2: W - 8, y2: y, stroke: 'var(--line)', 'stroke-dasharray': '2 6' }));
        const label = svg('text', { x: W / 2, y: y + 4, 'text-anchor': 'middle', class: 'n-lane', fill: 'var(--tier-b)' }, r.divider.toUpperCase());
        const pad = svg('rect', { x: W / 2 - r.divider.length * 3.6 - 10, y: y - 9, width: r.divider.length * 7.2 + 20, height: 18, fill: 'var(--surface)' });
        g.appendChild(pad); g.appendChild(label);
      } else if (r.note) {
        const lines = noteLines[i];
        const h = lines.length * 15 + 14;
        g.appendChild(svg('rect', { x: 40, y: y - 16, width: W - 80, height: h, rx: 6, fill: 'var(--surface-2)', stroke: 'var(--line-soft)' }));
        lines.forEach((ln, k) => g.appendChild(svg('text', { x: 54, y: y - 1 + k * 15, class: 'n-tiny' }, ln)));
      } else if (r.from === r.to) {
        const x = cx(r.from);
        g.appendChild(svg('path', {
          d: `M${x},${y - 10} h34 v20 h-34`, fill: 'none',
          stroke: 'var(--flow)', 'stroke-width': 1.4, 'marker-end': `url(#${key}-sq-flow)`,
        }));
        // A self-message on one of the right-hand lanes would run off the canvas, so
        // the label flips to the other side of the loop rather than being clipped.
        const est = r.text.length * 5.7;
        const right = x + 44 + est < W - 8;
        g.appendChild(svg('text', {
          x: right ? x + 44 : x - 8, y: y + 4, class: 'edge-label',
          'text-anchor': right ? 'start' : 'end',
        }, r.text));
      } else {
        const x1 = cx(r.from), x2 = cx(r.to);
        const dir = x2 > x1 ? 1 : -1;
        const stroke = r.kind === 'todo' ? 'var(--tier-b)' : r.kind === 'return' ? 'var(--tier-a)' : 'var(--flow)';
        const marker = r.kind === 'todo' ? `url(#${key}-sq-todo)` : `url(#${key}-sq-flow)`;
        const line = svg('line', {
          x1: x1 + dir * 4, y1: y, x2: x2 - dir * 8, y2: y,
          stroke, 'stroke-width': 1.4, 'marker-end': marker,
        });
        if (r.kind === 'todo' || r.kind === 'return') line.setAttribute('stroke-dasharray', '5 4');
        if (r.kind === 'return') line.setAttribute('marker-end', `url(#${key}-sq-flow)`);
        g.appendChild(line);
        const mid = (x1 + x2) / 2;
        const t = svg('text', { x: mid, y: y - 8, 'text-anchor': 'middle', class: 'edge-label' }, r.text);
        if (r.kind === 'todo') t.setAttribute('fill', 'var(--tier-b)');
        g.appendChild(t);
        if (r.kind === 'todo') {
          g.appendChild(svg('text', { x: mid, y: y + 15, 'text-anchor': 'middle', class: 'n-tiny', fill: 'var(--tier-b)' },
            'no publisher yet — the consumer is written and tested, the producer is not'));
        }
      }
      host.appendChild(g);
      return g;
    });

    let at = 0, timer = null;

    function paint() {
      nodes.forEach((g, i) => {
        g.setAttribute('opacity', i < at ? 1 : 0.06);
        g.style.transition = REDUCED ? 'none' : 'opacity .35s';
      });
      readout.textContent = `${at} / ${nodes.length}`;
      const r = spec.rows[at - 1];
      if (!r) { noteEl.textContent = baseNote; return; }
      noteEl.textContent = r.note || r.divider || r.text;
    }

    function stop() {
      clearInterval(timer); timer = null;
      playBtn.textContent = 'Play';
      playBtn.setAttribute('aria-pressed', 'false');
    }

    playBtn.addEventListener('click', () => {
      if (timer) { stop(); return; }
      if (at >= nodes.length) at = 0;
      playBtn.textContent = 'Pause';
      playBtn.setAttribute('aria-pressed', 'true');
      timer = setInterval(() => {
        at += 1; paint();
        if (at >= nodes.length) stop();
      }, 1100);
    });
    document.querySelector(`[data-seq-step="${key}"]`).addEventListener('click', () => {
      stop(); at = at >= nodes.length ? 0 : at + 1; paint();
    });
    document.querySelector(`[data-seq-reset="${key}"]`).addEventListener('click', () => {
      stop(); at = 0; paint();
    });

    paint();
  }

  function wrap(text, max) {
    const out = [], words = text.split(' ');
    let line = '';
    for (const w of words) {
      if ((line + ' ' + w).trim().length > max) { out.push(line.trim()); line = w; }
      else line = (line + ' ' + w).trim();
    }
    if (line) out.push(line);
    return out;
  }
})();

/* ------------------------------------------------------- 08 · RLS viewer */

(function rls() {
  const rolesEl = document.getElementById('rls-roles');
  if (!rolesEl) return;

  // The fixture is db/test/rls_test.sh's, trimmed. sup-north is a member of both
  // teams, which is why a supervisor sees three calls and not one.
  const CALLS = [
    { id: 'c…000a', tenant: 'Acme BPO', agent: 'agent-a', team: 'Team North', flags: 0 },
    { id: 'c…000b', tenant: 'Acme BPO', agent: 'agent-b', team: 'Team North', flags: 1 },
    { id: 'c…000c', tenant: 'Acme BPO', agent: 'agent-c', team: 'Team South', flags: 0 },
    { id: 'c…00ff', tenant: 'Rival BPO', agent: 'rival-admin', team: '—', flags: 0 },
  ];

  const VIEWS = [
    {
      key: 'agent', label: 'Agent', tenant: "'acme'", uid: "'agent-a'", role: "'agent'",
      keep: (c) => c.tenant === 'Acme BPO' && c.agent === 'agent-a',
      note: 'An agent sees their own calls. Asking for another agent\'s call by id returns nothing — not 403, nothing, because the row is not visible to the query at all.',
    },
    {
      key: 'supervisor', label: 'Supervisor', tenant: "'acme'", uid: "'sup-north'", role: "'supervisor'",
      keep: (c) => c.tenant === 'Acme BPO',
      note: 'A supervisor sees the teams they are a member of. sup-north is in both, so all three Acme calls are visible — a supervisor over several teams seeing only one was a real defect this test caught.',
    },
    {
      key: 'qa', label: 'QA', tenant: "'acme'", uid: "'qa-1'", role: "'qa'",
      keep: (c) => c.tenant === 'Acme BPO',
      note: 'QA and compliance work across the whole tenant. Same endpoint, same query, wider answer — decided by the policy, not by a branch in a handler.',
    },
    {
      key: 'client', label: 'Bank client', tenant: "'acme'", uid: "'client-1'", role: "'client'",
      keep: (c) => c.tenant === 'Acme BPO' && c.flags > 0,
      note: 'The bank client sees flagged calls only. Non-flagged calls are not filtered out in the browser or in the handler — they never leave the database.',
    },
    {
      key: 'rival', label: 'Rival tenant', tenant: "'rival'", uid: "'rival-admin'", role: "'admin'",
      keep: (c) => c.tenant === 'Rival BPO',
      note: 'A different tenant\'s admin, with a perfectly valid token, sees only their own tenant. This is the property the whole design rests on.',
    },
    {
      key: 'none', label: 'No context set', tenant: 'NULL', uid: 'NULL', role: 'NULL',
      keep: () => false,
      note: 'The failure mode that matters. A query that forgets to set its context returns zero rows rather than every row — the policies fail closed, so a bug in the gateway leaks nothing.',
    },
  ];

  const tenantEl = document.getElementById('rls-tenant');
  const uidEl = document.getElementById('rls-uid');
  const roleEl = document.getElementById('rls-role');
  const bodyEl = document.getElementById('rls-body');
  const countEl = document.getElementById('rls-count');
  const noteEl = document.getElementById('rls-note');

  const trs = CALLS.map((c) => {
    const tr = document.createElement('tr');
    for (const [v, cls] of [[c.id, 'mono'], [c.tenant, ''], [c.agent, 'mono'], [c.team, ''],
      [c.flags ? `${c.flags} critical` : '—', '']]) {
      const td = document.createElement('td');
      td.textContent = v;
      if (cls) td.className = cls;
      tr.appendChild(td);
    }
    bodyEl.appendChild(tr);
    return tr;
  });

  function show(view) {
    tenantEl.textContent = view.tenant;
    uidEl.textContent = view.uid;
    roleEl.textContent = view.role;
    let n = 0;
    CALLS.forEach((c, i) => {
      const keep = view.keep(c);
      if (keep) n += 1;
      trs[i].className = keep ? 'shown-row' : 'hidden-row';
    });
    countEl.innerHTML = `→ <b>${n} row${n === 1 ? '' : 's'}</b> of ${CALLS.length}`;
    noteEl.textContent = view.note;
    [...rolesEl.children].forEach((b) => b.classList.toggle('on', b.dataset.role === view.key));
  }

  VIEWS.forEach((v) => {
    const b = document.createElement('button');
    b.className = 'btn';
    b.type = 'button';
    b.dataset.role = v.key;
    b.textContent = v.label;
    b.addEventListener('click', () => show(v));
    rolesEl.appendChild(b);
  });

  show(VIEWS[0]);
})();
