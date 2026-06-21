// Mirrors the JSON DTOs served by buoy-server (crates/server/src/api.rs).

export interface Thought {
  id: string;
  text: string;
  /** Milliseconds since the epoch. */
  created_at: number;
  updated_at: number;
  is_settled: boolean;
}

export interface MatchRange {
  start: number;
  len: number;
}

export interface ThoughtMatch {
  thought: Thought;
  snippet: string;
  ranges: MatchRange[];
}

export interface Page {
  thoughts: Thought[];
  next_cursor: string | null;
}

export interface EditEntry {
  text: string;
  archived_at: number;
}

export interface SavedSearch {
  id: string;
  name: string;
  query: string;
  created_at: number;
}
