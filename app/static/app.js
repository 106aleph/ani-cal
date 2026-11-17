'use strict';
/* ============================================================ utils */
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];
const pad = n => String(n).padStart(2, '0');
const WD = ['日', '月', '火', '水', '木', '金', '土'];
const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const ymd = d => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
const epNum = n => (n === null || n === undefined) ? '' : `#${Number(n)}`;

const store = {
  get(k, d) { try { const v = localStorage.getItem(k); return v === null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem(k, JSON.stringify(v)); } catch { /* ignore */ } },
};

async function api(path, { method = 'GET', body } = {}) {
  const r = await fetch(path, {
    method, headers: body ? { 'Content-Type': 'application/json' } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!r.ok) {
    let msg = r.statusText;
    try { const j = await r.json(); msg = typeof j.detail === 'string' ? j.detail : JSON.stringify(j.detail); } catch { /* */ }
    throw new Error(msg);
  }
  return r.status === 204 ? null : r.json();
}

function toast(msg, isErr = false) {
  const t = $('#toast');
  t.textContent = msg;
  t.className = 'show' + (isErr ? ' err' : '');
  clearTimeout(toast._t);
  toast._t = setTimeout(() => (t.className = ''), 3500);
}
const guard = fn => async (...a) => { try { await fn(...a); } catch (e) { toast(e.message, true); } };

function hue(id) { return (Number(id) * 137.508) % 360; }

/* 'YYYY-MM-DDTHH:MM' → 表示用 {key, time, date} */
const state = {
  tab: store.get('tab', 'calendar'),
  view: store.get('view', 'month'),
  late: store.get('late', true),
  hideWatched: store.get('hideWatched', false),
  cursor: new Date(),
  catalog: null,
  openTracks: new Set(store.get('openTracks', [])),
};

function disp(s) {
  const [d, t] = s.split('T');
  const [Y, M, D] = d.split('-').map(Number);
  const [h, m] = t.split(':').map(Number);
  const dt = new Date(Y, M - 1, D);
  let hh = h;
  if (state.late && h < 5) { dt.setDate(dt.getDate() - 1); hh = h + 24; }
  return { key: ymd(dt), time: `${pad(hh)}:${pad(m)}`, date: dt };
}
function fmtFull(s) {
  if (!s) return '未定';
  const p = disp(s);
  return `${p.date.getMonth() + 1}/${p.date.getDate()}(${WD[p.date.getDay()]}) ${p.time}`;
}

/* ============================================================ dialog */
function formDialog({ title, fields = [], submit = '保存', extra = [], html = '', setup = null }) {
  return new Promise(resolve => {
    const dlg = document.createElement('dialog');
    const fieldHtml = fields.map(f => {
      const id = 'f_' + f.name;
      const req = f.required ? 'required' : '';
      let input;
      if (f.type === 'checkbox') {
        return `<label class="check field"><input type="checkbox" id="${id}" name="${f.name}" ${f.value ? 'checked' : ''}> ${esc(f.label)}</label>`;
      } else if (f.type === 'select') {
        input = `<select id="${id}" name="${f.name}">${f.options.map(o =>
          `<option value="${esc(o.value)}" ${String(o.value) === String(f.value ?? '') ? 'selected' : ''}>${esc(o.label)}</option>`).join('')}</select>`;
      } else if (f.type === 'textarea') {
        input = `<textarea id="${id}" name="${f.name}" rows="3" placeholder="${esc(f.placeholder ?? '')}">${esc(f.value ?? '')}</textarea>`;
      } else {
        input = `<input id="${id}" name="${f.name}" type="${f.type || 'text'}" value="${esc(f.value ?? '')}"
          placeholder="${esc(f.placeholder ?? '')}" ${f.step ? `step="${f.step}"` : ''} ${f.min !== undefined ? `min="${f.min}"` : ''} ${req}>`;
      }
      return `<label class="field"><span>${esc(f.label)}</span>${input}${f.help ? `<small>${esc(f.help)}</small>` : ''}</label>`;
    }).join('');
    dlg.innerHTML = `<form method="dialog">
      <h3>${esc(title)}</h3>${html}<div class="fields">${fieldHtml}</div>
      <menu>
        ${extra.map(b => `<button value="${b.value}" class="${b.cls || ''}" formnovalidate>${esc(b.label)}</button>`).join('')}
        <span class="spacer"></span>
        <button value="cancel" formnovalidate>キャンセル</button>
        <button value="ok" class="primary">${esc(submit)}</button>
      </menu></form>`;
    dlg.addEventListener('close', () => {
      const action = dlg.returnValue || 'cancel';
      const values = {};
      for (const f of fields) {
        const el = dlg.querySelector('#f_' + f.name);
        if (f.type === 'checkbox') values[f.name] = el.checked;
        else if (f.type === 'number') values[f.name] = el.value === '' ? null : Number(el.value);
        else values[f.name] = el.value;
      }
      dlg.remove();
      resolve({ action, values });
    });
    document.body.append(dlg);
    if (setup) setup(dlg);
    dlg.showModal();
  });
}
const confirmDialog = async (msg, ok = 'OK') =>
  (await formDialog({ title: msg, submit: ok })).action === 'ok';

/* ============================================================ 通知（Web Push） */
const pushSupported = () => 'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window;
const b64ToU8 = s => {
  const b = atob((s + '='.repeat((4 - s.length % 4) % 4)).replace(/-/g, '+').replace(/_/g, '/'));
  return Uint8Array.from(b, c => c.charCodeAt(0));
};
async function pushReg() {
  await navigator.serviceWorker.register('/sw.js');
  return navigator.serviceWorker.ready;
}

async function notifySettings() {
  const note = (h) => formDialog({ title: '通知', submit: 'OK', html: h });
  if (!pushSupported()) {
    await note(`<p class="muted small">このブラウザは通知に対応していません。iPhone / iPad では、共有メニューの「ホーム画面に追加」でインストールしたアプリ（iOS 16.4 以降）から設定してください。また、通知には HTTPS（tailscale serve）での接続が必要です。</p>`);
    return;
  }
  if (Notification.permission === 'denied') {
    await note('<p class="muted small">通知がブラウザで拒否されています。ブラウザ（またはOS）のサイト設定で通知を許可してから、もう一度開いてください。</p>');
    return;
  }
  const reg = await pushReg();
  let sub = await reg.pushManager.getSubscription();
  let st = sub ? await api('/api/push/settings?endpoint=' + encodeURIComponent(sub.endpoint)) : { subscribed: false, lead_min: 10 };
  if (sub && !st.subscribed) { await sub.unsubscribe(); sub = null; } // サーバー側で購読が消えている
  const on = !!sub;
  const r = await formDialog({
    title: '通知', submit: on ? '保存' : '通知を有効にする',
    html: `<p class="muted small">カレンダーに登録した作品（視聴済みでない話）の配信開始の少し前に、この端末へ通知します。<strong>この端末の通知は現在 ${on ? 'オン' : 'オフ'}</strong>です。設定は端末ごとです。アプリを開いていなくても届きます。</p>`,
    fields: [{ name: 'lead', label: '何分前に通知するか', type: 'number', min: 0, step: 1, value: st.lead_min, required: true,
      help: '0 で開始時刻ちょうど。最大 1440（24時間前）。' }],
    extra: on ? [{ label: 'テスト通知', value: 'test' }, { label: '通知を止める', value: 'off', cls: 'danger' }] : [],
  });
  if (r.action === 'cancel') return;
  if (r.action === 'off') {
    await api('/api/push/unsubscribe', { method: 'POST', body: { endpoint: sub.endpoint } });
    await sub.unsubscribe();
    toast('この端末の通知を止めました');
    return;
  }
  if (r.action === 'test') {
    await api('/api/push/test', { method: 'POST', body: { endpoint: sub.endpoint } });
    toast('テスト通知を送りました');
    return;
  }
  const lead = Math.round(r.values.lead);
  if (!(lead >= 0 && lead <= 1440)) throw new Error('0〜1440 の分数を入力してください');
  if (on) {
    await api('/api/push/settings', { method: 'POST', body: { endpoint: sub.endpoint, lead_min: lead } });
    toast(`開始の${lead}分前に通知します`);
    return;
  }
  if (await Notification.requestPermission() !== 'granted') throw new Error('通知が許可されませんでした');
  const { key } = await api('/api/push/key');
  sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: b64ToU8(key) });
  const j = sub.toJSON();
  try {
    await api('/api/push/subscribe', { method: 'POST', body: { endpoint: j.endpoint, keys: j.keys, lead_min: lead } });
  } catch (e) { await sub.unsubscribe(); throw e; }
  toast(`通知を有効にしました（開始の${lead}分前）`);
}

/* ============================================================ episode edit */
const SRC_LABEL = { sync: '取得', manual: '手動', generated: '予測' };
function badges(e) {
  let b = `<span class="badge src-${e.source}">${SRC_LABEL[e.source] || e.source}</span>`;
  if (e.edited && e.source !== 'manual') b += '<span class="badge edited">修正済</span>';
  if (e.hidden) b += '<span class="badge hidden">非表示</span>';
  return b;
}

async function editEpisode(e, title = '') {
  const orig = (e.pid && e.orig_start_at) ?
    `<p class="muted small">取得値: ${esc(fmtFull(e.orig_start_at))} ${esc(epNum(e.orig_number))} ${esc(e.orig_subtitle || '')}</p>` : '';
  const extra = [{ label: e.hidden ? '復元' : '削除', value: e.hidden ? 'restore' : 'delete', cls: e.hidden ? '' : 'danger' }];
  if (e.pid && e.edited) extra.push({ label: '取得値に戻す', value: 'revert' });
  const r = await formDialog({
    title: `${title} ${epNum(e.number)}`.trim(),
    html: `<p>${badges(e)}</p>${orig}`,
    fields: [
      { name: 'number', label: '話数', type: 'number', step: '0.5', value: e.number },
      { name: 'subtitle', label: 'サブタイトル', value: e.subtitle },
      { name: 'start_at', label: '配信開始日時（実時刻）', type: 'datetime-local', value: e.start_at },
      { name: 'watched', label: '視聴済み', type: 'checkbox', value: e.watched },
    ],
    extra,
  });
  if (r.action === 'ok') {
    await api(`/api/episodes/${e.id}`, { method: 'PATCH', body: r.values });
    toast('保存しました');
  } else if (r.action === 'delete') {
    await api(`/api/episodes/${e.id}`, { method: 'DELETE' }); toast('削除しました');
  } else if (r.action === 'restore') {
    await api(`/api/episodes/${e.id}`, { method: 'PATCH', body: { hidden: false } }); toast('復元しました');
  } else if (r.action === 'revert') {
    await api(`/api/episodes/${e.id}/revert`, { method: 'POST' }); toast('取得値に戻しました');
  } else return false;
  return true;
}

async function toggleWatched(id, watched) {
  await api(`/api/episodes/${id}`, { method: 'PATCH', body: { watched } });
}

/* ============================================================ calendar */
function calRange() {
  const c = state.cursor;
  if (state.view === 'month') {
    const first = new Date(c.getFullYear(), c.getMonth(), 1);
    const start = new Date(first); start.setDate(1 - first.getDay());
    const last = new Date(c.getFullYear(), c.getMonth() + 1, 0);
    const end = new Date(last); end.setDate(last.getDate() + (6 - last.getDay()) + 1);
    return { start, end };
  }
  if (state.view === 'list') { // リストは月ごと（その月の1日〜末日）
    const start = new Date(c.getFullYear(), c.getMonth(), 1);
    return { start, end: new Date(c.getFullYear(), c.getMonth() + 1, 1) };
  }
  if (state.view === 'week') {
    const start = new Date(c.getFullYear(), c.getMonth(), c.getDate() - c.getDay());
    const end = new Date(start); end.setDate(end.getDate() + 7);
    return { start, end };
  }
}

function evHtml(e) {
  const p = disp(e.start_at);
  return `<div class="ev ${e.watched ? 'done' : ''}" style="--h:${hue(e.track_id)}" data-id="${e.id}" title="${esc(e.platform)} ${esc(e.subtitle || '')}">
    <input type="checkbox" class="w" ${e.watched ? 'checked' : ''} aria-label="視聴済み">
    <span class="time">${p.time}</span><span class="t">${esc(e.title)}</span><span class="ep">${epNum(e.number)}</span>
  </div>`;
}

const narrowMq = window.matchMedia('(max-width: 720px)');
const isNarrow = () => narrowMq.matches;

/* 要素の上端から画面の下端までの高さ（下余白つき）。ページがスクロール済みでも文書上の位置で計算する */
function availHeight(el, reserve = 0) {
  const top = el.getBoundingClientRect().top + window.scrollY;
  return Math.max(0, Math.floor(window.innerHeight - top - 56 - reserve));
}

let calEvents = [];
async function renderCalendar() {
  const { start, end } = calRange();
  const fetchEnd = new Date(end); fetchEnd.setDate(fetchEnd.getDate() + 1); // 24時以降表記で前日に寄る分
  const fetchStart = new Date(start); fetchStart.setDate(fetchStart.getDate() - 1); // 24時以降表記で前日に寄る分（月初の深夜枠）
  let evs = await api(`/api/calendar?start=${ymd(fetchStart)}&end=${ymd(fetchEnd)}`);
  if (state.hideWatched) evs = evs.filter(e => !e.watched);
  calEvents = evs;
  state.nowTick = null;
  const byDay = {};
  for (const e of evs) (byDay[disp(e.start_at).key] ||= []).push(e);
  for (const k in byDay) byDay[k].sort((a, b) => disp(a.start_at).time.localeCompare(disp(b.start_at).time));
  const today = ymd(new Date());

  $('#cal-month').hidden = state.view !== 'month';
  const weekGrid = state.view === 'week';
  const narrow = isNarrow();
  $('#cal-week').hidden = !weekGrid;
  $('#cal-list').hidden = state.view !== 'list';
  $('#cal-day').hidden = !(state.view === 'month' && narrow);
  $$('.seg [data-view]').forEach(b => b.classList.toggle('active', b.dataset.view === state.view));

  if (state.view === 'month') {
    const c = state.cursor;
    $('#cal-label').textContent = `${c.getFullYear()}年${c.getMonth() + 1}月`;
    // スマホ: 選択中の日（既定は今日）の予定を、月グリッドの下に詳しく出す
    const inMonth = k => k.slice(0, 7) === ymd(new Date(c.getFullYear(), c.getMonth(), 1)).slice(0, 7);
    let sel = state.selDay && inMonth(state.selDay) ? state.selDay : (inMonth(today) ? today : ymd(new Date(c.getFullYear(), c.getMonth(), 1)));
    state.selDay = sel;
    const MAXCHIPS = 3;
    let h = '<div class="month">' + WD.map((w, i) => `<div class="wd wd${i}">${w}</div>`).join('');
    for (let d = new Date(start); d < end; d.setDate(d.getDate() + 1)) {
      const k = ymd(d);
      const out = d.getMonth() !== c.getMonth();
      const list = byDay[k] || [];
      const shown = narrow ? list.slice(0, MAXCHIPS) : list;
      h += `<div class="day ${out ? 'out' : ''} ${k === today ? 'today' : ''} ${narrow && k === sel ? 'sel' : ''} wd${d.getDay()}" data-date="${k}">
        <div class="dn">${d.getDate()}</div>${shown.map(evHtml).join('')}${narrow && list.length > MAXCHIPS ? `<div class="more">+${list.length - MAXCHIPS}</div>` : ''}</div>`;
    }
    if (narrow) {
      const [Y, M, D] = sel.split('-').map(Number), sd = new Date(Y, M - 1, D), list = byDay[sel] || [];
      $('#cal-day').innerHTML = `<h3 class="dayhead ${sel === today ? 'today' : ''}">${sd.getMonth() + 1}/${sd.getDate()}（${WD[sd.getDay()]}）${sel === today ? ' ・今日' : ''} <span class="muted small">${list.length}件</span></h3>`
        + (list.length ? `<div class="evlist">${list.map(evHtml).join('')}</div>` : '<p class="muted small empty">予定なし</p>');
    }
    $('#cal-month').innerHTML = h + '</div>';
    // 広い画面では、月グリッドを画面の下端まで伸ばす（縦向きモニターで下が空かないように）
    if (!narrow) $('#cal-month .month').style.minHeight = availHeight($('#cal-month')) + 'px';
  } else if (weekGrid) {
    const last = new Date(end - 864e5);
    $('#cal-label').textContent = `${start.getFullYear()}/${start.getMonth() + 1}/${start.getDate()} 〜 ${last.getMonth() + 1}/${last.getDate()}`;
    const DUR = 30; // 1話の表示長(分)
    let PX = narrow ? 2.4 : 1.6; // 1分あたりpx（スマホは文字が読めるよう高く。広い画面は下で画面の高さに合わせる）
    const mins = e => { const [h, m] = disp(e.start_at).time.split(':').map(Number); return h * 60 + m; };
    const wk = [];
    for (let d = new Date(start); d < end; d.setDate(d.getDate() + 1)) wk.push(new Date(d));
    const inWeek = wk.flatMap(d => byDay[ymd(d)] || []);
    // 配信のある時間（前後1時間つき）だけを行にして詰める。間が空く所は区切り線で示す
    const hrs = new Set();
    const lo = state.late ? 5 : 0, hi = state.late ? 29 : 23;
    for (const e of inWeek) {
      const s0 = mins(e);
      for (let hh = Math.floor(s0 / 60) - 1; hh <= Math.floor((s0 + DUR - 1) / 60) + 1; hh++) if (hh >= lo && hh <= hi) hrs.add(hh);
    }
    // 現在時刻（深夜表記ONなら 25:10 のように、日付も表示上の日に寄せる）
    const nowInfo = () => {
      const n = new Date(), p = disp(`${ymd(n)}T${pad(n.getHours())}:${pad(n.getMinutes())}`), [h, m] = p.time.split(':').map(Number);
      return { key: p.key, min: h * 60 + m, label: p.time };
    };
    const nd = nowInfo();
    const nowInWeek = wk.some(d => ymd(d) === nd.key);
    if (nowInWeek && Math.floor(nd.min / 60) >= lo && Math.floor(nd.min / 60) <= hi) hrs.add(Math.floor(nd.min / 60)); // 現在の時間帯の行は常に出す
    if (!hrs.size) for (let hh = 19; hh <= 25; hh++) if (hh >= lo && hh <= hi) hrs.add(hh);
    const rows = [...hrs].sort((x, y) => x - y), rowOf = new Map(rows.map((hh, i) => [hh, i]));
    if (!narrow) { // 画面の高さに合わせて時間軸を伸縮（最小は従来の高さ）。見出し行ぶんを引く
      PX = Math.min(4.2, Math.max(1.6, availHeight($('#cal-week'), 34) / (rows.length * 60)));
    }
    const H = rows.length * 60 * PX;
    const yOf = m => (rowOf.get(Math.floor(m / 60)) * 60 + m % 60) * PX;
    const axis = rows.map((hh, i) => `<div class="hl ${i && rows[i - 1] !== hh - 1 ? 'gap' : ''}" style="top:${i * 60 * PX}px">${pad(hh)}:00</div>`).join('');
    const nowTop = n => (rowOf.has(Math.floor(n.min / 60)) ? yOf(n.min) : null);
    const nt = nowTop(nd);
    const nowLabel = nowInWeek && nt !== null ? `<div class="nowlabel" style="top:${nt}px">${nd.label}</div>` : '';
    let head = '<div class="corner"></div>', cols = `<div class="axis" style="height:${H}px">${axis}${nowLabel}</div>`;
    for (const d of wk) {
      const k = ymd(d), list = byDay[k] || [];
      head += `<div class="wd wd${d.getDay()} ${k === nd.key ? 'today' : ''}">${d.getMonth() + 1}/${d.getDate()}（${WD[d.getDay()]}）</div>`;
      // 重なる予定は横に並べる
      const items = list.map(e => ({ e, s: mins(e) })).sort((x, y) => x.s - y.s);
      let cluster = [], clusterEnd = -1;
      const flush = () => { const n = Math.max(0, ...cluster.map(c => c.lane + 1)); cluster.forEach(c => c.n = n); cluster = []; };
      for (const it of items) {
        if (it.s >= clusterEnd && cluster.length) flush();
        const used = new Set(cluster.filter(c => c.s + DUR > it.s).map(c => c.lane));
        it.lane = 0; while (used.has(it.lane)) it.lane++;
        cluster.push(it); clusterEnd = Math.max(clusterEnd < 0 ? 0 : clusterEnd, it.s + DUR);
      }
      if (cluster.length) flush();
      const line = nowInWeek && nt !== null ? `<div class="nowline ${k === nd.key ? 'on' : ''}" style="top:${nt}px"></div>` : '';
      cols += `<div class="dcol ${k === nd.key ? 'today' : ''}" data-key="${k}" style="height:${H}px">` + line + items.map(it => {
        const top = yOf(it.s), w = 100 / it.n;
        return evHtml(it.e).replace('class="ev ', `class="ev blk ${it.n >= 3 ? 'tight' : ''} `)
          .replace('data-id', `style="top:${top}px;height:${DUR * PX - 2}px;left:calc(${it.lane * w}% + 1px);width:calc(${w}% - 2px);--h:${hue(it.e.track_id)}" data-id`)
          .replace(/style="--h:[^"]*" (style=)/, '$1');
      }).join('') + '</div>';
    }
    $('#cal-week').innerHTML = `<div class="tgrid" style="--hour:${60 * PX}px"><div class="thead">${head}</div><div class="tbody">${cols}</div></div>`;
    // 1分ごとに、再描画せず現在時刻の線だけ動かす（時間帯の行が無くなったときだけ描き直す）
    state.nowTick = () => {
      const n = nowInfo();
      if (!wk.some(d => ymd(d) === n.key)) { renderCalendar().catch(() => { }); return; }
      const top = nowTop(n);
      if (top === null) { renderCalendar().catch(() => { }); return; }
      $$('#cal-week .nowline').forEach(el => { el.style.top = top + 'px'; el.classList.toggle('on', el.parentElement.dataset.key === n.key); });
      $$('#cal-week .dcol').forEach(el => el.classList.toggle('today', el.dataset.key === n.key));
      const lab = $('#cal-week .nowlabel');
      if (lab) { lab.style.top = top + 'px'; lab.textContent = n.label; }
    };
    if (narrow) { // 今日（無ければ週の頭）の列が見える位置までスクロール
      const box = $('#cal-week'), col = $('.thead .wd.today', box) || $('.thead .wd', box);
      const left = col.getBoundingClientRect().left - box.getBoundingClientRect().left + box.scrollLeft;
      box.scrollLeft = Math.max(0, left - $('.thead .corner', box).offsetWidth);
    }
  } else {
    $('#cal-label').textContent = `${start.getFullYear()}年${start.getMonth() + 1}月`;
    // 配信済みの未視聴は、今月を表示しているときだけ先頭に出す
    const isCurMonth = ymd(start).slice(0, 7) === today.slice(0, 7);
    const backlog = isCurMonth ? (await api('/api/backlog')).filter(e => disp(e.start_at).key < ymd(start)) : [];
    let h = '';
    if (backlog.length) {
      h += `<h3 class="dayhead">未視聴（配信済み） <span class="muted">${backlog.length}件</span></h3><div class="evlist">${backlog.map(e =>
        evHtml(e).replace('<span class="time">', `<span class="date">${fmtFull(e.start_at).split(' ')[0]}</span><span class="time">`)).join('')}</div>`;
      calEvents = calEvents.concat(backlog);
    }
    // 予定のある日だけ表示（今日は予定が無くても出す）
    let any = false;
    for (let d = new Date(start); d < end; d.setDate(d.getDate() + 1)) {
      const k = ymd(d);
      const list = byDay[k] || [];
      if (!list.length && k !== today) continue;
      any = any || list.length > 0;
      h += `<h3 class="dayhead ${k === today ? 'today' : ''}">${d.getMonth() + 1}/${d.getDate()}（${WD[d.getDay()]}）${k === today ? ' ・今日' : ''}${list.length ? ` <span class="muted small">${list.length}件</span>` : ''}</h3>`;
      h += list.length ? `<div class="evlist">${list.map(evHtml).join('')}</div>` : '<p class="muted small empty">予定なし</p>';
    }
    if (!any) h += '<p class="empty">この月の予定はありません。</p>';
    $('#cal-list').innerHTML = h;
  }
}

function bindEventClicks(root, reload) {
  root.addEventListener('click', guard(async ev => {
    const el = ev.target.closest('.ev');
    if (!el) return;
    const e = calEvents.find(x => String(x.id) === el.dataset.id);
    if (!e) return;
    if (ev.target.classList.contains('w')) {
      await toggleWatched(e.id, ev.target.checked);
      await reload();
      return;
    }
    if (await editEpisode(e, e.title)) await reload();
  }));
}

/* ============================================================ catalog */
async function loadCatalog(force = false) {
  if (!state.catalog || force) state.catalog = await api('/api/catalog');
  const c = state.catalog;
  const ss = $('#cat-season');
  const prevS = ss.value || store.get('catSeason', String(c.current_season_key));
  ss.innerHTML = '<option value="">すべての時期</option>' + c.seasons.map(s => `<option value="${s.key}">${esc(s.label)}</option>`).join('');
  ss.value = c.seasons.some(s => String(s.key) === prevS) ? prevS : (c.seasons[0] ? String(c.seasons[0].key) : '');
  const ps = $('#cat-platform');
  const prevP = ps.value || store.get('catPlatform', '');
  ps.innerHTML = '<option value="">すべての媒体</option>' + c.platforms.map(p =>
    `<option value="${esc(p.name)}">${p.is_net ? '🌐 ' : ''}${esc(p.name)}（${p.titles}）</option>`).join('');
  ps.value = c.platforms.some(p => p.name === prevP) ? prevP : '';
  renderCatalog();
}

function renderCatalog() {
  const c = state.catalog;
  if (!c) return;
  const season = $('#cat-season').value, plat = $('#cat-platform').value;
  const net = $('#cat-net').checked, untracked = $('#cat-untracked').checked;
  const q = $('#cat-q').value.trim().toLowerCase();
  store.set('catSeason', season); store.set('catPlatform', plat);
  const items = c.items.filter(it =>
    (!season || String(it.season_key) === season) &&
    (!q || it.title.toLowerCase().includes(q)) &&
    (!plat || it.channels.some(ch => ch.name === plat)) &&
    (!untracked || !it.channels.some(ch => ch.track_id)));
  if (!c.items.length) {
    $('#cat-list').innerHTML = '<p class="empty">データがありません。右上の「今すぐ取得」を押してください（初回は数分かかります）。</p>';
    return;
  }
  $('#cat-list').innerHTML = `<p class="muted small">${items.length} 作品</p>` + items.map(it => {
    let chs = it.channels.filter(ch => (!net || ch.is_net));
    if (plat) chs.sort((a, b) => (b.name === plat) - (a.name === plat));
    return `<div class="work">
      <div class="wt"><strong>${esc(it.title)}</strong> <span class="muted small">${esc(it.season || '')}</span></div>
      <div class="chips">${chs.map(ch => `<button class="chip ${ch.is_net ? 'net' : ''} ${ch.track_id ? 'on' : ''} ${plat && ch.name === plat ? 'hl' : ''}"
          data-tid="${it.tid}" data-chid="${ch.chid}" data-track="${ch.track_id || ''}" data-archived="${ch.track_archived ? 1 : 0}"
          title="${esc(ch.group || '')}${ch.has_ep1 ? '' : '（第1話の日時は未取得）'}${ch.track_archived ? '（視聴済み）' : ''}">
          ${ch.track_id ? '✓ ' : ''}${esc(ch.name)} <span class="when">${esc(fmtFull(ch.first_start))}${ch.has_ep1 ? '〜' : '頃〜'}</span></button>`).join('')
        || '<span class="muted small">（条件に合う媒体なし）</span>'}</div></div>`;
  }).join('');
}

/* ============================================================ tracks */
const allWatched = t => t.ep_total > 0 && t.ep_watched === t.ep_total;
const trackMeta = t => t.archived
  ? `<span class="muted small">全${t.ep_total}話 視聴済み ・ ${esc((t.archived_at || '').slice(0, 10))} に移動</span>`
  : `${t.ep_watched}/${t.ep_total} 視聴
          ${t.backlog ? `<span class="badge backlog">未視聴 ${t.backlog}</span>` : ''}
          <span class="muted small">次回 ${esc(fmtFull(t.next_start))}</span>`;
const archiveBtnTitle = t => allWatched(t) ? '全話視聴済みの作品を「視聴済み」へ移します' : '全話を視聴済みにすると押せます';

/* 一覧全体を作り直さず、指定作品の件数表示とエピソード表だけ更新する（開閉状態・スクロールを保つ） */
async function refreshTrack(id) {
  const det = $(`details.track[data-id="${id}"]`);
  const tl = det.closest('.track-root'), archived = tl.dataset.archived === '1';
  const tracks = await api(`/api/tracks?archived=${archived}`);
  tl._tracks = tracks;
  const t = tracks.find(x => x.id === id);
  if (t) {
    $('summary .meta', det).innerHTML = trackMeta(t);
    const ab = $('button[data-act="archive"]', det); // 「視聴済みに移す」は全話視聴済みのときだけ押せる
    if (ab) { ab.disabled = !allWatched(t); ab.title = archiveBtnTitle(t); }
  }
  await loadEpisodes(id);
}

async function renderTracks(archived = false) {
  const root = $(archived ? '#archive-list' : '#track-list');
  const tracks = await api(`/api/tracks?archived=${archived}`);
  root._tracks = tracks;
  if (!tracks.length) {
    root.innerHTML = archived
      ? '<p class="empty">視聴済みの作品はまだありません。「登録作品」で全話を視聴済みにして「視聴済みに移す」を押すと、ここに表示されます。</p>'
      : '<p class="empty">登録作品はまだありません。「新作一覧」で媒体を選んで登録してください。</p>';
    return;
  }
  root.innerHTML = tracks.map(t => `
    <details class="track" data-id="${t.id}" ${state.openTracks.has(t.id) ? 'open' : ''} style="--h:${hue(t.id)}">
      <summary>
        <span class="bar"></span>
        <span class="tt"><strong>${esc(t.display_title)}</strong>
          <span class="badge plat">${esc(t.display_platform || '媒体未設定')}</span>
          ${t.tid ? '' : '<span class="badge src-manual">手動登録</span>'}
          ${t.auto_sync || !t.tid ? '' : '<span class="badge hidden">自動取得OFF</span>'}</span>
        <span class="meta">${trackMeta(t)}</span>
      </summary>
      <div class="track-body">
        <div class="actions">
          ${archived ? `
          <button data-act="unarchive" class="primary" title="登録作品に戻します（カレンダーにまた表示されます）">登録作品に戻す</button>` : `
          <button data-act="edit">作品を編集</button>
          <button data-act="add">話を追加</button>
          <button data-act="gen" title="最後の話の日時から一定間隔で残りの話を作成">間隔で話を作成</button>
          <button data-act="watchall">配信済みを全て視聴済みに</button>
          ${t.tid ? '<button data-act="resync">取得データを再反映</button>' : ''}
          <button data-act="archive" class="primary" ${allWatched(t) ? '' : 'disabled'} title="${archiveBtnTitle(t)}">視聴済みに移す</button>`}
          <span class="spacer"></span>
          <button data-act="del" class="danger">登録解除</button>
        </div>
        ${t.note ? `<p class="note">${esc(t.note)}</p>` : ''}
        <div class="eps" data-id="${t.id}"><p class="muted small">読み込み中…</p></div>
      </div>
    </details>`).join('');
  for (const d of $$('details.track[open]', root)) loadEpisodes(Number(d.dataset.id));
}

async function loadEpisodes(trackId) {
  const box = $(`.eps[data-id="${trackId}"]`);
  if (!box) return;
  const eps = await api(`/api/tracks/${trackId}/episodes`);
  box._eps = eps;
  const now = new Date();
  box.innerHTML = eps.length ? `<table class="eptable"><thead><tr>
      <th>視聴</th><th>話数</th><th>配信開始</th><th>サブタイトル</th><th>状態</th><th></th></tr></thead><tbody>
      ${eps.map(e => {
        const future = e.start_at && new Date(e.start_at) > now;
        return `<tr class="${e.hidden ? 'is-hidden' : ''} ${e.watched ? 'done' : ''} ${future ? 'future' : ''}" data-ep="${e.id}">
          <td><input type="checkbox" class="w" ${e.watched ? 'checked' : ''} ${e.hidden ? 'disabled' : ''}></td>
          <td>${epNum(e.number) || '—'}</td>
          <td>${esc(fmtFull(e.start_at))}</td>
          <td>${esc(e.subtitle || '')}</td>
          <td>${badges(e)}</td>
          <td><button class="small" data-act="ep-edit">編集</button></td></tr>`;
      }).join('')}</tbody></table>`
    : '<p class="muted small">エピソードがありません。「話を追加」または「間隔で話を作成」で登録できます。</p>';
}

async function trackAction(t, act) {
  if (act === 'edit') {
    let chOptions = null;
    if (t.tid) {
      const chs = await api(`/api/titles/${t.tid}/channels`);
      chOptions = chs.map(c => ({ value: c.chid, label: `${c.name || 'ChID:' + c.chid}（${fmtFull(c.first_start)}〜）` }));
    }
    const fields = [
      { name: 'title', label: 'タイトル', value: t.title || '', placeholder: t.orig_title || '', help: t.tid ? '空欄にすると取得データのタイトルを表示' : '' },
      { name: 'platform', label: '媒体名（表示用）', value: t.platform || '', placeholder: t.orig_platform || '', help: t.tid ? '空欄にすると取得データの媒体名を表示' : '' },
    ];
    if (chOptions) fields.push({ name: 'chid', label: '取得元チャンネル（媒体の変更）', type: 'select', options: chOptions, value: t.chid, help: '変更すると配信日時を新しい媒体のデータで取り直します（視聴記録・手動修正は保持）' });
    if (t.tid) fields.push({ name: 'auto_sync', label: '定期取得で自動更新する', type: 'checkbox', value: !!t.auto_sync });
    fields.push({ name: 'note', label: 'メモ', type: 'textarea', value: t.note || '' });
    const r = await formDialog({ title: '作品を編集', fields });
    if (r.action !== 'ok') return false;
    if (r.values.chid !== undefined) r.values.chid = Number(r.values.chid);
    await api(`/api/tracks/${t.id}`, { method: 'PATCH', body: r.values });
    toast('保存しました');
  } else if (act === 'add') {
    const box = $(`.eps[data-id="${t.id}"]`);
    const eps = box?._eps || [];
    const nums = eps.filter(e => e.number !== null).map(e => e.number);
    const r = await formDialog({
      title: '話を追加', submit: '追加', fields: [
        { name: 'number', label: '話数', type: 'number', step: '0.5', value: nums.length ? Math.floor(Math.max(...nums)) + 1 : 1 },
        { name: 'start_at', label: '配信開始日時', type: 'datetime-local' },
        { name: 'subtitle', label: 'サブタイトル' },
        { name: 'watched', label: '視聴済み', type: 'checkbox' },
      ],
    });
    if (r.action !== 'ok') return false;
    await api(`/api/tracks/${t.id}/episodes`, { method: 'POST', body: r.values });
    toast('追加しました');
  } else if (act === 'gen') {
    const r = await formDialog({
      title: '間隔で話を作成', submit: '作成',
      html: '<p class="muted small">日時が入っている最後の話を基準に、指定話数まで一定間隔で「予測」エピソードを作ります。後で取得データが入ると自動で置き換わります。</p>',
      fields: [
        { name: 'until', label: '最終話数', type: 'number', value: 12, min: 1, required: true },
        { name: 'interval_days', label: '間隔（日）', type: 'number', value: 7, min: 1, required: true },
      ],
    });
    if (r.action !== 'ok') return false;
    const res = await api(`/api/tracks/${t.id}/generate`, { method: 'POST', body: r.values });
    toast(`${res.created} 話を作成しました`);
  } else if (act === 'watchall') {
    if (!await confirmDialog('配信済みの話をすべて視聴済みにしますか？')) return false;
    const res = await api(`/api/tracks/${t.id}/watch_all`, { method: 'POST', body: { watched: true, until_now: true } });
    toast(`${res.updated} 話を更新しました`);
  } else if (act === 'resync') {
    const res = await api(`/api/tracks/${t.id}/resync`, { method: 'POST' });
    toast(`取得データ ${res.programs} 件を反映しました（手動修正した話は維持）`);
  } else if (act === 'archive') {
    await api(`/api/tracks/${t.id}/archive`, { method: 'POST' });
    state.catalog = null;
    toast(`「${t.display_title}」を視聴済みに移しました`);
  } else if (act === 'unarchive') {
    await api(`/api/tracks/${t.id}/unarchive`, { method: 'POST' });
    toast(`「${t.display_title}」を登録作品に戻しました`);
  } else if (act === 'del') {
    if (!await confirmDialog(`「${t.display_title}」の登録を解除しますか？視聴記録も削除されます。`, '解除')) return false;
    await api(`/api/tracks/${t.id}`, { method: 'DELETE' });
    state.catalog = null;
    toast('登録を解除しました');
  }
  return true;
}

async function manualAdd() {
  const r = await formDialog({
    title: '手動で作品を追加', submit: '追加',
    html: '<p class="muted small">取得データに無い作品を登録します。第1話の日時と話数を入れると、間隔ごとに話を作成します。</p>',
    fields: [
      { name: 'title', label: 'タイトル', required: true },
      { name: 'platform', label: '媒体', placeholder: '例: ABEMA' },
      { name: 'first_start', label: '第1話 配信開始日時', type: 'datetime-local' },
      { name: 'episodes', label: '話数', type: 'number', value: 12, min: 1 },
      { name: 'interval_days', label: '間隔（日）', type: 'number', value: 7, min: 1 },
    ],
  });
  if (r.action !== 'ok') return;
  const t = await api('/api/tracks', { method: 'POST', body: r.values });
  state.openTracks.add(t.id); store.set('openTracks', [...state.openTracks]);
  toast('追加しました');
  await renderTracks();
}

/* ============================================================ logs */
const TRIGGER_LABEL = { auto: '定期', startup: '起動時', manual: '今すぐ', range: '期間指定' };
const fmtDur = s => {
  if (s === null || s === undefined) return '—';
  if (s < 10) return `${s.toFixed(1)}秒`;
  const t = Math.round(s), m = Math.floor(t / 60);
  return m ? `${m}分${pad(t % 60)}秒` : `${t}秒`;
};
const STATUS_LABEL = { ok: '成功', error: 'エラー', running: '取得中', cancelled: '中断' };
async function renderLogs() {
  const [logs, cover] = await Promise.all([api('/api/logs'), api('/api/coverage')]);
  // 取得済みの期間を新しい順の一覧にし、間の「未取得」も示す（何を取り直せばよいか分かるように）
  const dayMs = 864e5, toD = s => new Date(s + 'T00:00:00'), plus = (s, n) => ymd(new Date(toD(s).getTime() + n * dayMs));
  const rows = [];
  cover.forEach((r, i) => {
    if (i > 0) {
      const gs = plus(cover[i - 1].end, 1), ge = plus(r.start, -1);
      rows.push({ gap: true, start: gs, end: ge, days: Math.round((toD(ge) - toD(gs)) / dayMs) + 1 });
    }
    rows.push(r);
  });
  $('#log-coverage').innerHTML = `<div class="cov-head"><strong>取得済みの期間</strong>
    <span class="muted small">初回・期間指定の合計（定期の差分取得は含みません）</span></div>
    <div class="cov-list">${rows.length ? rows.reverse().map(r => `<div class="cov-row ${r.gap ? 'gap' : 'ok'}">
      <span class="cov-kind">${r.gap ? '未取得' : '取得済み'}</span>
      <span class="cov-range">${esc(r.start)} 〜 ${esc(r.end)}</span>
      <span class="cov-days">${r.days}日</span></div>`).join('')
      : '<div class="cov-row"><span class="muted">まだありません</span></div>'}</div>`;
  const box = $('#log-list');
  if (!logs.length) { box.innerHTML = '<p class="empty">取得履歴はまだありません。</p>'; return; }
  const t = s => s ? s.replace('T', ' ') : '—';
  box.innerHTML = `<table class="eptable logtable"><thead><tr><th>実行日時</th><th>種別</th><th>取得範囲</th><th>結果</th><th>所要時間</th><th>番組</th><th>タイトル</th><th>詳細</th></tr></thead><tbody>
    ${logs.map(l => `<tr class="${l.status === 'error' ? 'logerr' : ''}">
      <td data-l="実行日時">${esc(t(l.started_at))}</td>
      <td data-l="種別">${esc(TRIGGER_LABEL[l.trigger] || l.trigger)}</td>
      <td data-l="取得範囲">${esc(l.range_start || '—')} 〜 ${esc(l.range_end || '—')}</td>
      <td data-l="結果"><span class="badge ${l.status === 'error' ? 'edited' : l.status === 'ok' ? 'src-sync' : 'src-generated'}">${esc(STATUS_LABEL[l.status] || l.status)}</span></td>
      <td data-l="所要時間">${fmtDur(l.duration_sec)}</td>
      <td data-l="番組">${l.programs ?? '—'}</td><td data-l="タイトル">${l.titles ?? '—'}</td>
      <td class="muted small" data-l="詳細">${esc(l.message || '')}</td></tr>`).join('')}</tbody></table>`;
}

/* ============================================================ sync status */
let pollTimer = null;
async function refreshStatus() {
  const s = await api('/api/status');
  const el = $('#sync-status');
  if (s.running) {
    el.textContent = `取得中… ${s.progress || ''}`;
    el.className = 'running';
  } else if (s.auto_paused) {
    el.textContent = `自動取得を停止中（連続失敗）: ${s.last_error || ''} 「今すぐ取得」で成功すると再開します`;
    el.className = 'err';
  } else if (s.last_error) {
    el.textContent = `取得エラー: ${s.last_error}`;
    el.className = 'err';
  } else {
    el.textContent = s.last_success ? `最終取得 ${s.last_success.replace('T', ' ')}` : '未取得';
    el.className = 'muted';
  }
  el.title = s.last_message || '';
  $('#sync-btn').disabled = s.running;
  $('#sync-range-btn').disabled = s.running;
  // 中断は取得中だけ押せる。中断要求後（停止処理中）も、二重に押せないよう無効にする
  const cb = $('#sync-cancel-btn'), stopping = s.running && (s.progress || '').startsWith('中断');
  cb.disabled = !s.running || stopping;
  cb.textContent = stopping ? '中断中…' : '中断';
  cb.title = !s.running ? '実行中の取得はありません' : stopping ? '中断しています' : '実行中の取得を中断（取得済みの分は保存されます）';
  clearTimeout(pollTimer);
  if (s.running) pollTimer = setTimeout(refreshStatus, 3000);
  else if (refreshStatus._wasRunning) { state.catalog = null; renderTab(); }
  refreshStatus._wasRunning = s.running;
}

/* ============================================================ tabs / init */
async function renderTab() {
  $$('.tabs button').forEach(b => b.classList.toggle('active', b.dataset.tab === state.tab));
  $$('.tab').forEach(s => s.classList.toggle('active', s.id === 'tab-' + state.tab));
  if (state.tab === 'calendar') await renderCalendar();
  else if (state.tab === 'catalog') await loadCatalog();
  else if (state.tab === 'logs') await renderLogs();
  else if (state.tab === 'archive') await renderTracks(true);
  else await renderTracks(false);
}

function init() {
  $('#opt-late').checked = state.late;
  $('#opt-hide-watched').checked = state.hideWatched;

  $$('.tabs button').forEach(b => b.addEventListener('click', guard(async () => {
    state.tab = b.dataset.tab; store.set('tab', state.tab); await renderTab();
  })));
  $$('.seg [data-view]').forEach(b => b.addEventListener('click', guard(async () => {
    state.view = b.dataset.view; store.set('view', state.view); state.cursor = new Date(); await renderCalendar();
  })));
  const shift = dir => guard(async () => {
    const c = state.cursor;
    state.cursor = state.view === 'month' || state.view === 'list' ? new Date(c.getFullYear(), c.getMonth() + dir, 1)
      : new Date(c.getFullYear(), c.getMonth(), c.getDate() + 7 * dir);
    await renderCalendar();
  });
  $('#cal-prev').addEventListener('click', shift(-1));
  $('#cal-next').addEventListener('click', shift(1));
  $('#cal-today').addEventListener('click', guard(async () => { state.cursor = new Date(); await renderCalendar(); }));
  $('#opt-late').addEventListener('change', guard(async e => { state.late = e.target.checked; store.set('late', state.late); await renderTab(); }));
  $('#opt-hide-watched').addEventListener('change', guard(async e => { state.hideWatched = e.target.checked; store.set('hideWatched', state.hideWatched); await renderCalendar(); }));
  bindEventClicks($('#cal-month'), renderCalendar);
  // スマホの月表示: 日付をタップすると、その日の予定を下に表示する
  $('#cal-month').addEventListener('click', guard(async ev => {
    const day = ev.target.closest('.day');
    if (!isNarrow() || !day?.dataset.date) return;
    state.selDay = day.dataset.date;
    await renderCalendar();
  }));
  bindEventClicks($('#cal-day'), renderCalendar);
  setInterval(() => { if (state.tab === 'calendar' && state.view === 'week' && state.nowTick) state.nowTick(); }, 30000);
  let rt;
  window.addEventListener('resize', () => { // 画面の回転・リサイズで、高さに合わせて描き直す
    clearTimeout(rt);
    rt = setTimeout(() => { if (state.tab === 'calendar') renderCalendar().catch(() => { }); }, 250);
  });
  narrowMq.addEventListener('change', () => { if (state.tab === 'calendar') renderCalendar().catch(e => toast(e.message, true)); });
  bindEventClicks($('#cal-week'), renderCalendar);
  bindEventClicks($('#cal-list'), renderCalendar);

  ['#cat-season', '#cat-platform', '#cat-net', '#cat-untracked'].forEach(s => $(s).addEventListener('change', renderCatalog));
  $('#cat-q').addEventListener('input', renderCatalog);
  $('#cat-list').addEventListener('click', guard(async ev => {
    const b = ev.target.closest('.chip');
    if (!b) return;
    if (b.dataset.track) {
      state.openTracks.add(Number(b.dataset.track)); store.set('openTracks', [...state.openTracks]);
      state.tab = b.dataset.archived === '1' ? 'archive' : 'tracks'; store.set('tab', state.tab); await renderTab();
      $(`details.track[data-id="${b.dataset.track}"]`)?.scrollIntoView({ block: 'start' });
      return;
    }
    b.disabled = true;
    const t = await api('/api/tracks', { method: 'POST', body: { tid: Number(b.dataset.tid), chid: Number(b.dataset.chid) } })
      .finally(() => (b.disabled = false));
    toast(`登録しました: ${t.display_title}（${t.display_platform}） ${t.ep_total}話`);
    await loadCatalog(true);
  }));

  const bindTrackRoot = tl => {
    const archived = tl.dataset.archived === '1';
    tl.addEventListener('toggle', ev => {
      const d = ev.target;
      if (!d.matches?.('details.track')) return;
      const id = Number(d.dataset.id);
      if (d.open) { state.openTracks.add(id); loadEpisodes(id).catch(e => toast(e.message, true)); }
      else state.openTracks.delete(id);
      store.set('openTracks', [...state.openTracks]);
    }, true);
    tl.addEventListener('click', guard(async ev => {
      const det = ev.target.closest('details.track');
      if (!det) return;
      const t = tl._tracks.find(x => x.id === Number(det.dataset.id));
      const row = ev.target.closest('tr[data-ep]');
      if (row) {
        const box = row.closest('.eps');
        const e = box._eps.find(x => x.id === Number(row.dataset.ep));
        if (ev.target.classList.contains('w')) {
          await toggleWatched(e.id, ev.target.checked);
          await refreshTrack(t.id);
        } else if (ev.target.dataset.act === 'ep-edit' || ev.target.tagName === 'TD') {
          if (await editEpisode(e, t.display_title)) await refreshTrack(t.id);
        }
        return;
      }
      const act = ev.target.closest('button[data-act]')?.dataset.act;
      if (act && await trackAction(t, act)) await renderTracks(archived);
    }));
  };
  $$('.track-root').forEach(bindTrackRoot);
  $('#track-add-manual').addEventListener('click', guard(manualAdd));

  $('#sync-btn').addEventListener('click', guard(async () => {
    const r = await api('/api/sync', { method: 'POST' });
    toast(r.started ? '取得を開始しました' : '取得中です');
    refreshStatus._wasRunning = true;
    await refreshStatus();
  }));

  $('#sync-cancel-btn').addEventListener('click', guard(async () => {
    const dlg = await confirmDialog('実行中の取得を中断しますか？（取得済みの分は保存されます）', '中断する');
    if (!dlg) return;
    const r = await api('/api/sync/cancel', { method: 'POST' });
    toast(r.cancelled ? '中断を要求しました' : '実行中の取得はありません');
    await refreshStatus();
  }));
  $('#sync-range-btn').addEventListener('click', guard(async () => {
    const day = n => { const d = new Date(); d.setDate(d.getDate() + n); return ymd(d); };
    const r = await formDialog({
      title: '期間を指定して取得', submit: '取得',
      html: '<p class="muted small">指定した期間の配信予定をしょぼいカレンダーから取得します。開始日を選ぶと終了日が90日間の末日に、終了日を選ぶと開始日が90日間の初日に自動で入ります（あとから短く直せます）。最大90日です。2日ごとに1.5秒間隔で取得するため、長い期間は数分かかります。放送開始が古い作品や、先の予定を一覧に出したいときに使います。</p>',
      fields: [
        { name: 'start', label: '開始日', type: 'date', value: store.get('syncStart', day(-30)), required: true },
        { name: 'end', label: '終了日', type: 'date', value: store.get('syncEnd', day(59)), required: true },
      ],
      // 開始日を決めると終了日を最大期間（90日）の末日に、終了日を決めると開始日を90日前に自動設定する
      setup: dlg => {
        const st = $('#f_start', dlg), en = $('#f_end', dlg);
        const shiftDay = (v, n) => { const [Y, M, D] = v.split('-').map(Number); return ymd(new Date(Y, M - 1, D + n)); };
        // 前回の値が上限を超えていたら、終了日を詰める
        if (st.value && en.value && en.value > shiftDay(st.value, 89)) en.value = shiftDay(st.value, 89);
        st.addEventListener('change', () => { if (st.value) en.value = shiftDay(st.value, 89); });
        en.addEventListener('change', () => { if (en.value) st.value = shiftDay(en.value, -89); });
      },
    });
    if (r.action !== 'ok') return;
    store.set('syncStart', r.values.start); store.set('syncEnd', r.values.end);
    const res = await api('/api/sync', { method: 'POST', body: r.values });
    toast(res.started ? '取得を開始しました' : '取得中です');
    refreshStatus._wasRunning = true;
    await refreshStatus();
  }));

  $('#notify-btn').addEventListener('click', guard(notifySettings));
  if ('serviceWorker' in navigator) navigator.serviceWorker.register('/sw.js').catch(() => { });
  renderTab().catch(e => toast(e.message, true));
  refreshStatus().catch(() => { });
  setInterval(() => refreshStatus().catch(() => { }), 60000);
}
init();
