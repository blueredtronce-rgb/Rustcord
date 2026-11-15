import { startFx } from './fx.js';
import { SYSTEMS, LATEST, detectOs, fetchLatest, buttonState, mb } from './download.js';

const html = document.documentElement, q = new URLSearchParams(location.search);
html.classList.add('ready');
if (window.rcSyncMotion) window.rcSyncMotion();
const $$ = (s, r = document) => [...r.querySelectorAll(s)];

// ---- download ----
const uad = navigator.userAgentData;
const os = detectOs({ force: q.get('os'), ua: navigator.userAgent, platform: (uad && uad.platform) || navigator.platform,
  mobile: !!(uad && uad.mobile), touch: navigator.maxTouchPoints });
const ALT = { deb: ['tar', 'Other Linux: .tar.gz for any distribution'], tar: ['deb', '.deb for Debian, Ubuntu and Mint'] };
const FIRST = {
  windows: () => 'First start: if Windows SmartScreen says “Windows protected your PC”, select More info, then Run anyway. The setup isn’t code-signed.',
  mac: () => 'First start: macOS blocks Rustcord once because Apple hasn’t notarized it. Open it, then click Open Anyway in System Settings, Privacy & Security.',
  deb: v => `Then install it from the folder you saved it to: sudo apt install ./rustcord_${v}_amd64.deb`,
  tar: () => 'Then unpack it and run install.sh; the commands are under Install on Linux.',
};
const set = (el, p, v) => { if (el) el[p] = v; };

function paint(rel, failed) {
  const st = buttonState(os, rel, failed), v = rel ? rel.v : '<version>', mobile = os === 'mobile';
  $$('[data-dl]').forEach(sc => {
    const btn = sc.querySelector('[data-dl-button]'), alt = sc.querySelector('[data-dl-alt]'), other = sc.querySelector('[data-dl-other]');
    const meta = sc.querySelector('[data-dl-meta]'), first = sc.querySelector('[data-dl-first]');
    btn.hidden = !!st.hidden; if (meta) meta.hidden = !!st.hidden;
    btn.querySelector('use').setAttribute('href', os === 'unknown' ? '#i-chev' : '#i-dl'); btn.querySelector('.icon').classList.toggle('icon-dl', os !== 'unknown');
    sc.querySelector('[data-dl-mobile]').hidden = !mobile;
    if (!st.hidden) { btn.href = st.href; sc.querySelector('[data-dl-label]').textContent = st.label; set(meta, 'textContent', st.meta); }
    set(sc.querySelector('[data-dl-title]'), 'textContent', mobile ? 'Rustcord is a desktop app' : (st.title || 'Get Rustcord'));
    if (other) { other.hidden = os === 'unknown'; other.textContent = mobile ? 'All systems' : 'Other systems'; }
    if (alt) {
      const a = ALT[os], f = a && rel && rel.files[a[0]];
      alt.hidden = !a; if (a) { alt.textContent = a[1]; alt.href = f ? f.url : (alt.closest('.dl-panel') ? '#systems' : '#download'); }
    }
    if (first) {
      const ok = rel && !failed && rel.files[os] && FIRST[os];
      first.hidden = !ok; if (ok) first.textContent = FIRST[os](v);
    }
  });
  $$('[data-row]').forEach(a => {
    const k = a.dataset.row, s = SYSTEMS[k], f = rel && rel.files[k];
    a.href = f ? f.url : LATEST;
    a.querySelector('[data-file]').textContent = f ? f.name : s.pattern;
    a.querySelector('[data-size]').textContent = f && f.size ? mb(f.size) : '';
    a.querySelector('[data-row-mine]').hidden = k !== os;
  });
  const mine = os in SYSTEMS ? SYSTEMS[os].notes : null;
  $$('[data-notes]').forEach(d => { if (d.dataset.notes === mine && !d.dataset.touched) d.open = true; });
  $$('[data-v]').forEach(c => { c.textContent = c.dataset.v.replace(/\{v\}/g, v); });
}
$$('[data-notes]').forEach(d => d.addEventListener('toggle', () => { d.dataset.touched = '1'; }, { once: true }));
paint(null, false);
fetchLatest(fetch, q.get('api') === 'fail').then(r => paint(r, false), () => paint(null, true));

// ---- copy buttons ----
function copy(btn, text, done) {
  const span = btn.querySelector('span'), label = btn.dataset.label || (btn.dataset.label = span.textContent);
  const show = (msg, ok) => { span.textContent = msg; btn.classList.toggle('is-done', ok); clearTimeout(btn._t);
    btn._t = setTimeout(() => { span.textContent = label; btn.classList.remove('is-done'); }, 1800); };
  (navigator.clipboard ? navigator.clipboard.writeText(text) : Promise.reject()).then(() => show(done, true), () => show('Couldn’t copy. Select the text instead.', false));
}
$$('[data-copy-link]').forEach(b => b.addEventListener('click', () => copy(b, location.origin + location.pathname, 'Link copied')));
$$('[data-copy]').forEach(b => b.addEventListener('click', () => copy(b, b.parentElement.querySelector('code').textContent, 'Copied')));

// ---- 199 ms bar ----
const boot = document.querySelector('[data-boot]');
if (boot && html.classList.contains('motion')) {
  const run = () => { boot.classList.remove('run'); void boot.offsetWidth; boot.classList.add('run'); };
  const rb = boot.querySelector('[data-boot-replay]'); rb.hidden = false; rb.addEventListener('click', run);
  const io = new IntersectionObserver(([e]) => { if (e.isIntersecting) { run(); io.disconnect(); } }, { threshold: 0.6 });
  io.observe(boot);
}

// ---- first shot: the load sweep plays when it is first seen ----
const first = document.querySelector('.tour-item:first-child .tour-shot');
if (first) new IntersectionObserver(([e], o) => { if (e.isIntersecting) { first.classList.add('lit'); o.disconnect(); } }, { threshold: 0.5 }).observe(first);

// ---- tour ----
const tour = document.querySelector('[data-tour]');
if (tour && !html.classList.contains('sda')) {
  const items = $$('.tour-item', tour);
  const headerH = () => parseFloat(getComputedStyle(html).getPropertyValue('--header-h')) || 64;
  let step = -1, raf = 0;
  const setStep = s => {
    if (s === step) return;
    const forward = step !== -1 && s > step; step = s; tour.dataset.step = s;
    items.forEach((li, i) => {
      li.classList.toggle('is-shown', i <= s); li.classList.toggle('is-current', i === s);
      if (forward && i === s) { li.classList.remove('forge'); void li.offsetWidth; li.classList.add('forge'); }
    });
  };
  const measure = () => {
    raf = 0; const r = tour.getBoundingClientRect(), h = headerH();
    const p = Math.min(1, Math.max(0, (h - r.top) / (r.height - (innerHeight - h))));
    setStep(Math.min(items.length - 1, Math.floor(p * items.length)));
  };
  const onScroll = () => { raf = raf || requestAnimationFrame(measure); };
  new IntersectionObserver(([e]) => {
    if (e.isIntersecting) { addEventListener('scroll', onScroll, { passive: true }); onScroll(); }
    else removeEventListener('scroll', onScroll);
  }).observe(tour);
  setStep(0);
  const io = new IntersectionObserver(es => es.forEach(e => {
    if (e.isIntersecting) { e.target.classList.add('is-in'); io.unobserve(e.target); } }), { threshold: 0.35 });
  $$('.tour-shot', tour).forEach(el => io.observe(el));
}

// ---- test hook: ?tour=0..4 lands mid-hold for that step when pinned ----
const tk = q.get('tour');
if (tk !== null && tour && html.classList.contains('pin')) {
  const k = Math.min(4, Math.max(0, +tk || 0));
  const place = () => { const top = tour.getBoundingClientRect().top + scrollY, len = tour.offsetHeight - (innerHeight - parseFloat(getComputedStyle(html).getPropertyValue('--header-h')));
    scrollTo({ top: top - (parseFloat(getComputedStyle(html).getPropertyValue('--header-h')) || 64) + len * (k + .5) / 5, behavior: 'instant' }); };
  addEventListener('load', () => { place(); setTimeout(place, 300); });
}

startFx();
