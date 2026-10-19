# Agent Guide — how an AI client should use this MCP server

Two delivery channels for this knowledge, pick either:

1. **Built-in**: the server's `initialize` response already carries a compact
   version of these rules in the MCP `instructions` field — clients read it at
   connection time with zero setup (the text lives in `src/instructions.md`).
2. **Paste it**: drop the block below (or your trimmed version) into your
   client's own instruction file — `CLAUDE.md` / `AGENTS.md` / a Cursor rule /
   a Codex system prompt — for workflows that go beyond the compact version.

## Pasteable block

```text
=== appflowy MCP usage rules ===
- Hierarchy: workspace -> space -> page. Never create content with workspace_id
  as parent_view_id (a child of the root becomes a Space). Pick ids via
  appflowy_list_spaces / appflowy_get_workspace_folder first; ids, not names.
- New page: appflowy_save_page (markdown default). Add to an existing page:
  appflowy_append_page_content. Native "Link to page": appflowy_append_page_link.
- Before any collab-level edit (appflowy_update_document_block /
  appflowy_delete_document_block), take appflowy_snapshot_document (or pass
  snapshot_path) — restore is available via appflowy_restore_document_snapshot.
- Bulk reads: appflowy_export_page/_space/_workspace (yjs-decoded markdown,
  inline formatting preserved). Tree exports are incremental — re-runs are cheap.
- Rows: appflowy_create_row WITH `document` to initialize the row body
  (the response carries document_id), appflowy_append_markdown_to_row for
  additions, appflowy_update_row/delete_row/restore_row for later changes.
  Never call page append tools with a raw row_id.
- Imports (markdown dir/CSV/docx/pdf/notion zip) are async server-side tasks;
  poll the matching status tool, don't re-fire.
- Validation failures come back as isError:true tool results with messages —
  correct the input and retry. API errors keep HTTP status/code in the message.
  code 1079 (permission-resolver busy) is transient — wait a bit and retry.
=== end ===
```

## Recipes

**Save an AI answer into AppFlowy**

1. `appflowy_list_spaces` → pick a space's `view_id`.
2. `appflowy_save_page` (`parent_view_id` = that id, `title`, `content`,
   keep `content_format: "markdown"`).
3. Later additions on the same thread: `appflowy_append_page_content` (also
   markdown by default).

**Nightly backup**

- MCP: `appflowy_export_workspace` the first time; reruns are incremental
  (unchanged pages are skipped with zero requests).
- CLI: `appflowy -w <ws> export -o ~/backups/appflowy-$(date +%F)` in cron.

**Meeting-notes row automation**

1. `appflowy_create_row` with `document` = the meeting summary markdown
   (initializes the row body atomically).
2. Later notes in that row: `appflowy_append_markdown_to_row`.
3. Fix metadata afterwards: `appflowy_update_row` (cells by field name),
   and if a row was filed wrong, `appflowy_delete_row` (soft) /
   `appflowy_restore_row`.

**Safe editing of an existing page**

1. Read `appflowy_get_page`/`appflowy_export_page` to find the block.
2. `appflowy_snapshot_document` (one file captures the whole document).
3. `appflowy_update_document_block` / `appflowy_delete_document_block`.
4. If anything looks wrong: `appflowy_restore_document_snapshot` — full
  visible rollback (snapshot content is rebuilt as fresh blocks).

**Bulk content migration**

- Markdown folders: `appflowy_import_markdown_directory` (images deduped +
  parallel-uploaded; sibling order preserved).
- Tables of data: `appflowy_import_csv` (grid/board/calendar).
- Word/PDF/HTML: `appflowy_import_document`. Notion export: `appflowy_import_notion_zip`.
- Everything above is async: poll `appflowy_get_csv_import_status` /
  `appflowy_get_document_import_status` / `appflowy_get_import_task`.

## Notes for implementers

- The protocol-level instructions shipped with the server intentionally stay
  under ~2 KB (they ride every `initialize`). This document is the long-form
  reference; keep the two consistent when behavior changes.
- Field-level usage lives in each tool's `description` (served via
  `tools/list` — see [tools.md](tools.md) for the catalog).
