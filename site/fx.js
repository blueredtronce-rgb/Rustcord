// Background motion: "the forge is lit". Embers behind the wordmark, sparks off every heat seam.
// Two canvases, one rAF loop that only runs while something moves. Pure helpers are exported for tests.
export const ease = t => t < .5 ? 4 * t * t * t : 1 - (-2 * t + 2) ** 3 / 2; // == --ease-in-out
export const tierFor = ({ w, hc = 8, mem = 8, save = false }) => save ? 0 : (w < 720 || hc <= 4 || mem <= 2) ? 1 : w < 1280 ? 2 : 3;
export const wmSeamY = (p, top, h) => top + h * (1.5 * ease(p) - .25); // wm-forge: bg 250% tall, band centre at 50%
const ST = [[0, 254, 238, 193], [.18, 235, 193, 102], [.45, 246, 136, 66], [.78, 164, 45, 21], [1, 164, 45, 21]];
export function ramp(t) {
  t = Math.min(1, Math.max(0, t));
  let i = 1; while (t > ST[i][0]) i++;
  const a = ST[i - 1], b = ST[i], k = (t - a[0]) / (b[0] - a[0]);
  return [1, 2, 3].map(j => Math.round(a[j] + (b[j] - a[j]) * k)).concat(t > .78 ? Math.round(Math.max(0, 1 - (t - .78) / .22) * 1e4) / 1e4 : 1);
}

// per tier 0..3: rest/fanned embers, spark cap, sparks per wordmark sweep / shot pass, sizzle/s, boot, strike, hover embers/s, dpr cap
const TB = { rest: [0, 26, 60, 90], fan: [0, 40, 90, 140], cap: [60, 90, 180, 280], wms: [24, 40, 72, 96], shot: [28, 44, 80, 110],
  siz: [0, 4, 8, 10], boot: [10, 14, 22, 28], strike: [14, 20, 32, 40], hov: [0, 0, 12, 14], dpr: [1, 1, 1.5, 1.5] };
const HOLD = 3500, FADE = 1200, TAU = Math.PI * 2;
const U = (a, b) => a + Math.random() * (b - a), cl = v => Math.min(1, Math.max(0, v));

class Pool { // preallocated, swap-remove
  constructor(cap) { this.cap = cap; this.a = []; this.n = 0; }
  add() { if (this.n >= this.cap) return null; return this.a[this.n] || (this.a[this.n] = {}), this.a[this.n++]; }
  del(i) { const a = this.a, t = a[i]; a[i] = a[--this.n]; a[this.n] = t; }
}

export function startFx() {
  const html = document.documentElement;
  const mqRM = matchMedia('(prefers-reduced-motion: reduce)');
  let stop = null;
  const go = () => {
    if (stop) return;
    if (!html.classList.contains('motion') || matchMedia('(forced-colors: active)').matches) return;
    const q = new URLSearchParams(location.search).get('fx');
    if (q === '0') return;
    stop = create(q === 'debug');
  };
  mqRM.addEventListener('change', () => { if (html.classList.contains('motion')) go(); else if (stop) { stop(); stop = null; } });
  go();
}

function create(debug) {
  const html = document.documentElement;
  const hero = document.querySelector('.hero'), wm = document.querySelector('.wordmark');
  if (!hero || !wm) return null;
  let tier = tierFor({ w: innerWidth, hc: navigator.hardwareConcurrency, mem: navigator.deviceMemory, save: navigator.connection && navigator.connection.saveData });
  const C = k => TB[k][tier], dpr = Math.min(devicePixelRatio || 1, C('dpr'));
  const mk = cls => { const c = document.createElement('canvas'); c.className = cls; c.setAttribute('aria-hidden', 'true'); return c; };
  const hc = tier ? mk('fx-hearth') : null, fc = mk('fx-sparks');
  const hx = hc && hc.getContext('2d'), fx = fc.getContext('2d');
  if (!fx || (hc && !hx)) return null;
  if (hc) hero.prepend(hc);
  document.body.append(fc);
  const ctxs = [hx, fx].filter(Boolean);
  const setup = x => { x.setTransform(dpr, 0, 0, dpr, 0, 0); x.globalCompositeOperation = 'lighter'; x.lineCap = 'round'; };

  // sprite atlas for embers, palette for sparks
  const CELL = Math.round(32 * dpr), atlas = document.createElement('canvas');
  atlas.width = CELL * 24; atlas.height = CELL;
  const pal = [], ax = atlas.getContext('2d');
  for (let i = 0; i < 24; i++) {
    const [r, g, b, a] = ramp(i / 23), cx = i * CELL + CELL / 2, h = CELL / 2, gr = ax.createRadialGradient(cx, h, 0, cx, h, h), c = `${r},${g},${b},`;
    gr.addColorStop(0, `rgba(${(r + 510) / 3 | 0},${(g + 492) / 3 | 0},${(b + 440) / 3 | 0},${a})`);
    gr.addColorStop(.12, `rgba(${c}${a})`); gr.addColorStop(.3, `rgba(${c}${.4 * a})`); gr.addColorStop(.6, `rgba(${c}${.1 * a})`); gr.addColorStop(1, `rgba(${c}0)`);
    ax.fillStyle = gr; ax.fillRect(i * CELL, 0, CELL, CELL); pal.push(`rgba(${c}${a})`);
  }

  const hearth = new Pool(TB.fan[tier]), front = new Pool(TB.cap[tier] + 30);
  const ac = new AbortController(), sig = ac.signal;
  let raf = 0, lit = false, last = 0, lastActive = performance.now(), lastScroll = 0, act = 1, heat = 0, ignited = false;
  let heroIn = false, tourIn = false, drewH = false, drewF = false, fanV = .45, hover = null, hoverAcc = 0, hAcc = 0, wmAcc = 0, wmE = .233;
  let px = 0, py = 0, ppx = 0, ppy = 0, pvx = 0, pvy = 0, fine = false, downs = 0, warm = 0, on = false;
  const iv = [], ring = new Float32Array(600); let rn = 0, frames = 0;
  const geo = { hl: 0, ht: 0, hh: 0, wl: 0, wt: 0, ww: 0, wh: 0 };
  const phi = [U(0, TAU), U(0, TAU), U(0, TAU)];
  const upd = () => {
    const hr = hero.getBoundingClientRect(), r = wm.getBoundingClientRect();
    geo.hl = hr.left; geo.ht = hr.top + scrollY; geo.hh = hr.height; geo.wl = r.left - hr.left; geo.wt = r.top - hr.top; geo.ww = r.width; geo.wh = r.height;
  };
  upd();

  // ---- spawning ----
  const ember = (p, z) => {
    p.z = z; p.vx = U(-10, 10) * z; p.vy = -U(22, 58) * (.55 + .45 * z); p.life = U(2.8, 5.6); p.r = U(1.3, 2.9) * (.55 + .45 * z);
    p.amp = U(6, 22); p.f = U(.9, 2.2); p.ph = U(0, TAU); p.tw = U(5, 11); p.t0 = U(.1, .28); p.age = 0; p.gx = p.gy = 0;
  };
  const spawnH = pre => {
    const p = hearth.add(); if (!p) return;
    ember(p, U(.35, 1)); p.kind = 0; p.x = geo.wl + U(-.06, 1.06) * geo.ww; p.y = geo.wt + geo.wh * U(.62, 1.02); if (pre) p.age = U(0, pre) * p.life;
  };
  const sp = (x, y, vx, vy, s, life, t0 = 0, fk = .1) => {
    if (front.n >= C('cap')) return;
    const p = front.add(); p.kind = 1; p.x = x; p.y = y; p.vx = vx; p.vy = vy; p.s = s; p.age = 0; p.life = life; p.t0 = t0; p.fk = fk; p.forked = 0;
  };
  const hov = r => {
    const p = front.add(); if (!p) return;
    ember(p, 1); p.kind = 2; p.x = r.left + U(.1, .9) * r.width; p.y = r.top + 2; p.vx = U(-12, 12); p.vy = -U(30, 70); p.life = U(.5, 1); p.r = U(.9, 1.8); p.t0 = U(.12, .3);
  };
  const burst = (n, x, y, vxa, vxb, vya, vyb, la, lb, s, fk) => { for (let i = 0; i < n; i++) sp(x, y, U(vxa, vxb), -U(vya, vyb), s, U(la, lb), 0, fk); };

  // ---- seams (found through the Web Animations API, never reimplemented) ----
  let wmAnim = wm.getAnimations().find(a => a.animationName === 'wm-forge') || null;
  const seamState = new WeakMap(); let seams = [];
  const collect = () => {
    seams = document.getAnimations().filter(a => a.animationName === 'seam' && a.effect && a.effect.pseudoElement === '::before').map(a => {
      let s = seamState.get(a);
      if (!s) {
        const li = a.effect.target.closest('.tour-item');
        seamState.set(a, s = { a, last: -1, acc: 0, idx: li ? [...li.parentNode.children].indexOf(li) : -1 });
      }
      return s;
    });
  };
  collect();

  // ---- activity ----
  const wake = () => { if (!raf && !document.hidden) { warm = 0; raf = requestAnimationFrame(frame); } };
  for (const n of ['pointermove', 'pointerdown', 'wheel', 'scroll', 'keydown', 'touchstart']) addEventListener(n, e => {
    lastActive = performance.now(); if (n === 'scroll') lastScroll = lastActive;
    if (n[0] === 'p') { px = e.clientX; py = e.clientY; fine = e.pointerType !== 'touch'; }
    wake();
  }, { passive: true, signal: sig });
  document.addEventListener('visibilitychange', () => { if (document.hidden) { cancelAnimationFrame(raf); raf = 0; } else { last = 0; wake(); } }, { signal: sig });
  document.addEventListener('animationstart', e => { if (e.animationName === 'seam') { collect(); wake(); } }, { signal: sig });
  document.addEventListener('animationend', e => {
    if (e.animationName !== 'boot') return;
    const t = document.querySelector('.boot-track'); if (!t) return;
    const r = t.getBoundingClientRect(); burst(C('boot'), r.right, r.top + r.height / 2, 60, 420, 40, 380, .3, .7, 1, .1); wake();
  }, { signal: sig });
  const canHover = matchMedia('(hover: hover) and (pointer: fine)').matches;
  document.querySelectorAll('[data-dl-button]').forEach(b => {
    const strike = x => { const r = b.getBoundingClientRect(); burst(C('strike'), Math.min(r.right - 8, Math.max(r.left + 8, x)), r.top + 3, -380, 380, 160, 620, .3, .75, 1, .12); wake(); };
    b.addEventListener('pointerdown', e => { if (e.button === 0) strike(e.clientX); }, { signal: sig });
    b.addEventListener('click', e => { if (e.detail === 0) { const r = b.getBoundingClientRect(); strike(r.left + r.width / 2); } }, { signal: sig });
    if (canHover && TB.hov[tier]) {
      b.addEventListener('pointerenter', () => { hover = b; wake(); }, { signal: sig });
      b.addEventListener('pointerleave', () => { if (hover === b) hover = null; }, { signal: sig });
    }
  });

  // ---- observers ----
  const ios = [], ro = [];
  const watch = (el, f) => { if (!el) return; const o = new IntersectionObserver(([e]) => f(e.isIntersecting)); o.observe(el); ios.push(o); };
  watch(hero, v => {
    heroIn = v;
    if (!v) { hearth.n = 0; if (hx) hx.clearRect(0, 0, 1e4, 1e4); drewH = false; }
    else if (ignited && act > 0) for (let i = 0, n = C('rest') * .5; i < n; i++) spawnH(1);
    wake();
  });
  watch(document.querySelector('[data-tour]'), v => { tourIn = v; wake(); });
  const rsz = new ResizeObserver(es => es.forEach(en => { const c = en.target, r = en.contentRect; c.width = Math.round(r.width * dpr); c.height = Math.round(r.height * dpr); setup(c === fc ? fx : hx); wake(); }));
  rsz.observe(fc); if (hc) rsz.observe(hc);
  const geoRo = new ResizeObserver(() => { upd(); wake(); }); geoRo.observe(wm); geoRo.observe(hero);
  let rt = 0; addEventListener('resize', () => { clearTimeout(rt); rt = setTimeout(collect, 200); }, { signal: sig });
  ctxs.forEach(setup);

  // ---- drawing ----
  const sprite = (X, p, a, yo) => {
    const lt = p.age / p.life, k = Math.min(23, ((p.t0 + (1 - p.t0) * lt) * 23) | 0), sz = p.r * 10;
    X.globalAlpha = a * Math.min(1, lt / .08) * (lt > .7 ? (1 - lt) / .3 : 1) * (.5 + .5 * p.z) * (.75 + .25 * Math.sin(p.tw * p.age + p.ph));
    X.drawImage(atlas, k * CELL, 0, CELL, CELL, p.x - sz / 2, p.y + yo - sz / 2, sz, sz);
  };
  const cnt = new Uint16Array(24), ci = new Uint8Array(400);
  const drawSparks = () => {
    cnt.fill(0); const a = front.a;
    for (let i = 0; i < front.n; i++) { const p = a[i]; if (p.kind !== 1) continue; const lt = p.age / p.life; ci[i] = Math.min(23, ((p.t0 + (1 - p.t0) * lt) * 23) | 0); cnt[ci[i]]++; }
    fx.lineWidth = 1.4;
    for (let c = 0; c < 24; c++) {
      if (!cnt[c]) continue;
      fx.beginPath();
      for (let i = 0; i < front.n; i++) { const p = a[i]; if (p.kind === 1 && ci[i] === c) { fx.moveTo(p.x - p.vx * .024, p.y - p.vy * .024); fx.lineTo(p.x, p.y); } }
      fx.strokeStyle = pal[c];
      if (c < 7) { fx.lineWidth = 4; fx.globalAlpha = .2; fx.stroke(); fx.lineWidth = 1.4; fx.globalAlpha = 1; }
      fx.stroke();
    }
    for (let i = 0; i < front.n; i++) if (a[i].kind === 2) sprite(fx, a[i], 1, 0);
    fx.globalAlpha = 1;
  };

  // ---- the frame ----
  function frame() {
    raf = 0;
    const t0 = performance.now(), now = t0, dt = last ? Math.min(.05, (now - last) / 1000) : .016;
    if (last && ++warm > 20 && now - last < 100) { iv.push(now - last); if (iv.length >= 90) { iv.sort((a, b) => a - b); if (iv[45] > 20 && tier > 1) tier--; iv.length = 0; } }
    last = now;
    // 1. reads
    const sy = scrollY; act = cl(1 - (now - lastActive - HOLD) / FADE);
    const rest = C('rest'), fanMax = C('fan');
    let wmRun = false, docRun = false, resting = false;
    if (wmAnim) {
      if (wmAnim.playState === 'finished' || wmAnim.playState === 'idle') { wmAnim = null; ignited = true; }
      else {
        wmRun = true; const p = wmAnim.effect.getComputedTiming().progress || 0, e = ease(p);
        if (e >= .85) ignited = true;
        const r = wm.getBoundingClientRect(), ec = Math.min(.767, Math.max(.233, e));
        wmAcc += C('wms') * Math.abs(ec - wmE) / .534; wmE = ec;
        const y = wmSeamY(p, r.top, r.height), s = Math.min(1, Math.max(.4, r.width / 875));
        if (e > .233 && e < .767) while (wmAcc >= 1) {
          wmAcc--; const x = r.left + U(0, 1) * r.width;
          if (Math.random() < .7) sp(x, y + U(-2, 2), U(-260, 260) * s, -U(140, 520) * s, s, U(.35, .9));
          else sp(x, y + U(-2, 2), U(-200, 200) * s, U(20, 200) * s, s, U(.35, .9));
        }
      }
    } else ignited = true;
    if (ignited && hx && heroIn && !lit) { lit = true; for (let i = 0, n = rest * .35; i < n; i++) spawnH(.25); }
    for (const s of seams) {
      const a = s.a, doc = a.timeline === document.timeline;
      if (doc && a.playState !== 'running') continue;
      if (doc) docRun = true; else if (!tourIn) continue;
      const p = a.effect.getComputedTiming().progress; if (p == null) continue;
      const e = doc ? ease(p) : p; if (e <= .02 || e >= .98) { s.last = -1; continue; }
      const t = a.effect.target, r = t.getBoundingClientRect(), y = r.top + r.height * e; if (y < 0 || y > innerHeight) continue;
      const d = s.last < 0 ? 0 : Math.abs(e - s.last); s.last = e;
      if (!doc && d < .001) { resting = true; s.acc += C('siz') * act * dt; } else s.acc += C('shot') * d / .96;
      const sc = Math.min(1, Math.max(.45, r.width / 1200)), t0s = html.classList.contains('pin') && s.idx === 4 && (doc || e > .5) ? .3 : 0;
      while (s.acc >= 1) {
        s.acc--; const u = Math.random(), L = U(.35, .9);
        if (u < .35) sp(r.left + U(0, 6), y, -U(120, 520) * sc, U(-360, 80) * sc, sc, L, t0s);
        else if (u < .7) sp(r.right - U(0, 6), y, U(120, 520) * sc, U(-360, 80) * sc, sc, L, t0s);
        else sp(r.left + U(.04, .96) * r.width, y, U(-180, 180) * sc, -U(80, 340) * sc, sc, L, t0s);
      }
    }
    if (hover && act > 0) { hoverAcc += C('hov') * dt; const r = hover.getBoundingClientRect(); while (hoverAcc >= 1) { hoverAcc--; hov(r); } }
    // 2. update
    const inHero = fine && py + sy >= geo.ht && py + sy <= geo.ht + geo.hh;
    const ap = 1 - Math.exp(-dt / .06); pvx += ((px - ppx) / dt - pvx) * ap; pvy += ((py - ppy) / dt - pvy) * ap; ppx = px; ppy = py;
    const spd = Math.hypot(pvx, pvy);
    heat = Math.min(1, (heat + (inHero ? Math.min(1, spd / 2400) * dt * 3 : 0)) * Math.exp(-dt / .8));
    if (hx && heroIn && ignited) {
      hAcc += rest / 4.2 * act * (1 + 2.5 * heat) * dt;
      const capH = rest + (fanMax - rest) * heat;
      while (hAcc >= 1) { hAcc--; if (hearth.n < capH) spawnH(0); }
    }
    const gx = px - geo.hl, gy = py + sy - geo.ht, g = Math.exp(-2.2 * dt);
    for (let i = hearth.n - 1; i >= 0; i--) {
      const p = hearth.a[i];
      p.age += dt * (1 + 3 * (1 - act));
      if (inHero) {
        const d = Math.hypot(p.x - gx, p.y - gy);
        if (d < 170) {
          const w = (1 - d / 170) ** 2;
          p.gx = Math.max(-260, Math.min(260, p.gx + pvx * w * 3.5 * dt)); p.gy = Math.max(-260, Math.min(260, p.gy + pvy * w * 3.5 * dt));
          if (w > .15 && spd > 300) p.age = Math.max(0, p.age - dt * .8 * w);
        }
      }
      if (p.age >= p.life || act === 0) { hearth.del(i); continue; }
      p.x += (p.vx + p.amp * Math.sin(TAU * p.f * p.age + p.ph) + p.gx) * dt; p.y += (p.vy + p.gy) * dt; p.vy -= 4 * dt; p.gx *= g; p.gy *= g;
    }
    const W = fc.width / dpr, H = fc.height / dpr, k = Math.exp(-1.4 * dt);
    for (let i = front.n - 1; i >= 0; i--) {
      const p = front.a[i]; p.age += dt;
      if (p.age >= p.life) { front.del(i); continue; }
      if (p.kind === 1) {
        p.vx *= k; p.vy = p.vy * k + 1100 * p.s * dt; p.x += p.vx * dt; p.y += p.vy * dt;
        if (p.y > H + 40 || p.x < -40 || p.x > W + 40) { front.del(i); continue; }
        const lt = p.age / p.life;
        if (!p.forked && lt >= .55) {
          p.forked = 1;
          if (Math.random() < p.fk) for (let j = 0; j < 3; j++) {
            const c = front.add(); if (!c) break; const a = U(0, TAU), v = U(60, 170);
            c.kind = 1; c.x = p.x; c.y = p.y; c.vx = Math.cos(a) * v; c.vy = Math.sin(a) * v; c.s = p.s; c.age = 0; c.life = U(.1, .22); c.t0 = p.t0 + (1 - p.t0) * lt; c.fk = 0; c.forked = 1;
          }
        }
      } else { p.x += p.vx * dt + p.amp * Math.sin(TAU * p.f * p.age + p.ph) * dt; p.y += p.vy * dt; p.vy -= 4 * dt; }
    }
    // 3./4. draw
    if (hx && (heroIn || drewH)) {
      hx.clearRect(0, 0, 1e4, 1e4); const sa = Math.sqrt(act);
      for (let i = 0; i < hearth.n; i++) { const p = hearth.a[i]; sprite(hx, p, sa, sy * (1 - p.z) * .35); }
      hx.globalAlpha = 1; drewH = hearth.n > 0;
    }
    if (front.n > 0 || drewF) { fx.clearRect(0, 0, 1e4, 1e4); if (front.n) drawSparks(); drewF = front.n > 0; }
    // 5. writes
    if (drewF !== on) { on = drewF; fc.classList.toggle('on', on); }
    if (hc) {
      const fl = .5 * Math.sin(TAU * .7 * now / 1e3 + phi[0]) + .3 * Math.sin(TAU * 1.9 * now / 1e3 + phi[1]) + .2 * Math.sin(TAU * 4.3 * now / 1e3 + phi[2]);
      let fan = cl(.45 + act * (.1 + .08 * fl) + .45 * heat);
      if (act === 0 && heat < .01) fan = .45;
      if (Math.abs(fan - fanV) >= .01 || (fan === .45 && fanV !== .45)) { fanV = fan; wm.style.setProperty('--fan', fan.toFixed(3)); }
    }
    ring[rn++ % 600] = performance.now() - t0; frames++;
    if ((front.n > 0 || wmRun || docRun || (heroIn && (hearth.n > 0 || act > 0 || heat > .01)) ||
        (tourIn && (now - lastScroll < 250 || (resting && act > 0))) || (hover && act > 0))) raf = requestAnimationFrame(frame);
  }
  if (debug) window.__fx = {
    state: () => ({ tier, dpr, embers: hearth.n, sparks: front.n, running: !!raf, act, fan: fanV.toFixed(3), ignited }),
    stats: () => { const a = [...ring.subarray(0, Math.min(rn, 600))].sort((x, y) => x - y); return { frames, p50: a[a.length >> 1], p95: a[Math.floor(a.length * .95)], max: a[a.length - 1] }; },
    reset: () => { rn = 0; frames = 0; },
  };
  wake();
  return () => {
    cancelAnimationFrame(raf); raf = 0; ac.abort(); ios.forEach(o => o.disconnect()); rsz.disconnect(); geoRo.disconnect();
    hc && hc.remove(); fc.remove(); wm.style.removeProperty('--fan'); delete window.__fx;
  };
}
