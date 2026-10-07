// Wiki Markdown -> HTML for the tray window. Pages are written by agents, so nothing in them runs:
// raw HTML is shown as text, links keep only http(s), mailto and in-app (#) targets, and
// [[slug]] / [[slug|text]] become links to the page inside the window.

import { Marked } from 'marked';

export const escapeHtml = (s) =>
  String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

const SAFE_HREF = /^(?:https?:|mailto:|#)/i;

/**
 * A Marked instance; `known(slug)` tells existing pages from links to pages not written yet, and
 * `title(slug)`, if given, labels a bare [[slug]] with the page's title.
 */
export function createRenderer({ known = () => true, title = () => '' } = {}) {
  const md = new Marked({ gfm: true, breaks: false });
  md.use({
    extensions: [
      {
        name: 'wikilink',
        level: 'inline',
        start: (src) => src.indexOf('[['),
        tokenizer(src) {
          const m = /^\[\[([a-z0-9][a-z0-9-]{0,79})(?:\|([^\]]*))?\]\]/.exec(src);
          if (m) return { type: 'wikilink', raw: m[0], slug: m[1], text: (m[2] || title(m[1]) || m[1]).trim() };
          return undefined;
        },
        renderer: (t) =>
          `<a class="wikilink${known(t.slug) ? '' : ' missing'}" href="#/page/${t.slug}" data-slug="${t.slug}">${escapeHtml(t.text)}</a>`,
      },
    ],
    renderer: {
      html: (t) => escapeHtml(t.text ?? t.raw ?? ''),
      link(t) {
        const text = this.parser.parseInline(t.tokens);
        const href = String(t.href || '').trim();
        if (!SAFE_HREF.test(href)) return text;
        const external = /^https?:/i.test(href);
        const title = t.title ? ` title="${escapeHtml(t.title)}"` : '';
        return `<a href="${escapeHtml(href)}"${title}${external ? ' target="_blank" rel="noopener noreferrer"' : ''}>${text}</a>`;
      },
      image(t) {
        // No remote images (CSP blocks them anyway): show the alt text as a link instead.
        const href = String(t.href || '');
        return SAFE_HREF.test(href) && /^https?:/i.test(href)
          ? `<a href="${escapeHtml(href)}" target="_blank" rel="noopener noreferrer">${escapeHtml(t.text || href)}</a>`
          : escapeHtml(t.text || '');
      },
    },
  });
  return (text) => md.parse(String(text ?? ''));
}

/** Wraps search terms in <mark> inside already-plain text; returns HTML. */
export function highlight(text, terms) {
  const safe = escapeHtml(text);
  const list = (terms || []).filter((t) => t && t.length > 1).map((t) => t.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'));
  if (!list.length) return safe;
  return safe.replace(new RegExp(`(${list.join('|')})`, 'gi'), '<mark>$1</mark>');
}

/** At most `max` chars of plain text around the first search term, with … where it was cut. */
export function snippetAround(text, terms, max = 180) {
  const s = plainText(text, Infinity);
  if (s.length <= max) return s;
  const lower = s.toLowerCase();
  const hits = (terms || []).map((t) => lower.indexOf(String(t).toLowerCase())).filter((i) => i >= 0);
  const first = hits.length ? Math.min(...hits) : 0;
  let start = Math.max(0, Math.min(first - Math.floor(max / 3), s.length - max));
  if (start > 0) {
    const space = s.indexOf(' ', start);
    if (space > 0 && space < first) start = space + 1;
  }
  const out = s.slice(start, start + max).trimEnd();
  return `${start > 0 ? '…' : ''}${out}${start + max < s.length ? '…' : ''}`;
}

/** Plain text for previews: drops Markdown syntax and turns [[slug|text]] into text. */
export function plainText(md, max = 240) {
  const s = String(md ?? '')
    .replace(/```[\s\S]*?```/g, ' ')
    .replace(/\[\[([^\]|]+)(?:\|([^\]]*))?\]\]/g, (_, a, b) => b || a)
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/^#{1,6}\s+/gm, '')
    .replace(/^>\s?/gm, '')
    .replace(/(^|[\s(])_([^_\n]+)_(?=[\s).,;:!?]|$)/g, '$1$2') // _emphasis_, but not snake_case names
    .replace(/[*`~]+/g, '')
    .replace(/\s+/g, ' ')
    .trim();
  return s.length > max ? `${s.slice(0, max - 1).trimEnd()}…` : s;
}
