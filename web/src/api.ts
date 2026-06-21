// Typed client for the buoy-server JSON API. Same-origin in production; the Vite
// dev server proxies /api to the Rust backend on :8092.

import type { EditEntry, Page, SavedSearch, Thought, ThoughtMatch } from "./types";

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    headers: init?.body ? { "content-type": "application/json" } : undefined,
    ...init,
  });
  if (!res.ok) {
    let detail = `HTTP ${res.status}`;
    try {
      const body = (await res.json()) as { error?: string };
      if (body.error) detail = body.error;
    } catch {
      // non-JSON error body; keep the status
    }
    throw new Error(detail);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

export function listThoughts(before?: string | null, limit?: number): Promise<Page> {
  const params = new URLSearchParams();
  if (before) params.set("before", before);
  if (limit) params.set("limit", String(limit));
  const qs = params.toString();
  return request<Page>(`/api/thoughts${qs ? `?${qs}` : ""}`);
}

export function createThought(text: string): Promise<Thought> {
  return request<Thought>("/api/thoughts", {
    method: "POST",
    body: JSON.stringify({ text }),
  });
}

export function updateThought(id: string, text: string): Promise<Thought> {
  return request<Thought>(`/api/thoughts/${id}`, {
    method: "PUT",
    body: JSON.stringify({ text }),
  });
}

export function deleteThought(id: string): Promise<void> {
  return request<void>(`/api/thoughts/${id}`, { method: "DELETE" });
}

export function search(q: string, limit?: number): Promise<ThoughtMatch[]> {
  const params = new URLSearchParams({ q });
  if (limit) params.set("limit", String(limit));
  return request<ThoughtMatch[]>(`/api/search?${params.toString()}`);
}

export function relatedToDraft(
  draft: string,
  exclude?: string,
  topK?: number,
): Promise<ThoughtMatch[]> {
  return request<ThoughtMatch[]>("/api/related", {
    method: "POST",
    body: JSON.stringify({ draft, exclude, top_k: topK }),
  });
}

export function relatedToThought(id: string, topK?: number): Promise<ThoughtMatch[]> {
  const qs = topK ? `?top_k=${topK}` : "";
  return request<ThoughtMatch[]>(`/api/thoughts/${id}/related${qs}`);
}

export function history(id: string): Promise<EditEntry[]> {
  return request<EditEntry[]>(`/api/thoughts/${id}/history`);
}

export function tagsWithPrefix(prefix: string, limit?: number): Promise<string[]> {
  const params = new URLSearchParams();
  if (prefix) params.set("prefix", prefix);
  if (limit) params.set("limit", String(limit));
  const qs = params.toString();
  return request<string[]>(`/api/tags${qs ? `?${qs}` : ""}`);
}

export function thoughtsByTag(name: string, limit?: number): Promise<Thought[]> {
  const qs = limit ? `?limit=${limit}` : "";
  return request<Thought[]>(`/api/tags/${encodeURIComponent(name)}/thoughts${qs}`);
}

export function listSavedSearches(): Promise<SavedSearch[]> {
  return request<SavedSearch[]>("/api/saved-searches");
}

export function createSavedSearch(name: string, query: string): Promise<SavedSearch> {
  return request<SavedSearch>("/api/saved-searches", {
    method: "POST",
    body: JSON.stringify({ name, query }),
  });
}

export function deleteSavedSearch(id: string): Promise<void> {
  return request<void>(`/api/saved-searches/${id}`, { method: "DELETE" });
}
