import { segmentTags } from "../lib/tags";

/**
 * Render thought text with its `#tag` tokens as clickable chips. Tag clicks
 * stop propagation so they don't also trigger the row's edit affordance.
 */
export function TaggedText({
  text,
  onTagClick,
}: {
  text: string;
  onTagClick: (tag: string) => void;
}) {
  return (
    <>
      {segmentTags(text).map((seg, i) =>
        seg.tag !== undefined ? (
          <button
            key={i}
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              onTagClick(seg.tag as string);
            }}
            className="text-accent hover:underline"
          >
            {seg.text}
          </button>
        ) : (
          <span key={i}>{seg.text}</span>
        ),
      )}
    </>
  );
}
