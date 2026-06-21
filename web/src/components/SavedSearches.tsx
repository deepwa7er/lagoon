import type { SavedSearch } from "../types";

/** A thin bar of pinned queries: click a chip to run it, × to unpin. */
export function SavedSearches({
  items,
  onRun,
  onDelete,
}: {
  items: SavedSearch[];
  onRun: (s: SavedSearch) => void;
  onDelete: (id: string) => void;
}) {
  if (items.length === 0) return null;
  return (
    <div className="flex flex-wrap items-center gap-1.5 border-b border-rule px-4 py-1.5">
      <span className="text-[10px] uppercase tracking-wider text-ink-faint">pinned</span>
      {items.map((s) => (
        <span key={s.id} className="flex items-center border border-rule bg-surface text-[11px]">
          <button
            type="button"
            onClick={() => onRun(s)}
            className="px-2 py-0.5 text-ink-muted hover:text-accent"
            title={s.query}
          >
            {s.name}
          </button>
          <button
            type="button"
            onClick={() => onDelete(s.id)}
            className="px-1 text-ink-faint hover:text-failed"
            title="unpin"
            aria-label={`unpin ${s.name}`}
          >
            ×
          </button>
        </span>
      ))}
    </div>
  );
}
