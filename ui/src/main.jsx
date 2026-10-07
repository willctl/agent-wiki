// The Agent Wiki tray window: search, ask the wiki (an agent answers from it), read pages, follow
// the activity log and the curator's inbox, and check the service. Served by the local service at
// /ui/; the tray opens it as an Edge app window.

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { ago, api, appColor, dayLabel, duration } from './api.js';
import { createRenderer, highlight, plainText, snippetAround } from './markdown.js';
import './app.css';

const VERSION = typeof __AGENT_WIKI_VERSION__ !== 'undefined' ? __AGENT_WIKI_VERSION__ : '';

// ---------------------------------------------------------------- routing (#/pages, #/page/<slug>, #/ask, #/ask/<id>, #/activity, #/day/<date>, #/note/<id>, #/inbox, #/status)

function parseRoute(hash) {
  const [, view = 'pages', arg = ''] = (hash || '').match(/^#\/([a-z]+)(?:\/(.+))?$/) || [];
  return { view, arg: decodeURIComponent(arg) };
}
function useRoute() {
  const [hash, setHash] = useState(location.hash);
  useEffect(() => {
    const f = () => setHash(location.hash);
    addEventListener('hashchange', f);
    return () => removeEventListener('hashchange', f);
  }, []);
  return parseRoute(hash);
}
const go = (h) => {
  if (location.hash !== h) location.hash = h;
};
const back = () => (history.length > 1 ? history.back() : go('#/pages'));

// ---------------------------------------------------------------- data

function useStatus() {
  const [state, setState] = useState({ status: null, error: null });
  useEffect(() => {
    let alive = true;
    let timer;
    const tick = async () => {
      try {
        const status = await api.status();
        if (alive) setState({ status, error: null });
      } catch (e) {
        if (alive) setState((s) => ({ status: s.status, error: e }));
      }
      if (alive) timer = setTimeout(tick, document.hidden ? 15000 : 4000);
    };
    tick();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, []);
  return state;
}

/** Loads `load()` and again whenever `key` changes; keeps the old data while reloading. */
function useResource(load, key) {
  const [s, set] = useState({ data: null, error: null, loading: true });
  useEffect(() => {
    let alive = true;
    set((p) => ({ ...p, loading: true }));
    load().then(
      (data) => alive && set({ data, error: null, loading: false }),
      (error) => alive && set((p) => ({ data: p.data, error, loading: false })),
    );
    return () => {
      alive = false;
    };
  }, [key]); // eslint-disable-line react-hooks/exhaustive-deps
  return s;
}

const Toast = { show: () => {} };
function Toaster() {
  const [msg, setMsg] = useState(null);
  useEffect(() => {
    let t;
    Toast.show = (m) => {
      setMsg(m);
      clearTimeout(t);
      t = setTimeout(() => setMsg(null), Math.min(7000, 1400 + String(m).length * 40));
    };
  }, []);
  return <div className={`toast${msg ? ' on' : ''}`} role="status">{msg}</div>;
}
async function copy(text, what) {
  try {
    await navigator.clipboard.writeText(text);
    Toast.show(`Copied ${what}`);
  } catch {
    Toast.show('Could not copy');
  }
}

// ---------------------------------------------------------------- icons

const I = {
  search: <path d="M10.5 4a6.5 6.5 0 0 1 5.18 10.43l4.45 4.44-1.06 1.06-4.44-4.45A6.5 6.5 0 1 1 10.5 4Zm0 1.5a5 5 0 1 0 0 10 5 5 0 0 0 0-10Z" />,
  back: <path d="M10.3 5.3 4.6 11a1.4 1.4 0 0 0 0 2l5.7 5.7 1.06-1.06-4.9-4.9H20v-1.5H6.46l4.9-4.9L10.3 5.3Z" />,
  close: <path d="m6.06 5 5.94 5.94L17.94 5 19 6.06 13.06 12 19 17.94 17.94 19 12 13.06 6.06 19 5 17.94 10.94 12 5 6.06 6.06 5Z" />,
  copy: <path d="M8 3h9.5A2.5 2.5 0 0 1 20 5.5V17h-1.5V5.5c0-.55-.45-1-1-1H8V3Zm-2.5 3h9A2.5 2.5 0 0 1 17 8.5v10a2.5 2.5 0 0 1-2.5 2.5h-9A2.5 2.5 0 0 1 3 18.5v-10A2.5 2.5 0 0 1 5.5 6Zm0 1.5c-.55 0-1 .45-1 1v10c0 .55.45 1 1 1h9c.55 0 1-.45 1-1v-10c0-.55-.45-1-1-1h-9Z" />,
  pause: <path d="M7 4.5h3.5v15H7v-15Zm6.5 0H17v15h-3.5v-15Z" />,
  play: <path d="M7 4.2c0-.8.86-1.28 1.53-.86l11.1 7.03c.63.4.63 1.32 0 1.72L8.53 19.12A1 1 0 0 1 7 18.27V4.2Z" />,
  page: <path d="M6.5 2h7.38L19 7.12V19.5A2.5 2.5 0 0 1 16.5 22h-10A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2Zm0 1.5c-.55 0-1 .45-1 1v15c0 .55.45 1 1 1h10c.55 0 1-.45 1-1V8h-4.5V3.5H6.5Zm8 .56V6.5h2.44L14.5 4.06ZM8 11h7v1.5H8V11Zm0 3.5h7V16H8v-1.5Z" />,
  note: <path d="M5.5 3h13A2.5 2.5 0 0 1 21 5.5v9a2.5 2.5 0 0 1-2.5 2.5H11l-5 4v-4h-.5A2.5 2.5 0 0 1 3 14.5v-9A2.5 2.5 0 0 1 5.5 3Zm0 1.5c-.55 0-1 .45-1 1v9c0 .55.45 1 1 1h2v2.38l2.98-2.38h8.02c.55 0 1-.45 1-1v-9c0-.55-.45-1-1-1h-13Z" />,
  log: <path d="M12 3a9 9 0 1 1 0 18 9 9 0 0 1 0-18Zm0 1.5a7.5 7.5 0 1 0 0 15 7.5 7.5 0 0 0 0-15Zm-.75 2.5h1.5v4.69l3.03 3.03-1.06 1.06-3.47-3.47V7Z" />,
  link: <path d="M9.5 7H7a5 5 0 0 0 0 10h2.5v-1.5H7a3.5 3.5 0 1 1 0-7h2.5V7Zm5 0H17a5 5 0 0 1 0 10h-2.5v-1.5H17a3.5 3.5 0 1 0 0-7h-2.5V7ZM8 11.25h8v1.5H8v-1.5Z" />,
  edit: <path d="M15.6 3.6a2.5 2.5 0 0 1 3.54 0l1.26 1.26a2.5 2.5 0 0 1 0 3.54L9.12 19.68 3.5 20.5l.82-5.62L15.6 3.6Zm2.48 1.06a1 1 0 0 0-1.42 0L5.73 15.6l-.46 3.13 3.13-.46 10.93-10.93a1 1 0 0 0 0-1.42l-1.25-1.26Z" />,
  check: <path d="m9.5 16.4-4.2-4.2-1.06 1.06 5.26 5.26L20.76 7.26 19.7 6.2 9.5 16.4Z" />,
  spark: <path d="M12 2.5c.3 3.7 1.8 5.2 5.5 5.5-3.7.3-5.2 1.8-5.5 5.5-.3-3.7-1.8-5.2-5.5-5.5 3.7-.3 5.2-1.8 5.5-5.5Zm6 9c.18 2.2 1.1 3.12 3.3 3.3-2.2.18-3.12 1.1-3.3 3.3-.18-2.2-1.1-3.12-3.3-3.3 2.2-.18 3.12-1.1 3.3-3.3Z" />,
  send: <path d="M12 3.6 18.9 10.5l-1.06 1.06-5.09-5.09V20.4h-1.5V6.47l-5.09 5.09L5.1 10.5 12 3.6Z" />,
  stop: <path d="M8 6.5h8A1.5 1.5 0 0 1 17.5 8v8a1.5 1.5 0 0 1-1.5 1.5H8A1.5 1.5 0 0 1 6.5 16V8A1.5 1.5 0 0 1 8 6.5Z" />,
  retry: <path d="M12 4.5a7.5 7.5 0 0 1 6.9 4.55V5.5h1.5V11.5h-6V10h3.6A6 6 0 1 0 18 12h1.5A7.5 7.5 0 1 1 12 4.5Z" />,
  chevron: <path d="m7.06 9 4.94 4.94L16.94 9 18 10.06l-6 6-6-6L7.06 9Z" />,
};
const Icon = ({ name, size = 16 }) => (
  <svg className="icon" width={size} height={size} viewBox="0 0 24 24" aria-hidden="true" fill="currentColor">
    {I[name]}
  </svg>
);

/** The tray mark: a dog-eared page with a speech-bubble tail. */
const Logo = ({ health }) => (
  <svg className={`logo ${health}`} width="30" height="30" viewBox="0 0 256 256" aria-hidden="true">
    <path className="logo-page" d="M52 16 H164 L232 84 V168 C232 184 220 196 204 196 H116 L64 244 V196 H52 C36 196 24 184 24 168 V44 C24 28 36 16 52 16 Z" />
    <path className="logo-fold" d="M164 16 V64 C164 75 173 84 184 84 H232 Z" />
    <rect fill="#fff" x="60" y="68" width="76" height="22" rx="11" />
    <rect fill="#fff" x="60" y="118" width="136" height="22" rx="11" />
  </svg>
);

const TYPE_LABEL = { project: 'Project', person: 'Person', preference: 'Preference', decision: 'Decision', howto: 'How-to', reference: 'Reference', topic: 'Topic' };
const TypeChip = ({ type }) => <span className={`chip type-${type}`}>{TYPE_LABEL[type] || type}</span>;

function AppBadge({ app, size = 26 }) {
  const name = String(app || '?');
  const words = name.split(/[-\s_]/).filter(Boolean);
  const KNOWN = { codex: 'CX', chatgpt: 'GP', 'chatgpt-desktop': 'GP', human: 'You' };
  const initials = KNOWN[name] || (words.length > 1 ? words.map((w) => w[0]).join('') : name).slice(0, 2).toUpperCase();
  return (
    <span className="app-badge" style={{ '--h': appColor(name), width: size, height: size }} title={name}>
      {name === 'curator' ? <Icon name="spark" size={size * 0.58} /> : initials}
    </span>
  );
}

function Empty({ icon = 'check', title, children }) {
  return (
    <div className="empty">
      <div className="empty-icon"><Icon name={icon} size={22} /></div>
      <div className="empty-title">{title}</div>
      {children && <div className="empty-text">{children}</div>}
    </div>
  );
}

function Skeleton({ rows = 4 }) {
  return (
    <div className="list">
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="card skeleton">
          <div className="sk sk-title" />
          <div className="sk sk-line" />
          <div className="sk sk-line short" />
        </div>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------- header, search, tabs

function StatusPill({ status, offline }) {
  const health = offline ? 'down' : !status ? 'wait' : status.health === 'ok' ? 'ok' : 'warn';
  const label = { down: 'Service down', wait: 'Connecting', ok: 'Healthy', warn: 'Needs attention' }[health];
  const title = offline ? 'The Agent Wiki service is not answering.' : status?.reasons?.join('\n') || 'All good';
  return (
    <button className={`pill pill-${health}`} onClick={() => go('#/status')} title={title}>
      <span className="dot" />
      {label}
    </button>
  );
}

function Header({ status, offline }) {
  const health = offline ? 'down' : status?.health === 'ok' ? 'ok' : status ? 'warn' : 'ok';
  return (
    <header className="top">
      <button className="brand" onClick={() => go('#/pages')} title="Agent Wiki">
        <Logo health={health} />
        <span className="brand-text">
          <span className="name">Agent Wiki</span>
          <span className="sub">{status ? `${status.queue?.pending ? `${status.queue.pending} waiting · ` : ''}${status.curator?.state || 'curator'} · v${status.version}` : `v${VERSION}`}</span>
        </span>
      </button>
      <StatusPill status={status} offline={offline} />
    </header>
  );
}

function SearchBox({ value, onChange, inputRef, onKeyDown }) {
  return (
    <div className="search">
      <Icon name="search" size={17} />
      <input
        ref={inputRef}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={onKeyDown}
        placeholder="Search pages, notes and the log"
        spellCheck="false"
        autoComplete="off"
        aria-label="Search the wiki"
      />
      {value ? (
        <button className="icon-btn" onClick={() => onChange('')} title="Clear (Esc)">
          <Icon name="close" size={14} />
        </button>
      ) : (
        <kbd>/</kbd>
      )}
    </div>
  );
}

function Tabs({ view, inboxCount, asking }) {
  const tabs = [
    ['pages', 'Pages'],
    ['ask', 'Ask'],
    ['activity', 'Activity'],
    ['inbox', 'Inbox', inboxCount],
    ['status', 'Status'],
  ];
  const active = view === 'page' ? 'pages' : view === 'day' ? 'activity' : view;
  return (
    <nav className="tabs" role="tablist">
      {tabs.map(([id, label, count]) => (
        <button key={id} role="tab" aria-selected={active === id} className={active === id ? 'on' : ''} onClick={() => go(`#/${id}`)}>
          {id === 'ask' && <Icon name="spark" size={13} />}
          {label}
          {count > 0 && <span className="count">{count}</span>}
          {id === 'ask' && asking && <span className="busy-dot" title="Answering a question" />}
        </button>
      ))}
    </nav>
  );
}

// ---------------------------------------------------------------- search results

function useDebounced(value, ms) {
  const [v, set] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => set(value), ms);
    return () => clearTimeout(t);
  }, [value, ms]);
  return v;
}

const noteHref = (target) => (String(target).startsWith('note:') ? `#/note/${String(target).slice(5)}` : '#/inbox');
const resultHref = (r) => (r.kind === 'page' ? `#/page/${r.target}` : r.kind === 'log' ? `#/day/${r.target}` : noteHref(r.target));

/** Row 0 of the search results: hand the query to the agent instead (Enter on it, or Ctrl+Enter anywhere). */
function AskRow({ query, selected, setSelected, onAsk, canAsk }) {
  return (
    <a
      href="#/ask"
      className={`result ask-row${selected ? ' sel' : ''}${canAsk ? '' : ' off'}`}
      onMouseEnter={() => setSelected(0)}
      onClick={(e) => {
        e.preventDefault();
        onAsk();
      }}
      title={canAsk ? 'An agent searches and reads the wiki, then answers with sources' : 'Asking needs the curator (tray app)'}
    >
      <span className="result-icon k-ask"><Icon name="spark" /></span>
      <span className="result-body">
        <span className="result-title">
          <span>Ask the wiki</span>
          <kbd>Ctrl ↵</kbd>
        </span>
        <span className="snippet">“{query.trim()}”: an agent searches and reads for you, then answers with sources</span>
      </span>
    </a>
  );
}

function SearchResults({ query, selected, setSelected, onOpen, results, setResults, onAsk, canAsk }) {
  const q = useDebounced(query.trim(), 140);
  const [state, setState] = useState({ loading: false, error: null, terms: [] });
  useEffect(() => {
    if (!q) return;
    let alive = true;
    setState((s) => ({ ...s, loading: true }));
    api.search(q).then(
      (r) => {
        if (!alive) return;
        setResults(r.results);
        // A question goes to the agent by default; keywords open the best match.
        setSelected(r.results.length && !looksLikeQuestion(q) ? 1 : 0);
        setState({ loading: false, error: null, terms: r.terms });
      },
      (e) => alive && setState({ loading: false, error: e, terms: [] }),
    );
    return () => {
      alive = false;
    };
  }, [q]); // eslint-disable-line react-hooks/exhaustive-deps

  const askRow = <AskRow query={query} selected={selected === 0} setSelected={setSelected} onAsk={onAsk} canAsk={canAsk} />;
  if (state.error) return <Empty icon="search" title="Search failed">{state.error.message}</Empty>;
  if (!results.length && !state.loading) {
    return (
      <div className="results">
        {askRow}
        <Empty icon="search" title={`Nothing found for “${query.trim()}”`}>Try fewer or different words, or ask the wiki: the agent tries other words too.</Empty>
      </div>
    );
  }
  const groups = [
    ['page', 'Pages'],
    ['pending', 'Waiting for the curator'],
    ['log', 'Activity log'],
    ['filed', 'Notes the apps sent'],
  ];
  const groupOf = (r) => (r.kind !== 'note' ? r.kind : r.rel.startsWith('.curator/') ? 'filed' : 'pending');
  let i = 0;
  return (
    <div className={`results${state.loading ? ' loading' : ''}`}>
      {askRow}
      {groups.map(([kind, label]) => {
        const items = results.filter((r) => groupOf(r) === kind);
        if (!items.length) return null;
        return (
          <section key={kind}>
            <h3 className="section-label">{label}</h3>
            {items.map((r) => {
              i++;
              const idx = i;
              const title = r.kind === 'page' ? r.label.replace(/\s\[[a-z]+\]$/, '') : r.kind === 'log' ? dayLabel(r.target) : r.snippets?.[0] || r.label;
              const type = r.kind === 'page' ? r.label.match(/\[([a-z]+)\]$/)?.[1] : null;
              const snippets = (r.kind === 'note' ? r.snippets.slice(1) : r.snippets).slice(0, 2);
              return (
                <a
                  key={`${r.kind}:${r.rel}`}
                  href={resultHref(r)}
                  className={`result${idx === selected ? ' sel' : ''}`}
                  onMouseEnter={() => setSelected(idx)}
                  onClick={(e) => {
                    e.preventDefault();
                    onOpen(r);
                  }}
                >
                  <span className={`result-icon k-${r.kind}`}><Icon name={r.kind === 'page' ? 'page' : r.kind === 'note' ? 'note' : 'log'} /></span>
                  <span className="result-body">
                    <span className="result-title">
                      <span dangerouslySetInnerHTML={{ __html: highlight(title, state.terms) }} />
                      {type && <TypeChip type={type} />}
                      {r.kind === 'log' && <span className="muted small">{r.target}</span>}
                    </span>
                    {snippets.map((s, k) => (
                      <span key={k} className="snippet" dangerouslySetInnerHTML={{ __html: highlight(snippetAround(s.replace(/^\[[^\]]+\]\s*/, ''), state.terms, 170), state.terms) }} />
                    ))}
                  </span>
                </a>
              );
            })}
          </section>
        );
      })}
    </div>
  );
}

// ---------------------------------------------------------------- pages

function PagesView({ pages }) {
  const [type, setType] = useState('all');
  if (pages.error && !pages.data) return <Empty icon="page" title="Could not load the pages">{pages.error.message}</Empty>;
  if (!pages.data) return <Skeleton />;
  const list = pages.data.pages;
  if (!list.length) return <Empty icon="page" title="No pages yet">Pages appear here as soon as the curator files the first notes.</Empty>;
  const counts = {};
  for (const p of list) counts[p.type] = (counts[p.type] || 0) + 1;
  const types = Object.keys(counts).sort((a, b) => counts[b] - counts[a]);
  const shown = type === 'all' ? list : list.filter((p) => p.type === type);
  return (
    <>
      <div className="filters">
        <button className={`filter${type === 'all' ? ' on' : ''}`} onClick={() => setType('all')}>
          All <span>{list.length}</span>
        </button>
        {types.map((t) => (
          <button key={t} className={`filter${type === t ? ' on' : ''}`} onClick={() => setType(t)}>
            {TYPE_LABEL[t] || t} <span>{counts[t]}</span>
          </button>
        ))}
      </div>
      <div className="list">
        {shown.map((p) => (
          <a key={p.slug} className="card page-card" href={`#/page/${p.slug}`}>
            <div className="card-head">
              <span className="card-title">{p.title}</span>
              <TypeChip type={p.type} />
            </div>
            {p.summary && <p className="card-text">{p.summary}</p>}
            <div className="card-meta">
              <span>Updated {ago(p.updated || p.time)}</span>
              {p.updatedBy && (
                <>
                  <span className="sep">·</span>
                  <span>{p.updatedBy}</span>
                </>
              )}
              {p.tags.slice(0, 3).map((t) => (
                <span key={t} className="tag">#{t}</span>
              ))}
            </div>
          </a>
        ))}
      </div>
    </>
  );
}

function PageView({ slug, render, refreshKey }) {
  const page = useResource(() => api.page(slug), `${slug}|${refreshKey}`);
  const articleRef = useRef(null);
  useEffect(() => {
    articleRef.current?.closest('main')?.scrollTo({ top: 0 });
  }, [slug]);
  if (page.error && (!page.data || page.data.slug !== slug)) {
    return (
      <div className="page">
        <BackBar />
        <Empty icon="page" title={page.error.status === 404 ? `No page “${slug}” yet` : 'Could not load the page'}>
          {page.error.status === 404 ? 'It is linked from somewhere, but nothing has been filed under it.' : page.error.message}
        </Empty>
      </div>
    );
  }
  if (!page.data || page.data.slug !== slug) return <Skeleton rows={2} />;
  const p = page.data;
  // The body usually starts with "# Title": the header shows it already.
  const body = p.body.replace(/^#\s+(.+)\n+/, (m, t) => (t.trim() === p.title ? '' : m));
  return (
    <div className="page">
      <BackBar>
        <button className="ghost" onClick={() => copy(`[[${p.slug}]]`, 'link')} title="Copy [[link]]">
          <Icon name="link" /> Link
        </button>
        <button className="ghost" onClick={() => copy(p.path, 'path')} title={p.path}>
          <Icon name="copy" /> Path
        </button>
      </BackBar>
      <div className="page-head">
        <div className="page-kicker">
          <TypeChip type={p.type} />
          {p.tags.map((t) => (
            <span key={t} className="tag">#{t}</span>
          ))}
        </div>
        <h1>{p.title}</h1>
        {p.summary && <p className="lede">{p.summary}</p>}
        <div className="page-meta">
          <span><Icon name="edit" size={13} /> Updated {ago(p.updated || p.time)}{p.updatedBy ? ` by ${p.updatedBy}` : ''}</span>
          <span>{p.words.toLocaleString()} words</span>
          {p.history.length > 0 && <span title="Earlier versions are kept in .history/">{p.history.length} earlier version{p.history.length > 1 ? 's' : ''}</span>}
        </div>
      </div>
      <article ref={articleRef} className="md" dangerouslySetInnerHTML={{ __html: render(body) }} />
      {(p.backlinks.length > 0 || p.links.length > 0) && (
        <div className="relations">
          {p.backlinks.length > 0 && (
            <div>
              <h3 className="section-label">Linked from</h3>
              <div className="link-chips">
                {p.backlinks.map((b) => (
                  <a key={b.slug} href={`#/page/${b.slug}`} className="link-chip">
                    <Icon name="page" size={13} /> {b.title}
                  </a>
                ))}
              </div>
            </div>
          )}
          {p.links.length > 0 && (
            <div>
              <h3 className="section-label">Links to</h3>
              <div className="link-chips">
                {p.links.map((l) => (
                  <a key={l.slug} href={`#/page/${l.slug}`} className={`link-chip${l.exists ? '' : ' missing'}`}>
                    <Icon name="page" size={13} /> {l.title}
                  </a>
                ))}
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function BackBar({ children }) {
  return (
    <div className="backbar">
      <button className="ghost" onClick={back} title="Back (Esc)">
        <Icon name="back" /> Back
      </button>
      <span className="grow" />
      {children}
    </div>
  );
}

// ---------------------------------------------------------------- activity

/** Undoes one page change of a curator batch; when the page moved on since, offers to ask the curator instead. */
async function revertChange(batch, slug, title) {
  if (!confirm(`Restore "${title}" to how it was before this change?`)) return;
  try {
    await api.revert(batch, slug);
    Toast.show(`Restored ${title}`);
  } catch (e) {
    if (e.status !== 409) return Toast.show(e.message);
    if (!confirm(`${e.message}\n\nAsk the curator to undo it on the current page?`)) return;
    try {
      await api.revert(batch, slug, 'ask');
      Toast.show('Asked the curator to undo it');
    } catch (e2) {
      Toast.show(e2.message);
    }
  }
}

function Entry({ e, render }) {
  const [open, setOpen] = useState(false);
  if (e.compact) {
    // "Page updated: Title [[slug]]", "Change approved: Title [[slug]]", ...
    const m = e.title.match(/^([A-Z][^:]{2,60}): (.+?)(?: \[\[([a-z0-9-]+)\]\])?$/);
    const revertable = m && /^Page (created|updated|rewritten)$/.test(m[1]) && m[3] && e.batch && e.app === 'curator';
    return (
      <div className="entry compact">
        <span className="time">{e.time}</span>
        <span className="compact-text">
          <span className="muted">{m ? m[1] : e.title}</span>
          {m && (m[3] ? <a href={`#/page/${m[3]}`}>{m[2]}</a> : <span>{m[2]}</span>)}
          <span className="muted small"> · {e.app}</span>
        </span>
        {revertable && (
          <button className="ghost small revert" onClick={() => revertChange(e.batch, m[3], m[2])} title="Undo this change to the page">
            Revert
          </button>
        )}
      </div>
    );
  }
  return (
    <div className={`entry${open ? ' open' : ''}`}>
      <AppBadge app={e.app} />
      <div className="entry-main">
        <button className="entry-head" onClick={() => setOpen(!open)} aria-expanded={open}>
          <span className="entry-title">{e.title}</span>
          <span className="time">{e.time}</span>
        </button>
        <div className="entry-sub">
          <span>{e.app}</span>
          {e.pages.slice(0, 4).map((s) => (
            <a key={s} href={`#/page/${s}`} className="tag">
              {s}
            </a>
          ))}
        </div>
        {open ? (
          <>
            <div className="md small-md" dangerouslySetInnerHTML={{ __html: render(e.body) }} />
            {e.notes?.length > 0 && (
              <div className="entry-sources muted small">
                {e.notes.length === 1 ? 'Source: ' : 'Sources: '}
                {e.notes.map((id, i) => (
                  <span key={id}>
                    {i > 0 && ' · '}
                    <a href={`#/note/${id}`}>{e.notes.length === 1 ? 'the original note' : `note ${i + 1}`}</a>
                  </span>
                ))}
              </div>
            )}
          </>
        ) : (
          e.body && (
            <p className="entry-preview" onClick={() => setOpen(true)}>
              {plainText(e.body, 180)}
            </p>
          )
        )}
      </div>
    </div>
  );
}

function Days({ days, render, filter }) {
  return days.map((d) => {
    const entries = d.entries.filter((e) => filter === 'all' || (filter === 'notes' ? !e.compact : e.compact));
    if (!entries.length) return null;
    return (
      <section key={d.date} className="day">
        <h3 className="day-label">
          {dayLabel(d.date)} <span>{d.date}</span>
        </h3>
        {entries.map((e, i) => (
          <Entry key={`${e.time}-${i}-${e.title}`} e={e} render={render} />
        ))}
      </section>
    );
  });
}

function ActivityView({ render, refreshKey }) {
  const act = useResource(() => api.activity(14), refreshKey);
  const [filter, setFilter] = useState('notes');
  if (act.error && !act.data) return <Empty icon="log" title="Could not load the activity log">{act.error.message}</Empty>;
  if (!act.data) return <Skeleton />;
  if (!act.data.days.length) return <Empty icon="log" title="Nothing in the last two weeks" />;
  return (
    <>
      <div className="filters">
        {[
          ['notes', 'What happened'],
          ['pages', 'Page changes'],
          ['all', 'Everything'],
        ].map(([id, label]) => (
          <button key={id} className={`filter${filter === id ? ' on' : ''}`} onClick={() => setFilter(id)}>
            {label}
          </button>
        ))}
      </div>
      <Days days={act.data.days} render={render} filter={filter} />
    </>
  );
}

function DayView({ date, render }) {
  const day = useResource(() => fetch(`/api/log?date=${date}`).then((r) => r.json()), date);
  return (
    <div className="page">
      <BackBar />
      {!day.data ? <Skeleton /> : day.data.error ? <Empty icon="log" title={day.data.error} /> : <Days days={[day.data]} render={render} filter="all" />}
    </div>
  );
}

function NoteView({ id, render }) {
  const note = useResource(() => api.note(id), id);
  if (note.error && !note.data) {
    return (
      <div className="page">
        <BackBar />
        <Empty icon="note" title="Could not open the note">{note.error.message}</Empty>
      </div>
    );
  }
  if (!note.data) return <Skeleton rows={2} />;
  const n = note.data;
  return (
    <div className="page">
      <BackBar />
      <div className="page-head">
        <div className="page-kicker">
          <span className="tag">{n.filed ? 'filed note' : 'waiting in the inbox'}</span>
          {n.source && <span className="tag">source: {n.source}</span>}
        </div>
        <h1>{n.title || id}</h1>
        <div className="page-meta">
          <span>{n.app}{n.submitted ? ` · ${ago(n.submitted)}` : ''}</span>
        </div>
        <div className="lede small md" dangerouslySetInnerHTML={{ __html: render(n.status) }} />
      </div>
      <article className="md" dangerouslySetInnerHTML={{ __html: render(n.body) }} />
    </div>
  );
}

// ---------------------------------------------------------------- inbox

function CuratorBar({ status }) {
  const [busy, setBusy] = useState(false);
  const c = status?.curator || {};
  const paused = Boolean(c.paused);
  const toggle = async () => {
    setBusy(true);
    try {
      await api.setPaused(!paused);
      Toast.show(paused ? 'Curator resumed' : 'Curator paused');
    } catch (e) {
      Toast.show(e.message);
    }
    setBusy(false);
  };
  const state = !c.running ? 'not running' : paused ? 'paused' : c.state;
  return (
    <div className={`curator-bar state-${(state || '').replace(/\s+/g, '-')}`}>
      <span className="curator-dot" />
      <div className="grow">
        <div className="curator-title">Curator {state}</div>
        <div className="muted small">
          {c.model ? `${c.model} · ${c.reasoningEffort}` : 'files notes into pages'}
          {c.lastRun?.at ? ` · last run ${ago(c.lastRun.at)}` : ''}
        </div>
      </div>
      <button className="btn" disabled={busy || !status} onClick={toggle}>
        <Icon name={paused ? 'play' : 'pause'} size={14} /> {paused ? 'Resume' : 'Pause'}
      </button>
    </div>
  );
}

const NOTE_STATUS = { pending: 'Waiting', retrying: 'Retrying', dead: 'Failed' };

const ACTION_LABEL = { created: 'new page', patched: 'update', replaced: 'rewrite' };

/** Ends a reason with exactly one full stop (a model's reason may bring its own). */
const sentence = (s) => s.replace(/[.\s]+$/, '') + '.';

/** The diff of a held change: changed lines with a little context; long unchanged runs folded. */
function Diff({ header, diff }) {
  const rows = [];
  const keep = diff.map(([k], i) => k !== ' ' || diff.slice(Math.max(0, i - 2), i + 3).some(([x]) => x !== ' '));
  for (let i = 0; i < diff.length; i++) {
    if (keep[i]) rows.push(<div key={i} className={`diff-line d-${diff[i][0] === '+' ? 'add' : diff[i][0] === '-' ? 'del' : 'ctx'}`}>{diff[i][0]} {diff[i][1]}</div>);
    else if (i === 0 || keep[i - 1]) {
      let j = i;
      while (j < diff.length && !keep[j]) j++;
      rows.push(<div key={i} className="diff-fold">… {j - i} unchanged line{j - i === 1 ? '' : 's'}</div>);
    }
  }
  return (
    <div className="diff">
      {header.map((h) => (
        <div key={h} className="diff-line d-add">+ {h}</div>
      ))}
      {rows}
    </div>
  );
}

/** The Approvals switch at the top of the Inbox: Automatic (the default) or Ask me first. */
function ApprovalsBar({ mode, problem, busy, onChange }) {
  return (
    <div className={`approvals-bar mode-${mode}`}>
      <div className="grow">
        <div className="curator-title">{mode === 'auto' ? 'Changes are applied automatically' : 'Changes wait for your OK'}</div>
        <div className="muted small">
          {mode === 'auto'
            ? 'Changes the curator would hold (from outside sources, preferences, new instructions or links) and scheduled cleanups go straight to the pages. You get a notification each time, and can undo any of them below. Requests to forget something always wait for your OK.'
            : 'Changes the curator holds and scheduled cleanups wait here until you approve them.'}
        </div>
        {problem && <div className="held-why">{sentence(problem)}</div>}
      </div>
      <div className="segmented" role="radiogroup" aria-label="Approvals">
        <button role="radio" aria-checked={mode === 'auto'} className={mode === 'auto' ? 'on' : ''} disabled={busy} onClick={() => onChange('auto')}>
          Automatic
        </button>
        <button role="radio" aria-checked={mode === 'manual'} className={mode === 'manual' ? 'on' : ''} disabled={busy} onClick={() => onChange('manual')}>
          Ask me first
        </button>
      </div>
    </div>
  );
}

const WEEKDAYS = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];

/** "Wed, Oct 7, 03:00" in the viewer's locale. */
const dateTime = (iso) => new Date(iso).toLocaleString(undefined, { weekday: 'short', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });

/** A stored schedule as the editor's choices: Daily or Weekly at a time, a cron line, or Off. */
function schedulePreset(s) {
  const m = /^([0-9]{1,2}) ([0-9]{1,2}) \* \* (\*|[0-7])$/.exec(s || '');
  if (m && +m[1] < 60 && +m[2] < 24) {
    const time = `${m[2].padStart(2, '0')}:${m[1].padStart(2, '0')}`;
    return { kind: m[3] === '*' ? 'daily' : 'weekly', time, day: m[3] === '*' ? 1 : +m[3] % 7, cron: s };
  }
  if (!s || s === 'off') return { kind: 'off', time: '03:00', day: 1, cron: '' };
  return { kind: 'custom', time: '03:00', day: 1, cron: s };
}

/** The editor's choices as the schedule to save. */
function scheduleOf(p) {
  const [h, m] = p.time.split(':').map(Number);
  if (p.kind === 'daily') return `${m} ${h} * * *`;
  if (p.kind === 'weekly') return `${m} ${h} * * ${p.day}`;
  if (p.kind === 'custom') return p.cron.trim();
  return 'off';
}

/** The Cleanup schedule under the Approvals switch: when the curator reviews the pages that changed
 * since their last review (daily at 03:00 by default), with Daily, Weekly, Custom (cron) and Off. */
function CleanupSchedule({ refreshKey }) {
  const [key, setKey] = useState(0);
  const settings = useResource(api.settings, `${refreshKey}|${key}`);
  const [edit, setEdit] = useState(null);
  const [preview, setPreview] = useState(null);
  const [busy, setBusy] = useState(false);
  const draft = edit ? scheduleOf(edit) : null;
  useEffect(() => {
    if (draft === null) return;
    let live = true;
    const t = setTimeout(() => {
      if (!draft) return setPreview({ error: 'Type a cron schedule: minute hour day month weekday.' });
      api.cleanupPreview(draft).then(
        (p) => live && setPreview(p),
        (e) => live && setPreview({ error: e.message }),
      );
    }, 200);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [draft]);
  const c = settings.data?.cleanup;
  if (!c) return null;
  const save = async () => {
    setBusy(true);
    try {
      const r = await api.setCleanupSchedule(draft);
      Toast.show(r.cleanup.schedule === 'off' ? 'Scheduled cleanups are off' : `Cleanup schedule: ${r.cleanup.description}`);
      setEdit(null);
      setKey((k) => k + 1);
    } catch (e) {
      Toast.show(e.message);
    }
    setBusy(false);
  };
  const open = () => {
    setPreview(null);
    setEdit(schedulePreset(c.schedule));
  };
  // Custom starts from what Daily or Weekly shows, so a small change stays a small edit.
  const choose = (kind) => setEdit({ ...edit, kind, cron: kind === 'custom' && (edit.kind === 'daily' || edit.kind === 'weekly') ? scheduleOf(edit) : edit.cron || scheduleOf({ ...edit, kind: 'daily' }) });
  const off = c.schedule === 'off' || c.offOnThisComputer;
  const when = c.offOnThisComputer
    ? 'Off on this computer: its config.json sets curator.lint to "off".'
    : c.schedule === 'off'
      ? 'Pages are not reviewed for cleanups.'
      : c.due
        ? 'A cleanup is due: it runs when the curator has nothing to file.'
        : c.next
          ? `Next cleanup ${ago(c.next)} (${dateTime(c.next)}).`
          : 'This schedule never comes round.';
  const invalid = edit?.kind === 'custom' && Boolean(preview?.error);
  return (
    <div className={`approvals-bar schedule-bar${off ? ' mode-off' : ''}`}>
      <div className="grow">
        <div className="curator-title">Cleanup schedule: {c.description}</div>
        <div className="muted small">
          {when}
          {c.lastPass ? ` The last one finished ${ago(c.lastPass)}.` : ''}
          {off ? '' : ' Each cleanup reviews the pages that changed since their last review.'}
        </div>
        {c.problem && <div className="held-why">{sentence(c.problem)}</div>}
      </div>
      {!edit && (
        <button className="btn" onClick={open}>
          Change
        </button>
      )}
      {edit && (
        <div className="schedule-editor">
          <div className="schedule-fields">
            <div className="segmented" role="radiogroup" aria-label="Cleanup schedule">
              {[
                ['daily', 'Daily'],
                ['weekly', 'Weekly'],
                ['custom', 'Custom'],
                ['off', 'Off'],
              ].map(([k, label]) => (
                <button key={k} role="radio" aria-checked={edit.kind === k} className={edit.kind === k ? 'on' : ''} disabled={busy} onClick={() => choose(k)}>
                  {label}
                </button>
              ))}
            </div>
            {edit.kind === 'weekly' && (
              <select className="forget-input" aria-label="Day" value={edit.day} onChange={(e) => setEdit({ ...edit, day: +e.target.value })}>
                {WEEKDAYS.map((d, i) => (
                  <option key={d} value={i}>
                    {d}
                  </option>
                ))}
              </select>
            )}
            {(edit.kind === 'daily' || edit.kind === 'weekly') && (
              <input type="time" className="forget-input" aria-label="Time" value={edit.time} onChange={(e) => e.target.value && setEdit({ ...edit, time: e.target.value })} />
            )}
            {edit.kind === 'custom' && (
              <input
                className="forget-input cron-input"
                aria-label="Cron schedule"
                value={edit.cron}
                placeholder="0 3 * * 1-5"
                spellCheck={false}
                autoFocus
                onChange={(e) => setEdit({ ...edit, cron: e.target.value })}
                onKeyDown={(e) => e.key === 'Enter' && !invalid && !busy && save()}
              />
            )}
          </div>
          {edit.kind === 'custom' && (
            <div className="muted small">
              Cron, in this computer's time: minute, hour, day of month, month, weekday. For example <code>0 3 * * 1-5</code> is weekdays at 03:00, and <code>0 */6 * * *</code> every six hours.
            </div>
          )}
          <div className="muted small">
            {edit.kind === 'off' ? (
              'No scheduled cleanups, and pages that grow past the size cap are not split.'
            ) : preview?.error ? (
              <span className="bad">{preview.error}</span>
            ) : preview?.next ? (
              preview.next.length ? `${preview.description}. Next: ${preview.next.map(dateTime).join(' · ')}` : `${preview.description}: this schedule never comes round.`
            ) : (
              ' '
            )}
          </div>
          <div className="held-actions">
            <button className="btn primary" disabled={busy || invalid || !preview} onClick={save}>
              Save
            </button>
            <button className="btn" disabled={busy} onClick={() => setEdit(null)}>
              Cancel
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

/** What was applied automatically in the last 7 days. A cleanup that touched several pages (a split)
 * is one card, and Undo puts all of its pages back. */
function AutoApplied({ items, busy, onUndo }) {
  const groups = [];
  for (const a of items) {
    const g = a.kind === 'lint' && groups.find((x) => x.kind === 'lint' && x.batch === a.batch);
    if (g) g.items.push(a);
    else groups.push({ kind: a.kind, batch: a.batch, items: [a] });
  }
  if (!groups.length) return null;
  return (
    <section className="held">
      <h3 className="section-label">Applied automatically · last 7 days</h3>
      {groups.map(({ kind, batch, items: [first, ...more] }) => {
        const all = [first, ...more];
        const why = first.reasons?.[0] || '';
        return (
          <div key={`${batch}/${first.index}`} className={`card auto-card${first.status === 'undone' ? ' undone' : ''}`}>
            <div className="card-head">
              <Icon name="page" size={16} />
              <span className="card-title">
                {all.map((a, i) => (
                  <span key={a.index}>
                    {i > 0 && ', '}
                    {a.status === 'undone' && a.action === 'created' ? a.title : <a href={`#/page/${a.slug}`}>{a.title}</a>}
                  </span>
                ))}
              </span>
              <span className="chip">{kind === 'lint' ? 'cleanup' : 'change'}</span>
              <span className="muted small">{ago(first.at)}</span>
            </div>
            {why && <div className="held-why">{sentence(kind === 'lint' ? why.charAt(0).toUpperCase() + why.slice(1) : `Would have waited because ${why}`)}</div>}
            <details className="auto-diff">
              <summary className="muted small">Show the change</summary>
              {all.map((a) => (
                <div key={a.index}>
                  {all.length > 1 && <div className="muted small">{a.title}</div>}
                  <Diff header={a.header} diff={a.diff} />
                </div>
              ))}
            </details>
            <div className="held-actions">
              {first.status === 'undone' ? (
                <span className="muted small">Undone</span>
              ) : all.every((a) => a.undoable) ? (
                <button className="btn" disabled={busy} onClick={() => onUndo(first, all)}>
                  Undo
                </button>
              ) : (
                <span className="muted small">The page changed since, so Undo is no longer possible. Edit it, or revert a later change in Activity.</span>
              )}
            </div>
          </div>
        );
      })}
    </section>
  );
}

/** Held changes, cleanups and forget requests waiting for the person. */
function NeedsOk({ mode, changes, forgets, busy, onDecide, onForget, onApproveAll }) {
  if (!changes.length && !forgets.length) return null;
  const cleanups = changes.filter((c) => c.kind === 'lint');
  return (
    <section className="held">
      <h3 className="section-label">Needs your OK · {changes.length + forgets.length}</h3>
      {mode === 'auto' && changes.length > 0 && (
        <p className="muted small">These could not be applied automatically, usually because the page changed after they were proposed. Approve, reject or send each one back.</p>
      )}
      {cleanups.length > 1 && (
        <div className="held-actions held-all">
          <span className="muted small">{cleanups.length} proposed cleanups are waiting.</span>
          <button className="btn" disabled={busy} onClick={() => onApproveAll(cleanups)}>
            Approve all cleanups
          </button>
        </div>
      )}
      {forgets.map((f) => (
        <div key={`forget/${f.batch}/${f.index}`} className="card held-card">
          <div className="card-head">
            <span className="card-title">Forget “{f.text}”</span>
            <span className="chip">forget</span>
          </div>
          <div className="held-why">
            You asked the wiki to forget this. It is in {f.matches} place{f.matches === 1 ? '' : 's'}:
          </div>
          <div className="card-meta">
            {f.files.slice(0, 12).map((x) => (
              <span key={x} className="tag">
                {x}
              </span>
            ))}
            {f.files.length > 12 && <span className="muted small">and {f.files.length - 12} more</span>}
          </div>
          <div className="held-actions">
            <button className="btn primary" disabled={busy || !f.matches} onClick={() => onForget(f, 'approve')}>
              Forget everywhere
            </button>
            <button className="btn" disabled={busy} onClick={() => onForget(f, 'reject')}>
              Dismiss
            </button>
          </div>
        </div>
      ))}
      {changes.map((c) => (
        <div key={`${c.batch}/${c.index}`} className="card held-card">
          <div className="card-head">
            <Icon name="page" size={16} />
            <span className="card-title">
              {c.kind === 'lint' && <span className="muted">Proposed cleanup: </span>}
              {c.action === 'created' ? c.title : <a href={`#/page/${c.slug}`}>{c.title}</a>}
            </span>
            <span className="chip">{ACTION_LABEL[c.action] || c.action}</span>
          </div>
          {c.reasons.map((r) => (
            <div key={r} className="held-why">{sentence(`Held because ${r}`)}</div>
          ))}
          {c.autoFailed && <div className="held-why">{sentence(`Not applied automatically: ${c.autoFailed}`)}</div>}
          {c.stale && !c.autoFailed && <div className="muted small">The page changed since. Approving applies the change only if it still fits.</div>}
          <div className="card-meta">
            {c.notes.map((n) => (
              <a key={n.id} href={`#/note/${n.id}`} className="tag" title={n.title}>
                {n.app} · {n.source}
              </a>
            ))}
            {c.reason && <span className="muted small">{c.reason}</span>}
          </div>
          <Diff header={c.header} diff={c.diff} />
          <div className="held-actions">
            <button className="btn primary" disabled={busy} onClick={() => onDecide(c, 'approve')}>
              Approve
            </button>
            <button className="btn" disabled={busy} onClick={() => onDecide(c, 'reject')}>
              Reject
            </button>
            <button className="ghost" disabled={busy} onClick={() => onDecide(c, 'refile')} title="Put its notes back in the inbox, so the curator plans them again on the current pages">
              Send back
            </button>
          </div>
        </div>
      ))}
    </section>
  );
}

function HeldChanges({ refreshKey }) {
  const [key, setKey] = useState(0);
  const held = useResource(api.held, `${refreshKey}|${key}`);
  const [busy, setBusy] = useState(false);
  if (!held.data) return null;
  const { changes = [], forget: forgets = [], auto = [], approvals: mode = 'auto', approvalsProblem } = held.data;
  /** Runs one window action, shows its outcome, and reloads the list. */
  const run = async (fn, done) => {
    setBusy(true);
    try {
      Toast.show(done(await fn()));
    } catch (e) {
      Toast.show(e.message);
    }
    setBusy(false);
    setKey((k) => k + 1);
  };
  const setMode = (m) =>
    m !== mode &&
    run(
      () => api.setApprovals(m),
      (r) => (m === 'manual' ? 'Changes will wait for your OK' : r.applied ? `Automatic: applied ${r.applied} waiting change${r.applied === 1 ? '' : 's'}` : 'Changes will be applied automatically'),
    );
  const undo = (first, all) => {
    const what = first.kind === 'lint' ? `the cleanup of ${all.map((a) => `"${a.title}"`).join(', ')}` : `the change to "${first.title}"`;
    if (!confirm(`Undo ${what}? ${all.length > 1 ? 'The pages go' : 'The page goes'} back to how ${all.length > 1 ? 'they were' : 'it was'} before.`)) return;
    run(
      () => api.undoHeld(first.batch, first.index),
      () => `Undone: ${first.title}`,
    );
  };
  const decide = (c, action) =>
    run(
      () => api.decideHeld(c.batch, c.index, action),
      () => (action === 'approve' ? `Applied to ${c.title}` : action === 'reject' ? 'Rejected' : 'Sent back to the curator'),
    );
  const forget = (f, action) => {
    if (action === 'approve' && !confirm(`Redact every copy of this text (${f.matches} in ${f.files.length} file(s))? This cannot be undone.`)) return;
    run(
      () => api.decideForget(f.batch, f.index, action),
      (r) => (action === 'approve' ? `Forgotten: ${r.matches} copies redacted` : 'Dismissed'),
    );
  };
  const approveAll = (cleanups) => {
    if (!confirm(`Apply all ${cleanups.length} proposed cleanups?`)) return;
    run(
      async () => {
        let failed = 0;
        for (const c of cleanups) await api.decideHeld(c.batch, c.index, 'approve').catch(() => failed++);
        return failed;
      },
      (failed) => (failed ? `${cleanups.length - failed} applied, ${failed} could not be` : `Applied ${cleanups.length} cleanups`),
    );
  };
  return (
    <>
      <ApprovalsBar mode={mode} problem={approvalsProblem} busy={busy} onChange={setMode} />
      <CleanupSchedule refreshKey={refreshKey} />
      <NeedsOk mode={mode} changes={changes} forgets={forgets} busy={busy} onDecide={decide} onForget={forget} onApproveAll={approveAll} />
      <AutoApplied items={auto} busy={busy} onUndo={undo} />
    </>
  );
}

function InboxView({ status, render, refreshKey }) {
  const box = useResource(api.inbox, refreshKey);
  const held = status?.held || 0;
  return (
    <>
      <CuratorBar status={status} />
      <HeldChanges refreshKey={refreshKey} />
      {box.error && !box.data ? (
        <Empty icon="note" title="Could not load the inbox">{box.error.message}</Empty>
      ) : !box.data ? (
        <Skeleton rows={2} />
      ) : !box.data.notes.length ? (
        held ? null : <Empty title="Inbox zero">Every note has been filed into the wiki.</Empty>
      ) : (
        <div className="list">
          {box.data.notes.map((n) => (
            <div key={n.id} className="card note-card">
              <div className="card-head">
                <AppBadge app={n.app} size={22} />
                <span className="card-title">{n.title}</span>
                <span className={`chip st-${n.status}`}>{NOTE_STATUS[n.status]}{n.attempts ? ` · ${n.attempts}` : ''}</span>
              </div>
              {n.body && <div className="md small-md clamp" dangerouslySetInnerHTML={{ __html: render(n.body) }} />}
              <div className="card-meta">
                <span>{n.app}</span>
                <span className="sep">·</span>
                <span>{ago(n.submitted)}</span>
                {n.kind === 'page' && <span className="tag">page hand-over</span>}
                {n.pages.map((s) => (
                  <a key={s} href={`#/page/${s}`} className="tag">
                    {s}
                  </a>
                ))}
              </div>
              {n.lastError && <div className="error-line">{n.lastError}</div>}
            </div>
          ))}
        </div>
      )}
    </>
  );
}

// ---------------------------------------------------------------- ask (agentic search)
//
// A question goes to the curator (it runs as you, with your ChatGPT sign-in), whose agent searches
// and reads the wiki with the read-only tools. The window shows each step as it happens, then the
// answer with the files it rests on. Follow-ups carry the conversation along.

const QUESTION_RE = /\?\s*$|^(who|what|when|where|why|how|which|whose|did|does|do|is|are|was|were|can|could|should|has|have|had|will|would|tell|explain|list|summari[sz]e|find)\b/i;
const looksLikeQuestion = (q) => QUESTION_RE.test(q.trim()) || q.trim().split(/\s+/).length >= 6;
const LIVE = new Set(['queued', 'running', 'stalled']);

/** Why asking is not possible right now, or '' when it is. */
function askBlocker(status, offline) {
  if (offline) return 'The Agent Wiki service is not answering.';
  if (!status) return 'Connecting…';
  if (!status.ask?.running) return 'Asking needs the curator, which runs in the Agent Wiki tray app. Start Agent Wiki from the Start menu.';
  if (status.ask.signedIn === false) return 'The curator is signed out of ChatGPT. Right-click the tray icon > Curator > Sign in.';
  return '';
}

/** Queues a question and opens it. Returns true, or throws with the service's reason. */
async function startAsk(question, parent) {
  const r = await api.startAsk(question, parent);
  go(`#/ask/${r.id}`);
  return true;
}

/** Re-renders every `ms` while `on`. */
function useTicker(on, ms = 1000) {
  const [, set] = useState(0);
  useEffect(() => {
    if (!on) return;
    const t = setInterval(() => set((n) => n + 1), ms);
    return () => clearInterval(t);
  }, [on, ms]);
}

function Composer({ onSubmit, placeholder, autoFocus, blocker, compact }) {
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const ref = useRef(null);
  // Grow with the text (up to ~7 lines). Empty: leave it to the stylesheet, which may not have applied yet on mount.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = '';
    if (!text) return;
    el.style.height = 'auto';
    el.style.height = `${Math.min(el.scrollHeight, 168)}px`;
  }, [text]);
  useEffect(() => {
    if (autoFocus) ref.current?.focus();
  }, [autoFocus]);
  const submit = async () => {
    const q = text.trim();
    if (!q || busy || blocker) return;
    setBusy(true);
    setError('');
    try {
      await onSubmit(q);
      setText('');
    } catch (e) {
      setError(e.message);
    }
    setBusy(false);
  };
  return (
    <div className={`composer-wrap${compact ? ' compact' : ''}`}>
      <div className={`composer${blocker ? ' blocked' : ''}${busy ? ' busy' : ''}`}>
        <span className="composer-mark"><Icon name="spark" size={17} /></span>
        <textarea
          ref={ref}
          rows={1}
          value={text}
          maxLength={2000}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault();
              submit();
            } else if (e.key === 'Escape') e.currentTarget.blur();
          }}
          placeholder={placeholder}
          aria-label={placeholder}
        />
        <button className="send" disabled={!text.trim() || busy || Boolean(blocker)} onClick={submit} title="Ask (Enter; Shift+Enter for a new line)">
          {busy ? <span className="spinner" /> : <Icon name="send" size={16} />}
        </button>
      </div>
      {(error || blocker) && <div className={error ? 'error-line' : 'notice'}>{error || blocker}</div>}
    </div>
  );
}

/** Questions that fit this wiki: its most recently updated project, and what changed lately. */
function suggestions(pages) {
  const list = pages?.pages || [];
  const project = list.filter((p) => p.type === 'project').sort((a, b) => b.time - a.time)[0];
  const pref = list.find((p) => p.type === 'preference');
  const out = ['What changed in the wiki today?'];
  if (project) out.push(`What is still open for ${project.title}?`);
  if (pref) out.push(`What are my standing preferences?`);
  else if (list[0]) out.push(`What do I know about ${list[0].title}?`);
  return out;
}

const ASK_STATUS = {
  queued: ['Waiting', 'st-pending'],
  running: ['Answering', 'st-pending'],
  stalled: ['Stalled', 'st-retrying'],
  done: ['Answered', 'st-done'],
  error: ['Failed', 'st-dead'],
  cancelled: ['Stopped', ''],
};

function AskStatusChip({ status, found }) {
  if (status === 'done' && found === false) return <span className="chip st-retrying">Not in the wiki</span>;
  const [label, cls] = ASK_STATUS[status] || [status, ''];
  return (
    <span className={`chip ${cls}`}>
      {LIVE.has(status) && status !== 'stalled' && <span className="chip-spin" />}
      {label}
    </span>
  );
}

function AskView({ status, offline, pages }) {
  const asking = (status?.ask?.active || []).length;
  const list = useResource(api.asks, `${asking}`);
  const blocker = askBlocker(status, offline);
  const asks = list.data?.asks || [];
  return (
    <div className="ask-home">
      <div className="ask-hero">
        <div className="ask-hero-mark"><Icon name="spark" size={22} /></div>
        <div>
          <h2>Ask the wiki</h2>
          <p>An agent searches and reads your pages, the activity log and notes not yet filed, then answers with its sources.</p>
        </div>
      </div>
      <Composer autoFocus placeholder="Ask anything about your projects, decisions, setup…" blocker={blocker} onSubmit={(q) => startAsk(q)} />
      {!blocker && (
        <div className="suggest">
          {suggestions(pages.data).map((s) => (
            <button key={s} className="suggest-chip" onClick={() => startAsk(s).catch((e) => Toast.show(e.message))}>
              {s}
            </button>
          ))}
        </div>
      )}
      {asks.length > 0 && (
        <section className="recent">
          <h3 className="section-label">Recent questions</h3>
          <div className="list">
            {asks.map((a) => (
              <a key={a.id} className="card ask-card" href={`#/ask/${a.id}`}>
                <div className="card-head">
                  <span className="card-title">{a.turns > 1 ? a.first : a.question}</span>
                  <AskStatusChip status={a.status} found={a.found} />
                </div>
                {a.turns > 1 && <div className="ask-latest">Latest: {a.question}</div>}
                {a.preview && <p className="card-text">{a.preview}</p>}
                <div className="card-meta">
                  <span>{ago(a.createdAt)}</span>
                  {a.turns > 1 && (
                    <>
                      <span className="sep">·</span>
                      <span>{a.turns} questions</span>
                    </>
                  )}
                </div>
              </a>
            ))}
          </div>
        </section>
      )}
    </div>
  );
}

/** Merges each tool call's "running" and "done" events into one step. */
function stepsOf(events) {
  const steps = [];
  const byId = new Map();
  for (const e of events || []) {
    if (e.type === 'tool') {
      const key = `tool:${e.id}`;
      if (byId.has(key)) steps[byId.get(key)] = { ...steps[byId.get(key)], ...e };
      else {
        byId.set(key, steps.length);
        steps.push(e);
      }
    } else if (e.type === 'thought') steps.push(e);
  }
  return steps;
}

const hrefOf = (kind, target) => {
  const t = String(target || '').split('#')[0]; // "slug#section": the page
  return kind === 'page' ? `#/page/${t}` : kind === 'log' ? `#/day/${t}` : kind === 'note' ? noteHref(t) : null;
};
const kindIcon = (kind) => (kind === 'log' ? 'log' : kind === 'note' ? 'note' : 'page');
const targetTitle = (kind, target, title) => (kind === 'log' ? `${dayLabel(target)}, activity log` : title || target);

function TargetChip({ kind, target, title }) {
  const href = hrefOf(kind, target);
  const body = (
    <>
      <Icon name={kindIcon(kind)} size={12} /> {targetTitle(kind, target, title)}
    </>
  );
  return href ? <a className="link-chip small-chip" href={href}>{body}</a> : <span className="link-chip small-chip">{body}</span>;
}

function Step({ s }) {
  if (s.type === 'thought') {
    return (
      <li className="step thought">
        <span className="step-icon"><Icon name="spark" size={12} /></span>
        <span className="step-main">{s.text}</span>
      </li>
    );
  }
  const running = s.status === 'running';
  const search = s.tool === 'wiki_search';
  return (
    <li className={`step${running ? ' running' : ''}${s.status === 'error' ? ' failed' : ''}`}>
      <span className="step-icon">{running ? <span className="spinner" /> : <Icon name={search ? 'search' : kindIcon(s.kind)} size={13} />}</span>
      <span className="step-main">
        {search ? (
          <>
            <span>{running ? 'Searching for' : 'Searched for'} <q>{s.args?.query}</q></span>
            {s.status === 'done' && <span className="muted"> · {s.count ? `${s.count} result${s.count > 1 ? 's' : ''}` : 'nothing found'}</span>}
            {s.hits?.length > 0 && (
              <span className="step-hits">
                {s.hits.slice(0, 4).map((h) => (
                  <TargetChip key={`${h.kind}:${h.target}`} {...h} />
                ))}
                {s.count > 4 && <span className="muted small">+{s.count - 4} more</span>}
              </span>
            )}
          </>
        ) : running ? (
          <span>Reading <code>{s.args?.target}</code></span>
        ) : s.status === 'done' ? (
          <span>
            Read{' '}
            {hrefOf(s.kind, s.target) ? <a href={hrefOf(s.kind, s.target)}>{targetTitle(s.kind, s.target, s.title)}</a> : <code>{s.target || s.args?.target}</code>}
            {s.kind === 'page' && <span className="muted"> · page</span>}
          </span>
        ) : (
          <span>Read <code>{s.args?.target}</code></span>
        )}
        {s.status === 'error' && <span className="step-error">{s.error}</span>}
      </span>
    </li>
  );
}

const secs = (ms) => (ms < 10_000 ? `${(ms / 1000).toFixed(1)} s` : `${Math.round(ms / 1000)} s`);

/** A source excerpt as plain text: no Markdown, table cells separated by dots. */
const quoteText = (q) => plainText(String(q).replace(/^[\s|]+|[\s|]+$/g, '').replace(/(?:\s*\|\s*)+/g, ' · '), 300);

function activity(a, steps) {
  if (a.status === 'queued') return a.worker && !a.worker.running ? 'Waiting for the curator (it is not running)…' : 'Waiting for the curator…';
  if (a.status === 'stalled') return 'The curator stopped responding…';
  const last = steps.at(-1);
  if (last?.type === 'tool' && last.status === 'running') return last.tool === 'wiki_search' ? `Searching for “${last.args?.query}”…` : `Reading ${last.args?.target}…`;
  return steps.some((s) => s.type === 'tool') ? 'Thinking…' : 'Deciding where to look…';
}

function Trace({ a, steps, open, onToggle, onStop }) {
  const live = LIVE.has(a.status);
  useTicker(live);
  const started = Date.parse(a.startedAt || a.createdAt);
  const ms = live ? Date.now() - started : a.result?.ms ?? 0;
  const searches = steps.filter((s) => s.tool === 'wiki_search').length;
  const reads = steps.filter((s) => s.tool === 'wiki_read').length;
  const summary = live
    ? activity(a, steps)
    : [searches && `Searched ${searches === 1 ? 'once' : `${searches} times`}`, reads && `read ${reads} file${reads > 1 ? 's' : ''}`].filter(Boolean).join(', ') || 'Answered from the page index';
  return (
    <div className={`trace${live ? ' live' : ''}${open ? ' open' : ''}`}>
      <div className="trace-head">
        <button className="trace-toggle" onClick={onToggle} aria-expanded={open} disabled={!steps.length}>
          <span className="trace-state">{live ? <span className="spinner" /> : <Icon name="search" size={13} />}</span>
          <span className="trace-label">{summary.charAt(0).toUpperCase() + summary.slice(1)}</span>
          {ms > 0 && <span className="time">{secs(ms)}</span>}
          {steps.length > 0 && <Icon name="chevron" size={14} />}
        </button>
        {live && (
          <button className="ghost stop" onClick={onStop} title="Stop">
            <Icon name="stop" size={13} /> Stop
          </button>
        )}
      </div>
      {open && steps.length > 0 && (
        <ol className="steps">
          {steps.map((s, i) => (
            <Step key={`${s.type}:${s.id}:${i}`} s={s} />
          ))}
        </ol>
      )}
    </div>
  );
}

function Answer({ r, render }) {
  return (
    <div className={`answer${r.found === false ? ' not-found' : ''}`}>
      {r.found === false && (
        <div className="answer-flag">
          <Icon name="search" size={13} /> The wiki does not say
        </div>
      )}
      <div className="md" dangerouslySetInnerHTML={{ __html: render(r.answer) }} />
      {r.sources?.length > 0 && (
        <div className="sources">
          <h3 className="section-label">Sources</h3>
          <div className="source-list">
            {r.sources.map((s, i) => {
              const href = hrefOf(s.kind, s.target);
              const Tag = href ? 'a' : 'div';
              return (
                <Tag key={`${s.kind}:${s.target}`} className="source" {...(href ? { href } : {})}>
                  <span className="source-n">{i + 1}</span>
                  <span className="source-body">
                    <span className="source-title">
                      <Icon name={kindIcon(s.kind)} size={13} />
                      <span>{targetTitle(s.kind, s.target, s.title)}</span>
                      {s.type && <TypeChip type={s.type} />}
                    </span>
                    {s.quote && <span className="source-quote">{quoteText(s.quote)}</span>}
                  </span>
                </Tag>
              );
            })}
          </div>
        </div>
      )}
      <div className="answer-foot">
        <span className="muted small">
          {r.model} · {r.reasoningEffort} · {secs(r.ms || 0)}
        </span>
        <span className="grow" />
        <button className="ghost" onClick={() => copy(r.answer, 'the answer')} title="Copy the answer as Markdown">
          <Icon name="copy" /> Copy
        </button>
      </div>
    </div>
  );
}

function Turn({ a, render, onRetry, live }) {
  const steps = useMemo(() => stepsOf(a.events), [a.events]);
  const running = LIVE.has(a.status);
  const [open, setOpen] = useState(running);
  useEffect(() => {
    if (!running) setOpen(false); // collapse the trace once the answer is in
  }, [running]);
  const r = a.result;
  const stop = () => api.cancelAsk(a.id).catch((e) => Toast.show(e.message));
  return (
    <section className={`turn${live ? ' latest' : ''}`}>
      <div className="question">
        <h2>{a.question}</h2>
        <span className="time">{ago(a.createdAt)}</span>
      </div>
      <Trace a={a} steps={steps} open={open} onToggle={() => setOpen(!open)} onStop={stop} />
      {r?.status === 'done' && <Answer r={r} render={render} />}
      {r && r.status !== 'done' && (
        <div className={`card ask-failed${r.status === 'cancelled' ? ' quiet' : ''}`}>
          <div className="grow">{r.error || 'No answer.'}</div>
          {onRetry && (
            <button className="btn" onClick={onRetry}>
              <Icon name="retry" size={14} /> Ask again
            </button>
          )}
        </div>
      )}
      {a.status === 'stalled' && <div className="error-line">The curator stopped responding while answering. If the tray app was closed, start it and ask again.</div>}
    </section>
  );
}

function AskThread({ id, status, offline, render }) {
  const [s, setS] = useState({ ask: null, thread: [], error: null });
  const lastRef = useRef(null);
  useEffect(() => {
    let alive = true;
    let timer;
    let after = 0;
    let events = [];
    let first = true;
    const load = async () => {
      try {
        const a = await api.ask(id, after, first);
        if (!alive) return;
        events = first ? a.events : [...events, ...a.events];
        after = a.eventCount;
        const thread = a.thread;
        setS((p) => ({ ask: { ...a, events }, thread: first ? thread || [] : p.thread, error: null }));
        first = false;
        if (LIVE.has(a.status)) timer = setTimeout(load, document.hidden ? 3000 : 600);
      } catch (e) {
        if (!alive) return;
        setS((p) => ({ ...p, error: e }));
        if (e.status !== 404) timer = setTimeout(load, 3000);
      }
    };
    load();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [id]);
  useEffect(() => {
    if (s.ask?.id === id && s.thread.length) lastRef.current?.scrollIntoView({ block: 'start' });
  }, [s.ask?.id, s.thread.length, id]);

  if (s.error && !s.ask) {
    return (
      <div className="page">
        <BackBar />
        <Empty icon="spark" title={s.error.status === 404 ? 'That question is gone' : 'Could not load the question'}>
          {s.error.status === 404 ? 'Questions are kept for two weeks.' : s.error.message}
        </Empty>
      </div>
    );
  }
  if (!s.ask) return <Skeleton rows={2} />;
  const a = s.ask;
  const blocker = askBlocker(status, offline);
  const retry = () => startAsk(a.question, a.parent).catch((e) => Toast.show(e.message));
  return (
    <div className="ask-thread">
      <div className="backbar">
        <button className="ghost" onClick={() => go('#/ask')} title="All questions">
          <Icon name="back" /> Questions
        </button>
        <span className="grow" />
        <button className="ghost" onClick={() => go('#/ask')} title="Start a new conversation">
          <Icon name="spark" /> New question
        </button>
      </div>
      {s.thread.map((t) => (
        <Turn key={t.id} a={t} render={render} />
      ))}
      <div ref={lastRef}>
        <Turn a={a} render={render} onRetry={a.status === 'error' || a.status === 'cancelled' ? retry : null} live />
      </div>
      {!LIVE.has(a.status) && (
        <Composer compact placeholder="Ask a follow-up…" blocker={blocker} onSubmit={(q) => startAsk(q, a.id)} autoFocus={a.status === 'done'} />
      )}
    </div>
  );
}

// ---------------------------------------------------------------- status

function Row({ k, children }) {
  return (
    <div className="row">
      <span className="k">{k}</span>
      <span className="v">{children}</span>
    </div>
  );
}

function StatusView({ status, offline }) {
  if (!status) return offline ? <Empty icon="close" title="The service is not answering">Right-click the tray icon and choose Restart service.</Empty> : <Skeleton rows={3} />;
  const c = status.curator || {};
  const q = status.queue || {};
  const ask = status.ask || {};
  return (
    <div className="status">
      {status.reasons?.length > 0 && (
        <div className="card warn-card">
          <div className="card-title">Needs attention</div>
          <ul>
            {status.reasons.map((r) => (
              <li key={r}>{r}</li>
            ))}
          </ul>
        </div>
      )}
      <ModelsCard />
      <div className="card">
        <h3 className="section-label">Service</h3>
        <Row k="Version">{status.version} <span className="muted">· build {status.build}</span></Row>
        <Row k="Up">{duration(status.uptimeSec)} <span className="muted">· pid {status.pid}</span></Row>
        <Row k="Writes">{status.writeMode === 'curated' ? 'queued for the curator' : 'written directly'}</Row>
        <Row k="MCP URL">
          <button className="linkish" onClick={() => copy(status.mcpUrl, 'the MCP URL')}>{status.mcpUrl} <Icon name="copy" size={12} /></button>
        </Row>
        <Row k="Wiki folder">
          <button className="linkish" onClick={() => copy(status.wikiDir, 'the wiki folder')}>{status.wikiDir} <Icon name="copy" size={12} /></button>
        </Row>
        <Row k="Requests">{status.requests?.last15m ?? 0} in the last 15 min{status.requests?.last ? ` · last ${ago(status.requests.last)}` : ''}</Row>
        {status.lock && <Row k="Write lock">{status.lock.state} by {status.lock.label || `pid ${status.lock.pid}`} ({status.lock.ageSec} s)</Row>}
      </div>
      <div className="card">
        <h3 className="section-label">Curator</h3>
        <Row k="State">{!c.running ? 'not running' : c.paused ? 'paused' : c.state}</Row>
        <Row k="Model">{c.model ? `${c.model}, ${c.reasoningEffort} reasoning` : '—'}</Row>
        <Row k="Signed in">{c.signedIn ? 'yes, ChatGPT' : c.signedIn === false ? 'no: tray > Curator > Sign in' : '—'}</Row>
        <Row k="Approvals">
          {status.approvals === 'manual' ? 'ask me first' : 'automatic, with a notification'} <a href="#/inbox">change</a>
        </Row>
        {status.cleanup && (
          <Row k="Cleanups">
            {status.cleanup.offOnThisComputer ? 'off on this computer (config.json)' : status.cleanup.description}
            {status.cleanup.due ? ' · one is due' : status.cleanup.next ? ` · next ${ago(status.cleanup.next)}` : ''} <a href="#/inbox">change</a>
          </Row>
        )}
        <Row k="Last run">
          {c.lastRun?.at ? `${ago(c.lastRun.at)} · ${c.lastRun.notes} note(s) · ${(c.lastRun.ms / 1000).toFixed(0)} s · ${c.lastRun.result}` : 'not yet'}
        </Row>
        {c.lastError && <Row k="Last error"><span className="bad">{c.lastError}</span></Row>}
      </div>
      <div className="card">
        <h3 className="section-label">Ask</h3>
        <Row k="Agent">{ask.running ? (ask.active?.length ? `answering ${ask.active.length} question${ask.active.length > 1 ? 's' : ''}` : 'ready') : 'not running: it runs with the curator in the tray app'}</Row>
        <Row k="Model">{ask.model ? `${ask.model}, ${ask.reasoningEffort} reasoning` : '—'}</Row>
        <Row k="Tools">wiki_search and wiki_read only (read-only)</Row>
      </div>
      <div className="card">
        <h3 className="section-label">Queue</h3>
        <div className="stats">
          <div><b>{q.pending ?? 0}</b><span>waiting</span></div>
          <div><b>{q.retrying ?? 0}</b><span>retrying</span></div>
          <div className={q.dead ? 'bad' : ''}><b>{q.dead ?? 0}</b><span>failed</span></div>
        </div>
        {q.oldestPendingAt && <Row k="Oldest">{ago(q.oldestPendingAt)}</Row>}
      </div>
      <ForgetCard />
    </div>
  );
}

/** Reasoning levels to offer when the account's model list is not known yet. */
const EFFORTS = ['low', 'medium', 'high', 'xhigh'];

/** One role's model and reasoning effort: a model from this ChatGPT account's list, or the default.
 * Each change is saved at once and applies from the role's next run. */
function ModelRow({ role, label, what, o, busy, onSave }) {
  const [other, setOther] = useState(null);
  const r = o[role];
  const def = o.defaults[role];
  const list = o.available?.models || [];
  const info = (slug) => list.find((m) => m.slug === slug);
  const inUse = info(r.model);
  const efforts = inUse?.efforts?.length ? inUse.efforts : EFFORTS;
  const followsCurator = role === 'ask' && !def.model;
  const chooseModel = (slug) => {
    if (slug === '__other') return setOther('');
    // Keep the chosen effort when the new model offers it, else take the model's own default.
    const next = info(slug || (followsCurator ? o.curator.model : def.model));
    const effort = r.choice.reasoningEffort;
    onSave(role, slug || null, effort && next?.efforts?.length && !next.efforts.includes(effort) ? next.defaultEffort : effort);
  };
  return (
    <div className="model-row">
      <div className="model-label">
        <b>{label}</b>
        <span className="muted small">{what}</span>
      </div>
      {other === null ? (
        <select className="forget-input" aria-label={`${label} model`} value={r.choice.model || ''} disabled={busy} onChange={(e) => chooseModel(e.target.value)}>
          <option value="">{followsCurator ? `Same as the curator (${o.curator.model})` : `Default (${def.model})`}</option>
          {list
            .filter((m) => m.listed)
            .map((m) => (
              <option key={m.slug} value={m.slug} title={m.description}>
                {m.name}
              </option>
            ))}
          {list.some((m) => !m.listed) && (
            <optgroup label="Not in Codex's model picker">
              {list
                .filter((m) => !m.listed)
                .map((m) => (
                  <option key={m.slug} value={m.slug} title={m.description}>
                    {m.name}
                  </option>
                ))}
            </optgroup>
          )}
          {r.choice.model && !info(r.choice.model) && <option value={r.choice.model}>{r.choice.model} (not in this account's list)</option>}
          {!list.length && <option value="__other">Another model…</option>}
        </select>
      ) : (
        <span className="model-other">
          <input className="forget-input" aria-label={`${label} model name`} placeholder="gpt-6.1-sol" value={other} autoFocus onChange={(e) => setOther(e.target.value)} onKeyDown={(e) => e.key === 'Enter' && other.trim() && (onSave(role, other.trim(), r.choice.reasoningEffort), setOther(null))} />
          <button className="btn" disabled={busy || !other.trim()} onClick={() => (onSave(role, other.trim(), r.choice.reasoningEffort), setOther(null))}>
            Save
          </button>
          <button className="btn" onClick={() => setOther(null)}>
            Cancel
          </button>
        </span>
      )}
      <select className="forget-input" aria-label={`${label} reasoning`} value={r.choice.reasoningEffort || ''} disabled={busy} onChange={(e) => onSave(role, r.choice.model || null, e.target.value || null)}>
        <option value="">Default reasoning ({def.reasoningEffort})</option>
        {efforts.map((e) => (
          <option key={e} value={e}>
            {e} reasoning{inUse?.defaultEffort === e ? " (the model's default)" : ''}
          </option>
        ))}
        {r.choice.reasoningEffort && !efforts.includes(r.choice.reasoningEffort) && <option value={r.choice.reasoningEffort}>{r.choice.reasoningEffort} (not offered)</option>}
      </select>
      <div className="muted small model-note">
        In use: {inUse?.name || r.model}, {r.reasoningEffort} reasoning{inUse?.description ? ` · ${inUse.description}` : ''}
      </div>
    </div>
  );
}

/** The models the curator and Ask use, chosen from this ChatGPT account's list (Codex's own). */
function ModelsCard() {
  const [key, setKey] = useState(0);
  const res = useResource(api.models, key);
  const [busy, setBusy] = useState(false);
  const o = res.data;
  if (!o) return null;
  const save = async (role, model, reasoningEffort) => {
    setBusy(true);
    try {
      const r = await api.setModel(role, model, reasoningEffort);
      Toast.show(`${role === 'ask' ? 'Ask' : 'Curator'}: ${r[role].model}, ${r[role].reasoningEffort} reasoning, from the next run`);
    } catch (e) {
      Toast.show(e.message);
    }
    setBusy(false);
    setKey((k) => k + 1);
  };
  return (
    <div className="card">
      <h3 className="section-label">Models</h3>
      <ModelRow role="curator" label="Curator" what="files notes, runs cleanups" o={o} busy={busy} onSave={save} />
      <ModelRow role="ask" label="Ask" what="answers your questions" o={o} busy={busy} onSave={save} />
      {o.problems.map((p) => (
        <div key={p} className="held-why">
          {sentence(p)}
        </div>
      ))}
      <p className="muted small">
        {o.available ? `The lists are this ChatGPT account's models, as Codex last fetched them (${ago(o.available.fetchedAt)}).` : "This ChatGPT account's model list appears here after the curator's first run."} A change applies from the next run, with no restart. In a
        terminal: <code>agent-wiki models</code>.
      </p>
    </div>
  );
}

/** Forget a piece of text everywhere: pages and their history, notes, audit records, Ask, the log and the request logs. */
function ForgetCard() {
  const [text, setText] = useState('');
  const [ignoreCase, setIgnoreCase] = useState(false);
  const [found, setFound] = useState(null);
  const [failures, setFailures] = useState([]);
  const [busy, setBusy] = useState(false);
  const look = async () => {
    setBusy(true);
    try {
      setFound(await api.forget(text, ignoreCase));
    } catch (e) {
      Toast.show(e.message);
    }
    setBusy(false);
  };
  const redact = async () => {
    if (!confirm(`Redact all ${found.matches} copies? This cannot be undone.`)) return;
    setBusy(true);
    try {
      const r = await api.forget(text, ignoreCase, true);
      const failed = r.failed || [];
      setFailures(failed);
      if (failed.length) {
        Toast.show(`Incomplete: ${r.matches} copies redacted; ${failed.length} file(s) could not be redacted.`);
        // Keep the text and failure details so the person can fix access and try again.
        setFound(null);
      } else {
        Toast.show(`Forgotten: ${r.matches} copies redacted in ${r.files} file(s)`);
        setFound(null);
        setText('');
      }
    } catch (e) {
      Toast.show(e.message);
    }
    setBusy(false);
  };
  return (
    <div className="card">
      <h3 className="section-label">Forget</h3>
      <p className="muted small">Finds every copy of a piece of text (pages and their history, notes, audit records, Ask, the log, the request logs) and redacts it. A git history of the wiki is not changed.</p>
      <div className="forget-row">
        <input
          className="forget-input"
          value={text}
          placeholder="Exact text to forget"
          disabled={busy}
          onChange={(e) => {
            setText(e.target.value);
            setFound(null);
            setFailures([]);
          }}
          onKeyDown={(e) => e.key === 'Enter' && text.trim().length >= 4 && look()}
        />
        <label className="muted small">
          <input type="checkbox" checked={ignoreCase} disabled={busy} onChange={(e) => { setIgnoreCase(e.target.checked); setFound(null); setFailures([]); }} /> ignore case
        </label>
        <button className="btn" disabled={busy || text.trim().length < 4} onClick={look}>
          Find
        </button>
      </div>
      {failures.length > 0 && (
        <div role="alert" className="bad small">
          <p>Some files could not be redacted. Fix the errors below, then use Find to review the remaining copies.</p>
          {failures.map((failure, index) => <div key={index}>{failure}</div>)}
        </div>
      )}
      {found && (
        <>
          <div className="muted small">{found.matches ? `${found.matches} cop${found.matches === 1 ? 'y' : 'ies'} in ${found.found.length} file(s):` : 'Not found anywhere.'}</div>
          <div className="diff">
            {found.found.map((f) => (
              <div key={f.rel} className="diff-line d-ctx">
                {f.count}× {f.rel} · {f.sample}
              </div>
            ))}
          </div>
          {found.matches > 0 && (
            <div className="held-actions">
              <button className="btn primary" disabled={busy} onClick={redact}>
                Redact everywhere
              </button>
            </div>
          )}
        </>
      )}
    </div>
  );
}

// ---------------------------------------------------------------- the app

function App() {
  const route = useRoute();
  const { status, error: statusError } = useStatus();
  const offline = Boolean(statusError);
  const [query, setQuery] = useState('');
  const [results, setResults] = useState([]);
  const [selected, setSelected] = useState(0);
  const inputRef = useRef(null);
  // Lists reload when the wiki changes: a new note, a filed batch, a new log line.
  const refreshKey = status ? `${status.lastWrite?.at}|${status.queue?.pending}|${status.curator?.lastRun?.at}` : '';
  const pages = useResource(api.pages, refreshKey);
  const known = useMemo(() => new Map((pages.data?.pages || []).map((p) => [p.slug, p.title])), [pages.data]);
  const render = useMemo(() => createRenderer({ known: (s) => known.size === 0 || known.has(s) }), [known]);
  // Answers read as prose: [[slug]] shows the page's title.
  const renderAnswer = useMemo(() => createRenderer({ known: (s) => known.size === 0 || known.has(s), title: (s) => known.get(s) }), [known]);

  const open = useCallback((r) => {
    setQuery('');
    go(resultHref(r));
  }, []);

  useEffect(() => {
    const onKey = (e) => {
      const typing = document.activeElement === inputRef.current;
      if ((e.key === '/' && !typing && !/^(INPUT|TEXTAREA)$/.test(document.activeElement?.tagName)) || ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k')) {
        e.preventDefault();
        inputRef.current?.focus();
        inputRef.current?.select();
      } else if (e.key === 'Escape' && !typing) {
        if (route.view === 'page' || route.view === 'day' || route.view === 'note') back();
      } else if (e.key === 'BrowserBack' || (e.altKey && e.key === 'ArrowLeft')) back();
    };
    addEventListener('keydown', onKey);
    return () => removeEventListener('keydown', onKey);
  }, [route.view]);

  const blocker = askBlocker(status, offline);
  const askFromSearch = () => {
    const q = query.trim();
    if (!q) return;
    if (blocker) return Toast.show(blocker);
    startAsk(q).then(
      () => setQuery(''),
      (e) => Toast.show(e.message),
    );
  };
  // Row 0 is "Ask the wiki"; search results follow from 1.
  const onSearchKey = (e) => {
    if (e.key === 'Escape') {
      if (query) setQuery('');
      else inputRef.current?.blur();
    } else if (e.key === 'ArrowDown') {
      e.preventDefault();
      setSelected((s) => Math.min(s + 1, results.length));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setSelected((s) => Math.max(s - 1, 0));
    } else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey || selected === 0)) {
      e.preventDefault();
      askFromSearch();
    } else if (e.key === 'Enter' && results[selected - 1]) {
      open(results[selected - 1]);
    }
  };
  useEffect(() => {
    if (!query.trim()) setResults([]);
  }, [query]);
  useEffect(() => {
    document.querySelector('.result.sel')?.scrollIntoView({ block: 'nearest' });
  }, [selected]);

  const searching = Boolean(query.trim());
  let view;
  if (searching) {
    view = <SearchResults query={query} selected={selected} setSelected={setSelected} onOpen={open} results={results} setResults={setResults} onAsk={askFromSearch} canAsk={!blocker} />;
  } else if (route.view === 'ask' && route.arg) view = <AskThread key={route.arg} id={route.arg} status={status} offline={offline} render={renderAnswer} />;
  else if (route.view === 'ask') view = <AskView status={status} offline={offline} pages={pages} />;
  else if (route.view === 'page' && route.arg) view = <PageView slug={route.arg} render={render} refreshKey={refreshKey} />;
  else if (route.view === 'activity') view = <ActivityView render={render} refreshKey={refreshKey} />;
  else if (route.view === 'day' && route.arg) view = <DayView date={route.arg} render={render} />;
  else if (route.view === 'note' && route.arg) view = <NoteView id={route.arg} render={render} />;
  else if (route.view === 'inbox') view = <InboxView status={status} render={render} refreshKey={refreshKey} />;
  else if (route.view === 'status') view = <StatusView status={status} offline={offline} />;
  else view = <PagesView pages={pages} />;

  const waiting = (status?.queue?.pending || 0) + (status?.queue?.dead || 0) + (status?.held || 0);
  return (
    <div className="app">
      <div className="chrome">
        <Header status={status} offline={offline} />
        <SearchBox value={query} onChange={setQuery} inputRef={inputRef} onKeyDown={onSearchKey} />
        {!searching && <Tabs view={route.view} inboxCount={waiting} asking={(status?.ask?.active || []).length > 0} />}
      </div>
      {offline && status && <div className="banner">Lost contact with the service; showing what was loaded. Retrying…</div>}
      <main key={searching ? 'search' : `${route.view}/${route.arg}`} className="view">
        {view}
      </main>
      <Toaster />
    </div>
  );
}

createRoot(document.getElementById('root')).render(<App />);
