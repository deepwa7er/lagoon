import type { Thought, ThoughtMatch } from "../types";
import { relTime } from "../lib/format";
import { Snippet } from "./Snippet";
import { TaggedText } from "./TaggedText";

/** Per-row related-thoughts state: `undefined` = closed, `null` = loading. */
export type RelatedState = ThoughtMatch[] | null | undefined;

export function ThoughtRow({
  thought,
  editing,
  related,
  onEdit,
  onDelete,
  onToggleRelated,
  onPick,
  onTagClick,
  onToggleActioned,
}: {
  thought: Thought;
  editing: boolean;
  related: RelatedState;
  onEdit: (t: Thought) => void;
  onDelete: (id: string) => void;
  onToggleRelated: (t: Thought) => void;
  onPick: (id: string) => void;
  onTagClick: (tag: string) => void;
  onToggleActioned: (t: Thought) => void;
}) {
  const open = related !== undefined;
  const actioned = thought.is_actioned;
  return (
    <li
      id={`t-${thought.id}`}
      className={`border-b border-rule px-4 py-2.5 ${editing ? "bg-surface-2" : ""} ${actioned ? "opacity-40" : ""}`}
    >
      <div className="flex items-start gap-3">
        {/* Live/settled: a flat amber square marks a still-mutable thought. */}
        <span
          className={`mt-1.5 size-2 shrink-0 ${thought.is_settled ? "bg-transparent" : "bg-accent"}`}
          title={thought.is_settled ? "settled" : "live — edits overwrite until it settles"}
        />
        {/* The note body is selectable text, not an edit trigger, so it can be
            read and copied freely; editing is an explicit action via the "edit"
            button. A div (not a button) keeps the inline tag chips from nesting
            inside a button. */}
        <div
          className={`flex-1 cursor-text whitespace-pre-wrap break-words text-left text-ink ${actioned ? "line-through decoration-ink-faint" : ""}`}
        >
          <TaggedText text={thought.text} onTagClick={onTagClick} />
        </div>
        <div className="flex shrink-0 items-center gap-3 pt-px text-[11px] uppercase tracking-wide text-ink-faint">
          <time
            dateTime={new Date(thought.created_at).toISOString()}
            title={new Date(thought.created_at).toLocaleString()}
            className="tabular-nums normal-case"
          >
            {relTime(thought.created_at)}
          </time>
          <button
            type="button"
            onClick={() => onToggleRelated(thought)}
            className={`hover:text-accent ${open ? "text-accent" : ""}`}
          >
            rel
          </button>
          <button
            type="button"
            onClick={() => onToggleActioned(thought)}
            className={`hover:text-accent ${actioned ? "text-accent" : ""}`}
            title={actioned ? "mark as not done" : "mark as done"}
          >
            done
          </button>
          <button type="button" onClick={() => onEdit(thought)} className="hover:text-accent">
            edit
          </button>
          <button type="button" onClick={() => onDelete(thought.id)} className="hover:text-failed">
            del
          </button>
        </div>
      </div>

      {open && (
        <ul className="mt-2 ml-5 border-l border-rule-strong pl-3">
          {related === null && <li className="py-1 text-ink-faint">…</li>}
          {related !== null && related.length === 0 && (
            <li className="py-1 text-ink-faint uppercase tracking-wide text-[11px]">
              nothing related
            </li>
          )}
          {related?.map((m) => (
            <li key={m.thought.id} className="py-1">
              <button
                type="button"
                onClick={() => onPick(m.thought.id)}
                className="text-left text-ink-muted hover:text-ink"
                title={m.thought.text}
              >
                <Snippet snippet={m.snippet} ranges={m.ranges} />
              </button>
            </li>
          ))}
        </ul>
      )}
    </li>
  );
}
