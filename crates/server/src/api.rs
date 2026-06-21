//! HTTP API: JSON DTOs over the buoy core's `ThoughtStore`, mirroring the
//! operations the native clients get through the `UniFFI` layer.
//!
//! The core's `ThoughtStore` is synchronous and `!Sync` (it owns a rusqlite
//! `Connection`), so it lives behind an `Arc<Mutex<…>>` and every handler runs
//! its store call on a blocking thread (`spawn_blocking`) to keep the async
//! runtime free. DTOs are defined here so the core stays UI-agnostic.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use buoy_core::{
    Cursor, EditEntry, MatchRange, Page, SavedSearch, SyncCursor, Thought, ThoughtChange,
    ThoughtMatch, ThoughtStore,
};

/// Shared handle to the canonical store.
pub type Shared = Arc<Mutex<ThoughtStore>>;

/// Default number of search results / suggestions when the client doesn't ask.
const DEFAULT_SEARCH_LIMIT: usize = 50;
const DEFAULT_DRAFT_SUGGESTIONS: usize = 3;
const DEFAULT_RELATED: usize = 5;
/// Default number of tag suggestions for `#tag` autocomplete.
const DEFAULT_TAGS_LIMIT: usize = 12;
/// Max changes returned in one `/api/sync` pull. A personal store fits in one
/// round-trip; if a client ever gets exactly this many it simply syncs again.
const SYNC_PAGE_LIMIT: usize = 1000;

// ── DTOs ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ThoughtDto {
    pub id: String,
    pub text: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub is_settled: bool,
}

impl From<&Thought> for ThoughtDto {
    fn from(t: &Thought) -> Self {
        Self {
            id: t.id.to_string(),
            text: t.text.clone(),
            created_at: t.created_at,
            updated_at: t.updated_at,
            is_settled: t.is_settled,
        }
    }
}

#[derive(Serialize)]
pub struct MatchRangeDto {
    pub start: usize,
    pub len: usize,
}

impl From<&MatchRange> for MatchRangeDto {
    fn from(r: &MatchRange) -> Self {
        Self {
            start: r.start,
            len: r.len,
        }
    }
}

#[derive(Serialize)]
pub struct ThoughtMatchDto {
    pub thought: ThoughtDto,
    pub snippet: String,
    pub ranges: Vec<MatchRangeDto>,
}

impl From<&ThoughtMatch> for ThoughtMatchDto {
    fn from(m: &ThoughtMatch) -> Self {
        Self {
            thought: ThoughtDto::from(&m.thought),
            snippet: m.snippet.clone(),
            ranges: m.ranges.iter().map(MatchRangeDto::from).collect(),
        }
    }
}

#[derive(Serialize)]
pub struct PageDto {
    pub thoughts: Vec<ThoughtDto>,
    /// Opaque cursor for the next page, or null when the stream is exhausted.
    pub next_cursor: Option<String>,
}

impl From<Page> for PageDto {
    fn from(p: Page) -> Self {
        Self {
            thoughts: p.thoughts.iter().map(ThoughtDto::from).collect(),
            next_cursor: p.next_cursor.map(encode_cursor),
        }
    }
}

#[derive(Serialize)]
pub struct EditEntryDto {
    pub text: String,
    pub archived_at: i64,
}

impl From<&EditEntry> for EditEntryDto {
    fn from(e: &EditEntry) -> Self {
        Self {
            text: e.text.clone(),
            archived_at: e.archived_at,
        }
    }
}

fn matches_dto(matches: &[ThoughtMatch]) -> Vec<ThoughtMatchDto> {
    matches.iter().map(ThoughtMatchDto::from).collect()
}

#[derive(Serialize)]
pub struct SavedSearchDto {
    pub id: String,
    pub name: String,
    pub query: String,
    pub created_at: i64,
}

impl From<&SavedSearch> for SavedSearchDto {
    fn from(s: &SavedSearch) -> Self {
        Self {
            id: s.id.to_string(),
            name: s.name.clone(),
            query: s.query.clone(),
            created_at: s.created_at,
        }
    }
}

/// A full thought row for sync, including tombstones (`deleted_at` set).
#[derive(Serialize, Deserialize)]
pub struct ThoughtChangeDto {
    pub id: String,
    pub text: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub settled_at: Option<i64>,
    pub deleted_at: Option<i64>,
}

impl From<&ThoughtChange> for ThoughtChangeDto {
    fn from(c: &ThoughtChange) -> Self {
        Self {
            id: c.id.to_string(),
            text: c.text.clone(),
            created_at: c.created_at,
            updated_at: c.updated_at,
            settled_at: c.settled_at,
            deleted_at: c.deleted_at,
        }
    }
}

impl TryFrom<&ThoughtChangeDto> for ThoughtChange {
    type Error = AppError;
    fn try_from(d: &ThoughtChangeDto) -> Result<Self, AppError> {
        Ok(Self {
            id: parse_id(&d.id)?,
            text: d.text.clone(),
            created_at: d.created_at,
            updated_at: d.updated_at,
            settled_at: d.settled_at,
            deleted_at: d.deleted_at,
        })
    }
}

// ── cursor codec ─────────────────────────────────────────────────────────────

/// Encode a keyset cursor as `"<created_at>_<uuid>"`. The uuid's hyphens never
/// collide with the single `_` separator, so decoding is a simple split.
fn encode_cursor(c: Cursor) -> String {
    format!("{}_{}", c.created_at, c.id)
}

fn decode_cursor(s: &str) -> Result<Cursor, AppError> {
    let (created, id) = s
        .split_once('_')
        .ok_or_else(|| AppError::bad_request("malformed cursor"))?;
    let created_at = created
        .parse::<i64>()
        .map_err(|_| AppError::bad_request("malformed cursor timestamp"))?;
    let id = id
        .parse::<Uuid>()
        .map_err(|_| AppError::bad_request("malformed cursor id"))?;
    Ok(Cursor { created_at, id })
}

fn parse_id(raw: &str) -> Result<Uuid, AppError> {
    raw.parse::<Uuid>()
        .map_err(|_| AppError::bad_request("invalid thought id"))
}

/// Encode a sync cursor as `"<updated_at>_<uuid>"` (same shape as the list
/// cursor, but over the `(updated_at, id)` change-feed order).
fn encode_sync_cursor(c: SyncCursor) -> String {
    format!("{}_{}", c.updated_at, c.id)
}

fn decode_sync_cursor(s: &str) -> Result<SyncCursor, AppError> {
    let (updated, id) = s
        .split_once('_')
        .ok_or_else(|| AppError::bad_request("malformed sync cursor"))?;
    Ok(SyncCursor {
        updated_at: updated
            .parse::<i64>()
            .map_err(|_| AppError::bad_request("malformed sync cursor timestamp"))?,
        id: id
            .parse::<Uuid>()
            .map_err(|_| AppError::bad_request("malformed sync cursor id"))?,
    })
}

// ── handlers ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ListQuery {
    pub before: Option<String>,
    pub limit: Option<usize>,
}

/// `GET /api/thoughts?before=<cursor>&limit=` — newest-first keyset page.
pub async fn list_thoughts(
    State(store): State<Shared>,
    Query(q): Query<ListQuery>,
) -> Result<Json<PageDto>, AppError> {
    let before = q.before.as_deref().map(decode_cursor).transpose()?;
    let limit = q.limit.unwrap_or(buoy_core::DEFAULT_PAGE_SIZE);
    let page = blocking(store, move |s| s.list_paginated(before, limit)).await?;
    Ok(Json(PageDto::from(page)))
}

#[derive(Deserialize)]
pub struct TextBody {
    pub text: String,
}

/// `POST /api/thoughts` — capture a new thought.
pub async fn create_thought(
    State(store): State<Shared>,
    Json(body): Json<TextBody>,
) -> Result<(StatusCode, Json<ThoughtDto>), AppError> {
    let thought = blocking(store, move |s| s.create(&body.text)).await?;
    Ok((StatusCode::CREATED, Json(ThoughtDto::from(&thought))))
}

/// `PUT /api/thoughts/{id}` — replace a thought's text.
pub async fn update_thought(
    State(store): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<TextBody>,
) -> Result<Json<ThoughtDto>, AppError> {
    let id = parse_id(&id)?;
    let thought = blocking(store, move |s| s.update_thought(id, &body.text)).await?;
    Ok(Json(ThoughtDto::from(&thought)))
}

/// `DELETE /api/thoughts/{id}` — delete a thought and its edit history.
pub async fn delete_thought(
    State(store): State<Shared>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let id = parse_id(&id)?;
    blocking(store, move |s| s.delete_thought(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub limit: Option<usize>,
}

/// `GET /api/search?q=&limit=` — combined keyword + semantic search.
pub async fn search(
    State(store): State<Shared>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Vec<ThoughtMatchDto>>, AppError> {
    let limit = q.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    let query = q.q;
    let matches = blocking(store, move |s| s.search_combined(&query, limit)).await?;
    Ok(Json(matches_dto(&matches)))
}

#[derive(Deserialize)]
pub struct RelatedDraft {
    pub draft: String,
    pub exclude: Option<String>,
    pub top_k: Option<usize>,
}

/// `POST /api/related` — related thoughts for an in-progress draft (the
/// composition-time suggestion strip). Empty for a blank draft or no embedder.
pub async fn related_to_draft(
    State(store): State<Shared>,
    Json(body): Json<RelatedDraft>,
) -> Result<Json<Vec<ThoughtMatchDto>>, AppError> {
    let exclude = body.exclude.as_deref().map(parse_id).transpose()?;
    let top_k = body.top_k.unwrap_or(DEFAULT_DRAFT_SUGGESTIONS);
    let draft = body.draft;
    let matches = blocking(store, move |s| s.find_related(&draft, top_k, exclude)).await?;
    Ok(Json(matches_dto(&matches)))
}

#[derive(Deserialize)]
pub struct TopKQuery {
    pub top_k: Option<usize>,
}

/// `GET /api/thoughts/{id}/related?top_k=` — related to an existing thought.
pub async fn related_to_thought(
    State(store): State<Shared>,
    Path(id): Path<String>,
    Query(q): Query<TopKQuery>,
) -> Result<Json<Vec<ThoughtMatchDto>>, AppError> {
    let id = parse_id(&id)?;
    let top_k = q.top_k.unwrap_or(DEFAULT_RELATED);
    let matches = blocking(store, move |s| s.find_related_to(id, top_k)).await?;
    Ok(Json(matches_dto(&matches)))
}

/// `GET /api/thoughts/{id}/history` — prior versions, oldest first.
pub async fn history(
    State(store): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<Vec<EditEntryDto>>, AppError> {
    let id = parse_id(&id)?;
    let entries = blocking(store, move |s| s.edit_history(id)).await?;
    Ok(Json(entries.iter().map(EditEntryDto::from).collect()))
}

// ── tags & saved searches ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct TagsQuery {
    pub prefix: Option<String>,
    pub limit: Option<usize>,
}

/// `GET /api/tags?prefix=&limit=` — tag names for `#tag` autocomplete,
/// most-used first.
pub async fn list_tags(
    State(store): State<Shared>,
    Query(q): Query<TagsQuery>,
) -> Result<Json<Vec<String>>, AppError> {
    let prefix = q.prefix.unwrap_or_default();
    let limit = q.limit.unwrap_or(DEFAULT_TAGS_LIMIT);
    let tags = blocking(store, move |s| s.tags_with_prefix(&prefix, limit)).await?;
    Ok(Json(tags))
}

#[derive(Deserialize)]
pub struct LimitQuery {
    pub limit: Option<usize>,
}

/// `GET /api/tags/{name}/thoughts?limit=` — live thoughts carrying the tag,
/// newest first (the "tap a tag to filter" path).
pub async fn thoughts_by_tag(
    State(store): State<Shared>,
    Path(name): Path<String>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Vec<ThoughtDto>>, AppError> {
    let limit = q.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    let thoughts = blocking(store, move |s| s.thoughts_with_tag(&name, limit)).await?;
    Ok(Json(thoughts.iter().map(ThoughtDto::from).collect()))
}

/// `GET /api/saved-searches` — pinned queries, oldest first.
pub async fn list_saved_searches(
    State(store): State<Shared>,
) -> Result<Json<Vec<SavedSearchDto>>, AppError> {
    let saved = blocking(store, ThoughtStore::list_saved_searches).await?;
    Ok(Json(saved.iter().map(SavedSearchDto::from).collect()))
}

#[derive(Deserialize)]
pub struct SavedSearchBody {
    pub name: String,
    pub query: String,
}

/// `POST /api/saved-searches` — pin a named query.
pub async fn create_saved_search(
    State(store): State<Shared>,
    Json(body): Json<SavedSearchBody>,
) -> Result<(StatusCode, Json<SavedSearchDto>), AppError> {
    let saved = blocking(store, move |s| s.create_saved_search(&body.name, &body.query)).await?;
    Ok((StatusCode::CREATED, Json(SavedSearchDto::from(&saved))))
}

/// `DELETE /api/saved-searches/{id}` — unpin a query (a no-op if it's gone).
pub async fn delete_saved_search(
    State(store): State<Shared>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let id = parse_id(&id)?;
    blocking(store, move |s| s.delete_saved_search(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct SyncRequest {
    /// The client's high-water mark into the change feed (`None` = full pull).
    pub since: Option<String>,
    /// The client's locally-modified rows to push (its outbox).
    #[serde(default)]
    pub changes: Vec<ThoughtChangeDto>,
}

#[derive(Serialize)]
pub struct SyncResponse {
    /// Server changes since `since`, for the client to apply (last-writer-wins).
    pub changes: Vec<ThoughtChangeDto>,
    /// The cursor to send as `since` next time (echoes the request when the feed
    /// is empty). When `changes` is `SYNC_PAGE_LIMIT` long, sync again to drain.
    pub cursor: Option<String>,
}

/// `POST /api/sync` — the two-way reconcile. Applies the client's pushed changes
/// (last-writer-wins by `updated_at`), then returns the server's changes since
/// the client's cursor. The server is authoritative and keeps no outbox of its
/// own — the web app already reflects this same store live.
pub async fn sync(
    State(store): State<Shared>,
    Json(req): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, AppError> {
    let since = req.since.as_deref().map(decode_sync_cursor).transpose()?;
    // Validate + convert incoming changes outside the store lock.
    let incoming = req
        .changes
        .iter()
        .map(ThoughtChange::try_from)
        .collect::<Result<Vec<_>, _>>()?;

    let feed = blocking(store, move |s| {
        for change in &incoming {
            s.apply_remote(change)?;
        }
        s.changes_since(since, SYNC_PAGE_LIMIT)
    })
    .await?;

    let cursor = feed
        .last()
        .map(SyncCursor::after)
        .map(encode_sync_cursor)
        .or(req.since);
    Ok(Json(SyncResponse {
        changes: feed.iter().map(ThoughtChangeDto::from).collect(),
        cursor,
    }))
}

/// `GET /healthz` — liveness probe.
pub async fn healthz() -> &'static str {
    "ok"
}

// ── plumbing ─────────────────────────────────────────────────────────────────

/// Run a synchronous store operation on a blocking thread, holding the store
/// lock only for the duration of the call. Recovers a poisoned lock rather than
/// propagating the panic — a poisoned mutex shouldn't take the whole server down.
async fn blocking<T, F>(store: Shared, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&ThoughtStore) -> buoy_core::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().unwrap_or_else(PoisonError::into_inner);
        f(&guard)
    })
    .await
    .map_err(|e| AppError::internal(&format!("store task failed: {e}")))?
    .map_err(AppError::from)
}

/// API error with an HTTP status and a JSON `{ "error": … }` body.
pub enum AppError {
    NotFound(String),
    BadRequest(String),
    Internal(String),
}

impl AppError {
    fn bad_request(msg: &str) -> Self {
        Self::BadRequest(msg.to_owned())
    }
    fn internal(msg: &str) -> Self {
        Self::Internal(msg.to_owned())
    }
}

impl From<buoy_core::Error> for AppError {
    fn from(e: buoy_core::Error) -> Self {
        match e {
            buoy_core::Error::NotFound { .. } => Self::NotFound(e.to_string()),
            other => Self::Internal(other.to_string()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::NotFound(m) => (StatusCode::NOT_FOUND, m),
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Self::Internal(m) => {
                tracing::warn!(error = %m, "request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, m)
            }
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}
