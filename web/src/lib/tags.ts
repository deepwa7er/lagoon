// Client-side `#tag` helpers — for rendering tags inline and driving composer
// autocomplete. The canonical parser lives in the Rust core (it owns the mirror
// tables); this mirrors the same shape (`#` on a word boundary, then a body of
// letters/digits/`_`/`-` starting with letter/digit/`_`) for display only.

const TAG_BODY = "[\\p{L}\\p{N}_-]";
// Capture the boundary char (group 1, kept as plain text) instead of using a
// lookbehind, for broader browser support.
const TAG_RE = new RegExp(`(^|[^\\p{L}\\p{N}_#])(#[\\p{L}\\p{N}_]${TAG_BODY}*)`, "gu");

export interface TextSegment {
  text: string;
  /** Present when this segment is a `#tag`; the bare tag name (no `#`). */
  tag?: string;
}

/** Split `text` into plain and `#tag` segments for inline rendering. */
export function segmentTags(text: string): TextSegment[] {
  const segments: TextSegment[] = [];
  let last = 0;
  for (const m of text.matchAll(TAG_RE)) {
    const boundary = m[1];
    const token = m[2];
    const tokenStart = (m.index ?? 0) + boundary.length;
    if (tokenStart > last) segments.push({ text: text.slice(last, tokenStart) });
    segments.push({ text: token, tag: token.slice(1).replace(/-+$/u, "") });
    last = tokenStart + token.length;
  }
  if (last < text.length) segments.push({ text: text.slice(last) });
  return segments;
}

const TAG_CHAR = /[\p{L}\p{N}_-]/u;
const WORD_CHAR = /[\p{L}\p{N}_#]/u;

/**
 * The `#tag` token the caret is currently inside (for autocomplete), or null.
 * `prefix` is the text typed after the `#` so far (may be empty right after
 * `#`); `start` is the index of the `#`.
 */
export function activeTagToken(
  text: string,
  caret: number,
): { prefix: string; start: number } | null {
  let i = caret;
  while (i > 0 && TAG_CHAR.test(text[i - 1])) i--;
  if (i === 0 || text[i - 1] !== "#") return null;
  const hash = i - 1;
  // The `#` must sit on a word boundary (start, or after a non-word, non-`#`).
  if (hash > 0 && WORD_CHAR.test(text[hash - 1])) return null;
  return { prefix: text.slice(i, caret), start: hash };
}
