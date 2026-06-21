import { useCallback, useEffect, useRef, useState } from "react";

import * as api from "./api";
import type { SavedSearch, Thought, ThoughtMatch } from "./types";
import { activeTagToken } from "./lib/tags";
import { SavedSearches } from "./components/SavedSearches";
import { Snippet } from "./components/Snippet";
import { SuggestionStrip } from "./components/SuggestionStrip";
import { ThoughtRow } from "./components/ThoughtRow";

/** A bare `#tag` query (no spaces) routes to the tag filter instead of search. */
const TAG_QUERY = /^#([\p{L}\p{N}_][\p{L}\p{N}_-]*)$/u;

export function App() {
  const [thoughts, setThoughts] = useState<Thought[]>([]);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);

  const [draft, setDraft] = useState("");
  const [editingId, setEditingId] = useState<string | null>(null);
  const [suggestions, setSuggestions] = useState<ThoughtMatch[]>([]);

  const [query, setQuery] = useState("");
  const [results, setResults] = useState<ThoughtMatch[] | null>(null);

  const [related, setRelated] = useState<Record<string, ThoughtMatch[] | null>>({});
  const [error, setError] = useState<string | null>(null);

  const [savedSearches, setSavedSearches] = useState<SavedSearch[]>([]);

  const [hideActioned, setHideActioned] = useState<boolean>(() => {
    try {
      return localStorage.getItem("buoy.hideActioned") === "true";
    } catch {
      return false;
    }
  });

  // Composer #tag autocomplete: the token under the caret, its suggestions, and
  // the highlighted option.
  const [tagToken, setTagToken] = useState<{ prefix: string; start: number } | null>(null);
  const [tagOptions, setTagOptions] = useState<string[]>([]);
  const [tagActive, setTagActive] = useState(0);

  const textareaRef = useRef<HTMLTextAreaElement>(null);

  // Initial stream load.
  const reload = useCallback(async () => {
    try {
      const page = await api.listThoughts();
      setThoughts(page.thoughts);
      setNextCursor(page.next_cursor);
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e));
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  // Pinned searches (non-critical; a failure just hides the bar).
  const reloadSaved = useCallback(async () => {
    try {
      setSavedSearches(await api.listSavedSearches());
    } catch {
      // ignore
    }
  }, []);

  useEffect(() => {
    void reloadSaved();
  }, [reloadSaved]);

  // Debounced search — swaps the stream for results while the box is non-empty.
  // A bare `#tag` query filters by tag; anything else runs combined search.
  useEffect(() => {
    const q = query.trim();
    if (!q) {
      setResults(null);
      return;
    }
    const tag = q.match(TAG_QUERY);
    const h = setTimeout(async () => {
      try {
        if (tag) {
          const name = tag[1].replace(/-+$/u, "");
          const ths = await api.thoughtsByTag(name);
          setResults(ths.map((t) => ({ thought: t, snippet: t.text, ranges: [] })));
        } else {
          setResults(await api.search(q));
        }
      } catch (e) {
        setError(String(e instanceof Error ? e.message : e));
      }
    }, 150);
    return () => clearTimeout(h);
  }, [query]);

  // Debounced composition-time suggestions for the current draft.
  useEffect(() => {
    const d = draft.trim();
    if (!d) {
      setSuggestions([]);
      return;
    }
    const h = setTimeout(async () => {
      try {
        setSuggestions(await api.relatedToDraft(draft, editingId ?? undefined));
      } catch {
        // suggestions are an enhancement; never surface their failures
      }
    }, 200);
    return () => clearTimeout(h);
  }, [draft, editingId]);

  // Tag suggestions for the token under the caret (debounced, lightweight).
  useEffect(() => {
    if (!tagToken) {
      setTagOptions([]);
      return;
    }
    const h = setTimeout(async () => {
      try {
        setTagOptions(await api.tagsWithPrefix(tagToken.prefix, 8));
        setTagActive(0);
      } catch {
        setTagOptions([]);
      }
    }, 80);
    return () => clearTimeout(h);
  }, [tagToken]);

  const refreshTagToken = useCallback(() => {
    const el = textareaRef.current;
    if (!el) return;
    setTagToken(activeTagToken(el.value, el.selectionStart ?? el.value.length));
  }, []);

  const completeTag = useCallback(
    (name: string) => {
      const el = textareaRef.current;
      if (!el || !tagToken) return;
      const caret = el.selectionStart ?? el.value.length;
      const before = draft.slice(0, tagToken.start);
      const insert = `#${name} `;
      const next = before + insert + draft.slice(caret);
      setDraft(next);
      setTagToken(null);
      setTagOptions([]);
      const pos = before.length + insert.length;
      requestAnimationFrame(() => {
        el.focus();
        el.setSelectionRange(pos, pos);
      });
    },
    [draft, tagToken],
  );

  const save = useCallback(async () => {
    const text = draft.trim();
    if (!text) return;
    try {
      if (editingId) {
        const updated = await api.updateThought(editingId, draft);
        setThoughts((ts) => ts.map((t) => (t.id === updated.id ? updated : t)));
      } else {
        const created = await api.createThought(draft);
        setThoughts((ts) => [created, ...ts]);
      }
      setDraft("");
      setEditingId(null);
      setSuggestions([]);
      setTagToken(null);
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e));
    }
  }, [draft, editingId]);

  const startEdit = useCallback((t: Thought) => {
    setEditingId(t.id);
    setDraft(t.text);
    textareaRef.current?.focus();
  }, []);

  const cancelEdit = useCallback(() => {
    setEditingId(null);
    setDraft("");
  }, []);

  const remove = useCallback(
    async (id: string) => {
      try {
        await api.deleteThought(id);
        setThoughts((ts) => ts.filter((t) => t.id !== id));
        setResults((rs) => (rs ? rs.filter((m) => m.thought.id !== id) : rs));
        if (editingId === id) cancelEdit();
      } catch (e) {
        setError(String(e instanceof Error ? e.message : e));
      }
    },
    [editingId, cancelEdit],
  );

  const loadMore = useCallback(async () => {
    if (!nextCursor || loadingMore) return;
    setLoadingMore(true);
    try {
      const page = await api.listThoughts(nextCursor);
      setThoughts((ts) => [...ts, ...page.thoughts]);
      setNextCursor(page.next_cursor);
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e));
    } finally {
      setLoadingMore(false);
    }
  }, [nextCursor, loadingMore]);

  const onScroll = useCallback(
    (e: React.UIEvent<HTMLDivElement>) => {
      const el = e.currentTarget;
      if (el.scrollHeight - el.scrollTop - el.clientHeight < 320) void loadMore();
    },
    [loadMore],
  );

  const toggleRelated = useCallback(
    async (t: Thought) => {
      if (t.id in related) {
        setRelated(({ [t.id]: _omit, ...rest }) => rest);
        return;
      }
      setRelated((p) => ({ ...p, [t.id]: null }));
      try {
        const items = await api.relatedToThought(t.id);
        setRelated((p) => ({ ...p, [t.id]: items }));
      } catch {
        setRelated((p) => ({ ...p, [t.id]: [] }));
      }
    },
    [related],
  );

  // Scroll a thought into view if it's currently loaded in the stream.
  const reveal = useCallback((id: string) => {
    document.getElementById(`t-${id}`)?.scrollIntoView({ behavior: "smooth", block: "center" });
  }, []);

  // Clicking a #tag (in the stream) filters by it.
  const onTagClick = useCallback((tag: string) => {
    setQuery(`#${tag}`);
  }, []);

  const runSaved = useCallback((s: SavedSearch) => setQuery(s.query), []);

  const deleteSaved = useCallback(
    async (id: string) => {
      try {
        await api.deleteSavedSearch(id);
        await reloadSaved();
      } catch (e) {
        setError(String(e instanceof Error ? e.message : e));
      }
    },
    [reloadSaved],
  );

  const toggleActioned = useCallback(
    async (t: Thought) => {
      try {
        const updated = t.is_actioned
          ? await api.unmarkActioned(t.id)
          : await api.markActioned(t.id);
        setThoughts((ts) => ts.map((th) => (th.id === updated.id ? updated : th)));
      } catch (e) {
        setError(String(e instanceof Error ? e.message : e));
      }
    },
    [],
  );

  const toggleHideActioned = useCallback(() => {
    setHideActioned((prev) => {
      const next = !prev;
      try {
        localStorage.setItem("buoy.hideActioned", String(next));
      } catch {
        // ignore
      }
      return next;
    });
  }, []);

  const pinSearch = useCallback(async () => {
    const q = query.trim();
    if (!q) return;
    const name = window.prompt("Name this pinned search:", q)?.trim();
    if (!name) return;
    try {
      await api.createSavedSearch(name, q);
      await reloadSaved();
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e));
    }
  }, [query, reloadSaved]);

  const acOpen = tagToken !== null && tagOptions.length > 0;

  const onComposerKey = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (acOpen) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setTagActive((i) => (i + 1) % tagOptions.length);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        setTagActive((i) => (i - 1 + tagOptions.length) % tagOptions.length);
        return;
      }
      if ((e.key === "Enter" || e.key === "Tab") && !e.nativeEvent.isComposing) {
        e.preventDefault();
        completeTag(tagOptions[tagActive]);
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        setTagToken(null);
        return;
      }
    }
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      void save();
    } else if (e.key === "Escape" && editingId) {
      e.preventDefault();
      cancelEdit();
    }
  };

  return (
    <div className="mx-auto flex h-full max-w-2xl flex-col">
      {/* Documentation furniture — on-brand for the instrument school. */}
      <div className="flex gap-6 border-b border-rule px-4 py-1.5 text-[10px] uppercase tracking-wider text-ink-faint">
        <span>DOC. BUOY-001</span>
        <span>REV. 0.1.0</span>
        <span className="ml-auto">CLASSIFICATION · PERSONAL</span>
      </div>

      <header className="flex items-baseline gap-3 border-b border-rule-strong px-4 py-3">
        <h1 className="text-base font-bold uppercase tracking-[0.2em] text-ink">buoy</h1>
        <span className="text-[11px] uppercase tracking-wide text-ink-muted">
          capture-first notes
        </span>
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="search… or #tag"
          aria-label="search"
          className="ml-auto w-44 border border-rule bg-surface px-2 py-1 text-ink placeholder:text-ink-faint focus:border-accent focus:outline-none"
        />
        <button
          type="button"
          onClick={() => void pinSearch()}
          disabled={!query.trim()}
          title="pin this search"
          className="border border-rule px-2 py-1 text-[11px] uppercase tracking-wide text-ink-muted hover:border-accent hover:text-accent disabled:cursor-not-allowed disabled:opacity-30"
        >
          pin
        </button>
      </header>

      <SavedSearches
        items={savedSearches}
        onRun={runSaved}
        onDelete={(id) => void deleteSaved(id)}
      />

      <div className="border-b border-rule px-4 py-3">
        <div className="mb-1.5 flex items-center gap-2 text-[11px] uppercase tracking-wide text-ink-muted">
          <span>{editingId ? "editing thought" : "new thought"}</span>
          {editingId && (
            <button
              type="button"
              onClick={cancelEdit}
              className="border border-rule px-1.5 text-ink-faint hover:border-accent hover:text-accent"
            >
              cancel
            </button>
          )}
        </div>
        <div className="relative">
          <textarea
            ref={textareaRef}
            value={draft}
            onChange={(e) => {
              setDraft(e.target.value);
              refreshTagToken();
            }}
            onSelect={refreshTagToken}
            onKeyDown={onComposerKey}
            onBlur={() => window.setTimeout(() => setTagToken(null), 120)}
            rows={2}
            placeholder="what's on your mind? use #tags to organize"
            className="w-full resize-none border border-rule bg-surface px-3 py-2 text-ink placeholder:text-ink-faint focus:border-accent focus:outline-none"
          />
          {acOpen && (
            <ul className="absolute inset-x-0 top-full z-10 max-h-44 overflow-y-auto border border-rule-strong bg-surface text-[13px] shadow-lg">
              {tagOptions.map((opt, i) => (
                <li key={opt}>
                  <button
                    type="button"
                    // mousedown (not click) + preventDefault keeps the textarea
                    // focused so the completion lands before any blur.
                    onMouseDown={(e) => {
                      e.preventDefault();
                      completeTag(opt);
                    }}
                    className={`block w-full px-3 py-1 text-left ${
                      i === tagActive ? "bg-surface-2 text-accent" : "text-ink-muted"
                    }`}
                  >
                    #{opt}
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
        <div className="mt-2 flex items-center gap-3">
          <button
            type="button"
            onClick={() => void save()}
            disabled={!draft.trim()}
            className="border border-accent bg-accent px-3 py-1 text-[11px] font-bold uppercase tracking-wider text-bg disabled:cursor-not-allowed disabled:opacity-30 enabled:hover:bg-transparent enabled:hover:text-accent"
          >
            {editingId ? "Update" : "Capture"}
          </button>
          <span className="text-[11px] uppercase tracking-wide text-ink-faint">
            Enter to save · Shift+Enter newline
          </span>
        </div>
      </div>

      {!results && (
        <SuggestionStrip
          suggestions={suggestions}
          onPick={reveal}
          onDismiss={() => setSuggestions([])}
        />
      )}

      <div className="flex-1 overflow-y-auto" onScroll={onScroll}>
        {results ? (
          <ul>
            {results.length === 0 && (
              <li className="px-4 py-6 text-[11px] uppercase tracking-wide text-ink-faint">
                no matches
              </li>
            )}
            {results.map((m) => (
              <li key={m.thought.id} className="border-b border-rule px-4 py-2.5">
                <button
                  type="button"
                  onClick={() => startEdit(m.thought)}
                  className="w-full whitespace-pre-wrap break-words text-left text-ink"
                >
                  <Snippet snippet={m.snippet} ranges={m.ranges} />
                </button>
              </li>
            ))}
          </ul>
        ) : (
          <ul>
            {thoughts.length === 0 && (
              <li className="px-4 py-10 text-[11px] uppercase tracking-wide text-ink-faint">
                nothing captured yet — write your first thought above
              </li>
            )}
            {thoughts
              .filter((t) => !hideActioned || !t.is_actioned)
              .map((t) => (
                <ThoughtRow
                  key={t.id}
                  thought={t}
                  editing={editingId === t.id}
                  related={related[t.id]}
                  onEdit={startEdit}
                  onDelete={(id) => void remove(id)}
                  onToggleRelated={(th) => void toggleRelated(th)}
                  onPick={reveal}
                  onTagClick={onTagClick}
                  onToggleActioned={(th) => void toggleActioned(th)}
                />
              ))}
            {loadingMore && <li className="px-4 py-3 text-center text-ink-faint">…</li>}
          </ul>
        )}
      </div>

      <footer className="flex items-center justify-between border-t border-rule-strong px-4 py-2 text-[11px] uppercase tracking-wide text-ink-faint">
        {error ? (
          <button type="button" className="text-accent" onClick={() => setError(null)} title="dismiss">
            ERR · {error}
          </button>
        ) : (
          <span>
            {results
              ? `${results.length} match${results.length === 1 ? "" : "es"}`
              : `${thoughts.length} loaded`}
          </span>
        )}
        <div className="flex items-center gap-4">
          <button
            type="button"
            onClick={toggleHideActioned}
            className={`hover:text-accent ${hideActioned ? "text-accent" : ""}`}
            title={hideActioned ? "show actioned thoughts" : "hide actioned thoughts"}
          >
            {hideActioned ? "show done" : "hide done"}
          </button>
          <span>buoy v0.1.0</span>
        </div>
      </footer>
    </div>
  );
}
