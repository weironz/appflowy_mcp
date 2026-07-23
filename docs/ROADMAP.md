# Development Roadmap

Prioritized improvements for `appflowy-mcp` (and the `appflowy-cli` shim),
distilled from a full code audit of the server plus a gap analysis against the
AppFlowy-Cloud REST API. Ordered by value / risk, not by area.

Legend: **[ ]** todo · **[~]** partial · **[x]** done

Most of this landed in **0.7.0**. Items marked deferred need live testing
against a real workspace (they can silently corrupt data or need an external
upload flow) and were intentionally not shipped un-verified.

---

## Tier 1 — high value, low risk

### P1 — Raw collab fetch as an export fallback  **[x]** (0.6.5)
`fetch_collab_markdown` + the `appflowy_get_collab` tool read a document via
`GET /api/workspace/v1/{ws}/collab/{object_id}`, bypassing the lazy page-view
path; `fetch_page_markdown` falls back to it. Field result: for the 19
"unreachable" pages in the `infrastructure` workspace this endpoint *also*
returns nothing — confirming that content was never synced to the server (not a
fetch-method problem). The fallback still helps pages that exist server-side but
transiently fail page-view.

### P2 — Parallel export  **[x]** (0.7.0)
`export_views_to_directory` now prefetches every page's Markdown with a bounded
thread pool (`EXPORT_MAX_WORKERS=8`) and the tree walk reads from that cache.
Large workspaces go from minutes to seconds.
- Deferred: the dedicated **batch** endpoint (`/collab_list`). Its response
  encodes each collab as **bincode-serialized `EncodedCollab` bytes**, not JSON,
  so decoding it in Python is fragile; the thread pool gets the same win safely.

### Markdown round-trip fidelity  **[~]** (0.7.0)
- [x] Headings **h4–h6** now parse on import (`HEADING_PATTERN`); export already
  emitted up to level 6.
- [x] **Tables** (`simple_table`) now render as GFM tables on export (0.7.1),
  and math equations as `$$...$$`. Verified against a real page. Previously a
  workspace could silently lose hundreds of tables on every export.
- [x] Any remaining unhandled block type leaves a visible
  `<!-- unsupported block: TYPE -->` marker instead of vanishing.
- [x] **Import now parses tables, nested lists, math, and strips YAML
  front-matter** (0.7.3). GFM `| a | b |` -> simple_table (nested rows/cells),
  indented list items -> nested `children`, `$$...$$` -> math_equation, and a
  leading `---`…`---` front-matter block is dropped. Verified end-to-end
  (markdown -> AppFlowy page -> export round-trips to a real table/nesting/math).
  The nested-block create contract was validated live first.
- [ ] **Inline escaping** of `* _ | [ ] backtick` — deferred: requires a matched
  unescape on import (this parser has no backslash-escape handling), so shipping
  export-only escaping would break the round-trip. (Table cells DO unescape
  `\|`.)
- [ ] **Underscore emphasis** (`_x_` / `__x__`) — deliberately NOT done: the
  docs are full of `snake_case` identifiers that naive `_..._` would italicize.
- [ ] Nested inline (e.g. a bold link `**[t](u)**`) — needs a real tokenizer.

---

## Tier 2 — robustness / correctness  **[x]** (0.7.0)
- [x] Auto-retry now restricted to idempotent methods on 5xx (POST/PATCH no
  longer retried → no duplicate create/append); 429 still retried for all.
- [x] Token refresh serialized with a lock + re-check, so concurrent 401s
  (now reachable via the parallel export) can't spend the rotating refresh
  token twice and log the session out.
- [x] `AppFlowyError(message, status, code)` preserves the AppFlowy error code
  for 4xx (and the 200-envelope business errors).
- [x] The HTTP client layer is now unit-tested (`tests/test_client.py`):
  idempotent retry, 429 retry, 401 reauth, typed errors, refresh re-check.

---

## Tier 3 — new user-facing capabilities

- [x] **P3 — Search → summary** (0.7.0): `appflowy_search_summary` runs a
  document search then `GET /api/search/{ws}/summary`, returning cited summaries.
- [x] **P5 — Duplicate a published page/template** (0.7.0):
  `appflowy_duplicate_published_page` (`POST /.../published-duplicate`).
- [x] **P6 — Comments & reactions on published views** (0.7.0): get/add tools
  for both (`appflowy_get_published_comments` / `_add_published_comment` /
  `_get_published_reactions` / `_add_published_reaction`). Delete endpoints exist
  but are intentionally not exposed (destructive; add on request).
- [x] **P7 — Published outline** (0.7.0): `appflowy_get_published_outline`
  (`GET /api/workspace/published-outline/{namespace}`, public).
- [ ] **P4 — Import AppFlowy native `.zip`** — deferred (see research below).
- [ ] Incremental / skip-unchanged export (by `last_edited_time`).

---

## Tier 4 — stretch

### P8 — Edit existing document content — researched, **deferred**
Endpoint verified: `POST /api/workspace/v1/{ws}/collab/{object_id}/web-update`,
body `{ "doc_state": [<int bytes of a raw yjs update>], "collab_type": 0 }`.
Feasibility: **technically possible** — `pycrdt` is already a dependency, so one
can load the current document (from the collab endpoint's `doc_state`), mutate
the CRDT, and emit a yjs update. Risk: the update must encode a *valid* mutation
in AppFlowy's exact document schema (blocks / text_map / children_map maps); a
malformed update merged server-side can corrupt the document. This needs careful
live testing and a rollback story before shipping — **not done blind.** This is
the only true feature ceiling (the server is otherwise append-only via
`append-block`).

---

## Research notes

### P4 native import flow (verified, why it's deferred)
Not one call — a multi-step async flow:
1. `POST /api/import/create` `{ workspace_name, content_length }` → returns
   `{ task_id, presigned_url }` (creates a new empty workspace + a 10-min S3 URL).
2. Client **PUTs the raw zip to the presigned S3 URL** directly (out-of-band).
3. Poll `GET /api/import` → `{ tasks: [{ task_id, status, ... }], has_more }`;
   `status` is a numeric enum; a background worker does the actual import.
   (Alt path: `POST /api/import` multipart with `X-Content-Length` +
   `X-Content-MD5` headers, no task_id returned.) Max 3 pending tasks/user.
Deferred because the presigned-S3 PUT / MD5 handling and async polling can't be
verified offline and shouldn't ship un-tested.

### Contract gotchas (for future work)
- `CollabType` serializes as an **integer** (`Document=0`), not a string.
- Batch collab (`/collab_list`) returns **bincode** bytes, not JSON.
- `web-update` `doc_state` is an **int array of a raw yjs update**, not base64.
- `search/{ws}/summary` and `collab_list` (GET) both take a **JSON body on GET**.
