# Development Roadmap

Prioritized improvements for `appflowy-mcp` (and the `appflowy-cli` shim),
distilled from a full code audit of the server plus a gap analysis against the
AppFlowy-Cloud REST API. Ordered by value / risk, not by area.

Legend: **[ ]** todo · **[~]** in progress · **[x]** done

---

## Tier 1 — high value, low risk, fixes problems already hit in the field

### P1 — Raw collab fetch as an export fallback  **[~]**
- **Problem:** export and `appflowy_get_page` only call `GET /page-view/{id}`,
  whose handler routes through a lazy `ws_server` path — exactly where the
  transient/"Collab not found"/empty-document failures come from. Some pages
  (e.g. the ~19 "unreachable" pages seen exporting the `infrastructure`
  workspace) never resolve there.
- **Fix:** AppFlowy exposes `GET /api/workspace/{ws}/collab/{object_id}`
  (collab_type=Document), which reads `collab_storage.get_full_encode_collab`
  **directly**, bypassing that path, and returns the same yjs `encoded_collab`
  the MCP already decodes. Add a client method + an `appflowy_get_collab` tool,
  and use it as a fallback in `fetch_page_markdown` when page-view yields no
  content after retries. **May recover pages currently believed lost.**

### P2 — Batch collab fetch + parallel export  **[ ]**
- **Problem:** `export_views_to_directory` fetches one page at a time on a
  blocking `httpx.Client`; N pages = N serial round-trips → minutes on large
  workspaces.
- **Fix:** use `GET/POST /api/workspace/{ws}/collab_list`
  (`batch_get_collab_handler`) to fetch many collabs per request, and/or a
  bounded thread pool over the flattened view list. Pairs with P1.

### Markdown round-trip fidelity  **[ ]**
- Nested/indented lists are flattened on import (`markdown.py` strips indent)
  while export renders nesting → export→import collapses hierarchy. Build a
  block tree by indent depth.
- Tables are silently dropped both directions. At minimum warn on unhandled
  block types during export; ideally parse/render GFM tables.
- Headings h4–h6 not parsed on import (export emits up to 6) → no round-trip.
- Export does not escape Markdown metacharacters in inline text.
- Underscore emphasis (`_x_`/`__x__`) and nested inline (e.g. bold link) lost
  on import.

---

## Tier 2 — robustness / correctness

- **Do not auto-retry non-idempotent POSTs on 429/5xx** — `create_row` /
  `create_page` / `append-block` / `invite_members` could be duplicated.
  Restrict auto-retry to GET and pre_hash'd PUT.
- **Serialize token refresh** — concurrent 401s can both refresh; GoTrue rotates
  refresh tokens, so the second refresh fails and can log the session out. Guard
  with a lock + re-check inside it.
- **Typed errors** — 4xx responses drop AppFlowy's numeric `code`; every tool
  re-wraps as a flat `Exception`. Introduce `AppFlowyError(status, code, msg)`.
- **Test the HTTP client layer** — reauth-once, backoff/`retry-after`, the
  "HTTP 200 with non-zero envelope code" business-error path, and row-id
  batching are all currently untested.
- Replace the bare `except Exception` retry in `fetch_page_markdown` so real
  decode bugs aren't masked as transient failures.

---

## Tier 3 — new user-facing capabilities

- **Search → summary (RAG answer)** — `GET /api/search/{ws}/summary` turns the
  existing search results into a cited natural-language answer over the user's
  own notes. `appflowy_search_summary(ws, query)`.
- **Import AppFlowy native `.zip`** — `POST /api/import` (+ `create` / detail
  polling) ingests AppFlowy's own export, preserving databases/boards/relations
  that Markdown import flattens.
- **Duplicate a published page/template** — `POST /.../published-duplicate`.
- **Comments & reactions on published pages** —
  `/.../published-info/{view_id}/comment` and `/reaction`.
- **Incremental export** — skip views whose `last_edited_time` predates the
  existing file's mtime; today export requires an empty destination.

---

## Tier 4 — stretch (biggest feature ceiling)

- **Edit existing document content** — `POST /.../collab/{id}/web-update` applies
  arbitrary yjs updates (modify / reorder / delete blocks). The MCP can only
  *append* today. Feasible since `pycrdt` is already a dependency, but requires
  constructing yjs updates client-side. Large effort.

---

## Notes

- Non-gaps confirmed during analysis: `appflowy_update_page` already covers the
  dedicated update-name/icon/extra endpoints; `appflowy_search` already matches
  the search DTO exactly (only the `/summary` companion is missing);
  `get_database_row_details` is already chunked to bound URL length.
- Known limitation to document: MDX/Docusaurus constructs (`:::note`
  admonitions, `<Tabs>`) and inline HTML pass through as literal paragraph text.
