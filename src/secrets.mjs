// The secret guard: what the wiki refuses to store and what the logs redact.
// No dependencies, so the request log can use it unbundled.

const SECRET_PATTERNS = [
  ['a private key block', /-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----/],
  ['an API key (sk-...)', /(?<![A-Za-z0-9])sk-(?:ant-)?[A-Za-z0-9_-]{20,}/],
  ['a GitHub token', /(?<![A-Za-z0-9])(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{20,})/],
  ['a Slack token', /(?<![A-Za-z0-9])xox[abposr]-[A-Za-z0-9-]{10,}/],
  ['an AWS access key ID', /(?<![A-Za-z0-9])(?:AKIA|ASIA)[0-9A-Z]{16}(?![A-Za-z0-9])/],
  ['a Google API key', /(?<![A-Za-z0-9])AIza[0-9A-Za-z_-]{35}/],
  ['a JSON Web Token', /(?<![A-Za-z0-9])eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}/],
  ['an npm token', /(?<![A-Za-z0-9])npm_[A-Za-z0-9]{36}(?![A-Za-z0-9])/],
  ['a US Social Security number', /(?<![\d-])(?!000|666|9\d\d)\d{3}-(?!00)\d{2}-(?!0000)\d{4}(?![\d-])/],
];

// "password: X", "api key = X", "token: X": only when X is 12+ chars with a
// letter AND a digit, so prose like "the token refreshes every 8 hours" passes.
const ASSIGNMENT_RE =
  /\b(?:pass(?:word|wd|phrase)?|pwd|secret|client[ _-]?secret|api[ _-]?key|apikey|access[ _-]?key|(?:auth|access|bearer|refresh)[ _-]?token|token)\b["']?\s*[:=]\s*["']?([^\s"'`,;)]+)/gi;

const CARD_CANDIDATES = [
  /(?<![\d.])\d{15,16}(?![\d.]?\d)/g,
  /(?<![\d-])\d{4}([ -])\d{4}\1\d{4}\1\d{3,4}(?![\d-])/g,
  /(?<![\d-])\d{4}([ -])\d{6}\1\d{5}(?![\d-])/g,
];

function luhn(digits) {
  let sum = 0;
  let alt = false;
  for (let i = digits.length - 1; i >= 0; i--) {
    let n = digits.charCodeAt(i) - 48;
    if (alt) {
      n *= 2;
      if (n > 9) n -= 9;
    }
    sum += n;
    alt = !alt;
  }
  return sum % 10 === 0;
}

/** Returns a description of the first secret-looking thing in text, or null. */
export function findSecret(text) {
  const s = String(text ?? '');
  if (!s) return null;
  for (const [kind, re] of SECRET_PATTERNS) if (re.test(s)) return kind;
  for (const m of s.matchAll(ASSIGNMENT_RE)) {
    const v = m[1];
    if (v.length >= 12 && /[A-Za-z]/.test(v) && /\d/.test(v)) return 'a credential assignment (e.g. "password: ...")';
  }
  for (const re of CARD_CANDIDATES) {
    for (const m of s.matchAll(re)) {
      const d = m[0].replace(/\D/g, '');
      if (isCard(d)) return 'a payment card number';
    }
  }
  return null;
}

const PRIVATE_KEY_BLOCK = /-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----|$)/g;
const isCard = (m) => {
  const d = m.replace(/\D/g, '');
  return (d.length === 15 || d.length === 16) && d[0] >= '3' && d[0] <= '6' && luhn(d);
};

/** Replaces everything findSecret would flag with [REDACTED]. Used before anything is written to a log. */
export function redactSecrets(text) {
  let s = String(text ?? '');
  if (!s) return s;
  s = s.replace(PRIVATE_KEY_BLOCK, '[REDACTED]');
  for (const [, re] of SECRET_PATTERNS) s = s.replace(new RegExp(re.source, `${re.flags}g`), '[REDACTED]');
  s = s.replace(ASSIGNMENT_RE, (m, v) => (v.length >= 12 && /[A-Za-z]/.test(v) && /\d/.test(v) ? m.replace(v, '[REDACTED]') : m));
  for (const re of CARD_CANDIDATES) s = s.replace(re, (m) => (isCard(m) ? '[REDACTED]' : m));
  return s;
}
