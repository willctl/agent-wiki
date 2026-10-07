// The window's calls to the local service (same origin), and small formatting helpers.

async function get(path) {
  const r = await fetch(path, { headers: { Accept: 'application/json' }, cache: 'no-store' });
  const body = await r.json().catch(() => ({}));
  if (!r.ok) throw Object.assign(new Error(body.error || `HTTP ${r.status}`), { status: r.status });
  return body;
}

/** State-changing calls carry X-Agent-Wiki, which another site's page cannot send. */
async function post(path, data) {
  const r = await fetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-Agent-Wiki': 'ui' },
    body: JSON.stringify(data),
  });
  const body = await r.json().catch(() => ({}));
  if (!r.ok) throw Object.assign(new Error(body.error || `HTTP ${r.status}`), { status: r.status });
  return body;
}

export const api = {
  status: () => get('/api/status'),
  pages: () => get('/api/pages'),
  page: (slug) => get(`/api/page?slug=${encodeURIComponent(slug)}`),
  search: (q, scope = 'all') => get(`/api/search?q=${encodeURIComponent(q)}&scope=${scope}&limit=30`),
  activity: (days = 14) => get(`/api/activity?days=${days}&max=300`),
  inbox: () => get('/api/inbox'),
  held: () => get('/api/held'),
  decideHeld: (batch, index, action) => post('/api/held', { batch, index, action }),
  undoHeld: (batch, index) => post('/api/held', { batch, index, action: 'undo' }),
  setApprovals: (approvals) => post('/api/settings', { approvals }),
  settings: () => get('/api/settings'),
  models: () => get('/api/models'),
  setModel: (role, model, reasoningEffort) => post('/api/models', { role, model, reasoningEffort }),
  setCleanupSchedule: (cleanupSchedule) => post('/api/settings', { cleanupSchedule }),
  cleanupPreview: (schedule) => get(`/api/cleanup-preview?schedule=${encodeURIComponent(schedule)}`),
  revert: (batch, slug, action = 'revert') => post('/api/revert', { batch, slug, action }),
  forget: (text, ignoreCase, apply = false) => post('/api/forget', { text, ignoreCase, apply }),
  decideForget: (batch, index, action) => post('/api/forget', { batch, index, action }),
  note: (id) => get(`/api/note?id=${encodeURIComponent(id)}`),
  setPaused: (paused) => post('/api/curator', { paused }),
  asks: () => get('/api/asks'),
  ask: (id, after = 0, thread = false) => get(`/api/ask?id=${encodeURIComponent(id)}&after=${after}${thread ? '&thread=1' : ''}`),
  startAsk: (question, parent) => post('/api/ask', { question, ...(parent ? { parent } : {}) }),
  cancelAsk: (id) => post('/api/ask/cancel', { id }),
};

const RTF = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });

/** "3 min ago", "yesterday", "Sep 28". Accepts ISO strings, "YYYY-MM-DD HH:MM" or ms. */
export function ago(when, now = Date.now()) {
  const t = typeof when === 'number' ? when : Date.parse(String(when || '').replace(' ', 'T'));
  if (!Number.isFinite(t)) return '';
  const s = Math.round((t - now) / 1000);
  const a = Math.abs(s);
  if (a < 45) return 'just now';
  if (a < 3600) return RTF.format(Math.round(s / 60), 'minute');
  if (a < 86400) return RTF.format(Math.round(s / 3600), 'hour');
  if (a < 7 * 86400) return RTF.format(Math.round(s / 86400), 'day');
  return new Date(t).toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
}

export function dayLabel(date, now = new Date()) {
  const d = new Date(`${date}T12:00:00`);
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate(), 12);
  const diff = Math.round((today - d) / 86400000);
  if (diff === 0) return 'Today';
  if (diff === 1) return 'Yesterday';
  return d.toLocaleDateString(undefined, { weekday: 'long', month: 'short', day: 'numeric' });
}

export function duration(sec) {
  if (sec == null) return '';
  if (sec < 90) return `${sec} s`;
  if (sec < 5400) return `${Math.round(sec / 60)} min`;
  if (sec < 172800) return `${Math.floor(sec / 3600)} h ${Math.round((sec % 3600) / 60)} min`;
  return `${Math.floor(sec / 86400)} days`;
}

/** A stable hue per app name, with fixed colors for the usual ones. */
export function appColor(app) {
  const known = { 'claude-code': 24, 'claude-desktop': 18, claude: 20, codex: 160, 'chatgpt-desktop': 150, chatgpt: 150, curator: 245, human: 210, installer: 200 };
  const a = String(app || '').toLowerCase();
  if (a in known) return known[a];
  let h = 0;
  for (const c of a) h = (h * 31 + c.charCodeAt(0)) % 360;
  return h;
}
