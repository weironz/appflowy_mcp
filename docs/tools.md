# Tool Reference (128 tools)

Every tool the MCP server exposes, grouped by domain. Names are exact
(`appflowy_*`); inputs and defaults are described inline where they matter.
For agent-facing field descriptions, ask the client for the tool schema
(`tools/list`), which mirrors the serde/JsonSchema definitions in `src/tools/`.

- Authentication · Workspaces & Spaces · Workspace Management · Members ·
  Pages · Page Content · Document Editing · Page Content Import (Markdown) ·
  Markdown Export · Server-Side Export (PDF/ZIP) · Databases & Rows ·
  Database Row Documents · Database History · Search · Publishing ·
  Quick Notes · AI Chat · Meeting Transcription · Document Comments ·
  Page Sharing · CSV/Document Import · Notion Import · Notifications ·
  User & Files

### Authentication

- `appflowy_login`
- `appflowy_refresh_token`

### Workspaces And Spaces

- `appflowy_list_workspaces`
- `appflowy_get_workspace_folder`
- `appflowy_list_spaces`
- `appflowy_create_space`
- `appflowy_update_space`

### Workspace Management

- `appflowy_create_workspace`
- `appflowy_update_workspace`
- `appflowy_delete_workspace`
- `appflowy_leave_workspace`
- `appflowy_get_workspace_settings`
- `appflowy_update_workspace_settings`
- `appflowy_get_workspace_usage`

Note: creating extra workspaces requires a paid AppFlowy Cloud plan; the free plan returns a workspace-limit error.

### Members And Invitations

- `appflowy_list_members`
- `appflowy_update_member`
- `appflowy_remove_members`
- `appflowy_invite_members`
- `appflowy_list_invitations`
- `appflowy_accept_invitation`

Member roles are `Owner`, `Member`, or `Guest`.

### Pages

- `appflowy_create_page`
- `appflowy_get_page`
- `appflowy_get_collab`
- `appflowy_update_page`
- `appflowy_move_page`
- `appflowy_reorder_pages`
- `appflowy_reorder_favorite`
- `appflowy_duplicate_page`
- `appflowy_move_page_to_trash`
- `appflowy_restore_page_from_trash`
- `appflowy_delete_page_from_trash`
- `appflowy_restore_all_from_trash`
- `appflowy_empty_trash`
- `appflowy_favorite_page`
- `appflowy_list_trash`
- `appflowy_list_favorites`
- `appflowy_list_recent`

AppFlowy has no bulk-sort endpoint; sibling order is controlled by the move endpoint's `prev_view_id`. `appflowy_reorder_pages` builds on it to reorder all direct children of a space or page in one call — pass an explicit `page_ids` order, or `sort_by` one of `name`, `created_at`, `last_edited_time` (with optional `descending`); pages already in place are skipped. `appflowy_reorder_favorite` reorders a page within the favorites list.

### Page Content

- `appflowy_save_page`
- `appflowy_append_page_content`
- `appflowy_append_text_to_page`
- `appflowy_append_blocks_to_page`
- `appflowy_create_markdown_page`
- `appflowy_append_markdown_to_page`
- `appflowy_append_page_link`
- `appflowy_import_markdown_file`
- `appflowy_import_markdown_directory`

Page-content support covers appending new document blocks, creating pages from Markdown, appending Markdown to existing pages, and appending a native `linked_page` block (`appflowy_append_page_link`, the GUI "Link to page" equivalent — not a Markdown hyperlink). Appending goes through AppFlowy's `append-block` endpoint, **which prepends what it receives at every level** (batched calls landed out of order; table rows and row cells flipped) — these tools work around it by sending one top-level block per call with recursively pre-reversed children. AppFlowy exposes no high-level REST endpoint for deleting or editing arbitrary existing blocks; for that, see Document Editing below. Page-level deletion through trash is supported.

### Document Editing (collab-level)

- `appflowy_update_document_block`
- `appflowy_delete_document_block`
- `appflowy_snapshot_document`
- `appflowy_restore_document_snapshot`

For editing existing content — what `append-block` cannot do. These go through AppFlowy's `collab/web-update` endpoint with a `yrs` state-vector diff: `appflowy_update_document_block` replaces the ENTIRE text of one block (inline markdown semantics, text-bearing types only), `appflowy_delete_document_block` detaches a block from its parent's children (the record stays as invisible CRDT history). Snapshots capture the full document state to a local file before risky edits; restore **detaches the current content and rebuilds the snapshot as fresh blocks** (monotone yjs delete sets mean a raw state push-back cannot revive anything — verified live; this rebuild is the strongest restore CRDT allows). All four mutate the live document — use snapshots.

When saving AI answers, notes, summaries, or generated content into AppFlowy, use `appflowy_save_page` by default. Its `content_format` field defaults to `markdown`, so agents do not need to ask users to say "save as Markdown" every time. Set `content_format` to `plain_text` only when the user explicitly wants the content preserved as literal plain text.

Use `appflowy_append_page_content` by default when adding AI-generated content to an existing page. It also treats content as Markdown unless `content_format` is set to `plain_text`.

Example paragraph block:

```json
{
  "type": "paragraph",
  "data": {
    "delta": [
      {
        "insert": "Hello from MCP"
      }
    ]
  }
}
```

Example Markdown page:

```json
{
  "parent_view_id": "space-or-page-view-id",
  "title": "Meeting Notes",
  "content": "# Meeting Notes\n\n- [ ] Follow up\n- **Important** decision\n\n```python\nprint(\"hello\")\n```",
  "content_format": "markdown"
}
```

Markdown conversion supports headings, paragraphs, dividers, bullet lists, numbered lists, todo lists, quotes, code blocks, GFM tables, math equations, image links, YAML front-matter stripping, and inline bold, italic, strikethrough, code, and links.

### Markdown Import

Use `appflowy_import_markdown_file` to import one local Markdown file as an AppFlowy page. Use `appflowy_import_markdown_directory` to recursively import a local folder tree.

Directory import maps local structure directly to AppFlowy:

```text
local folder -> AppFlowy page
local subfolder -> AppFlowy subpage
local .md/.markdown file -> AppFlowy page
```

`README.md` or `index.md` becomes the content of its folder page instead of a separate child page. Other Markdown files in the same folder become child pages. Hidden folders plus `.git`, `.hg`, `.svn`, `.idea`, `.vscode`, `node_modules`, and `__pycache__` are skipped.

Local image references are resolved relative to the Markdown file, uploaded to AppFlowy file storage in parallel (deduplicated per file), and replaced with AppFlowy file URLs:

```markdown
![Architecture](./images/architecture.png)
![Screenshot](../assets/screenshot.png)
```

Remote image URLs such as `https://...` are kept as-is. Missing local images are reported in the import result warnings, while other files continue importing.

Example directory import:

```json
{
  "parent_view_id": "space-or-page-view-id",
  "path": "D:\\notes",
  "upload_assets": true
}
```

Example single-file import:

```json
{
  "parent_view_id": "space-or-page-view-id",
  "path": "D:\\notes\\MCP\\config.md",
  "title": "MCP Config",
  "upload_assets": true
}
```

### Markdown Export

- `appflowy_export_page`
- `appflowy_export_space`
- `appflowy_export_workspace`

For Markdown there is no server-side export format, so these tools rebuild it from the page collab data. The raw yjs update returned by the page-view endpoint is decoded natively in Rust ([yrs](https://github.com/y-crdt/y-crdt), the reference implementation of Yjs), which preserves inline formatting (bold, italic, strikethrough, inline code, links) that the server's JSON conversion would flatten.

`appflowy_export_page` writes one page to a local `.md` file. `appflowy_export_space` exports a space (or any page subtree) as a directory tree that mirrors the import convention — a page with children becomes a folder whose own content is `README.md`, leaf pages become `.md` files. `appflowy_export_workspace` does the same for every space in the workspace. Page Markdown is prefetched concurrently (bounded at 8 workers) with per-page retry and fallback to the raw collab endpoint; pages that still fail are reported as warnings with full attempt traces instead of aborting the run. Non-document views (grids, boards, calendars) are skipped and reported in warnings, and the destination directory must be empty so existing files are never overwritten.

### Server-Side Export (PDF / ZIP)

- `appflowy_export_pdf` — newer commercial-cloud endpoint that renders a page (with nesting, databases and images) to PDF on the server and downloads the bytes to a local path; filename is recovered from `Content-Disposition` (RFC 6266). Higher fidelity than the Markdown rebuild for print/share use cases.
- `appflowy_export_workspace_zip` — owner-only full-workspace backup: returns a `task_id`; the server builds the ZIP asynchronously (with optional file attachments) and emails the owner a download link.

### Databases And Rows

- `appflowy_list_databases`
- `appflowy_get_database_fields`
- `appflowy_create_database_field`
- `appflowy_list_rows`
- `appflowy_get_row_details`
- `appflowy_create_row`
- `appflowy_upsert_row`
- `appflowy_get_updated_rows`
- `appflowy_update_row`
- `appflowy_delete_row`
- `appflowy_restore_row`

`appflowy_create_database_field` field types: 0=RichText, 1=Number, 2=DateTime, 3=SingleSelect, 4=MultiSelect, 5=Checkbox, 6=URL, 7=Checklist.

`appflowy_update_row` rewrites cells by **field name** through the collab web-update path (all field types incl. select options and checklist operations by task name); `appflowy_delete_row`/`appflowy_restore_row` soft-delete and restore the way the web UI does (flagged in every view). `appflowy_upsert_row` derives row ids from `pre_hash` and never shows the row in views — prefer `appflowy_create_row` to create and `appflowy_update_row` to edit.

### Database Row Documents

- `appflowy_append_markdown_to_row`

A database row's page body is a **separate** Document collab addressed by `UUIDv5(namespace=row UUID, name="document_id")` — pass `document` to `appflowy_create_row` to initialize it atomically (the response includes `document_id`); calling page append tools with a bare row id hits the wrong collab (`Document/{id} is not active`). `appflowy_append_markdown_to_row` resolves the derived document and appends there.

### Database History

- `appflowy_list_database_history`
- `appflowy_get_database_restore_state`

Read-only revision history of a database (versions + timestamps, with paging) and whether it currently sits on a restore point. Preview and restore jobs are not exposed yet.

### Search

- `appflowy_search`
- `appflowy_search_summary`

Full-text search across a workspace. `appflowy_search_summary` follows up with the server's summary endpoint and returns cited summaries. Newly written content can take a short while to appear while the server indexes it.

### Publishing

- `appflowy_publish_page`
- `appflowy_unpublish_page`
- `appflowy_get_publish_namespace`
- `appflowy_set_publish_namespace`
- `appflowy_list_published_views`
- `appflowy_get_published_outline`
- `appflowy_get_published_comments`
- `appflowy_add_published_comment`
- `appflowy_get_published_reactions`
- `appflowy_add_published_reaction`
- `appflowy_duplicate_published_page`

`appflowy_publish_page` returns the publish info (namespace and publish name) for building the public URL. On older server deployments that lack the list endpoint, `appflowy_list_published_views` falls back to walking the workspace folder tree (detected via the structured 404 status, not error-string matching).

### Quick Notes

- `appflowy_list_quick_notes`
- `appflowy_create_quick_note`
- `appflowy_update_quick_note`
- `appflowy_delete_quick_note`

Quick note tools accept plain `text` (converted to paragraph blocks) or raw `data` JSON.

### AI Chat

- `appflowy_create_chat`
- `appflowy_delete_chat`
- `appflowy_get_chat_settings`
- `appflowy_update_chat_settings`
- `appflowy_list_chat_messages`
- `appflowy_chat_ask`

Create a chat with `appflowy_create_chat` (optionally passing `rag_ids`, page view ids the AI may use as context), then ask questions with `appflowy_chat_ask`, which posts the question and waits for the complete AI answer (non-streaming).

### AI Meeting Transcription

- `appflowy_meeting_transcribe`
- `appflowy_meeting_get_transcript`
- `appflowy_meeting_get_paragraphs`
- `appflowy_meeting_delete_transcript`
- `appflowy_meeting_usage`

`appflowy_meeting_transcribe` accepts a local audio file (uploaded via a presigned storage URL — the AppFlowy bearer token is never sent to storage) or an existing audio URL, then submits it for AI transcription returns a full transcript with empty text only when the audio is silent. Poll `appflowy_meeting_get_transcript` (status `queued` → `processing` → `completed`/`error`), then fetch speaker-labeled paragraphs with word-level timestamps.

### Document Comments

- `appflowy_list_inline_comments`
- `appflowy_add_inline_comment`
- `appflowy_resolve_inline_comment`
- `appflowy_delete_inline_comment`
- `appflowy_list_inline_comment_reactions`
- `appflowy_add_inline_comment_reaction`
- `appflowy_delete_inline_comment_reaction`
- `appflowy_notify_row_comment`

Inline annotations on a document page (distinct from published-page public comments) — the read/review loop for human-AI collaboration: an agent writes content, a reviewer leaves and resolves comments in the document. Deletion is a soft delete server-side (`is_deleted: true`); the v2 create endpoint falls back to the legacy one on older deployments (404/405).

### Page Sharing

- `appflowy_share_page`
- `appflowy_revoke_page_access`
- `appflowy_get_page_access_details`
- `appflowy_request_access`
- `appflowy_get_access_request`
- `appflowy_approve_access_request`
- `appflowy_list_shared_with_me`

Share a page with collaborators by email at an access level — `10` ReadOnly, `20` ReadAndComment, `30` ReadAndWrite, `50` FullAccess (verified against AppFlowy-Web's `AccessLevel` enum). `appflowy_get_page_access_details` uses the v2 endpoint with a structured-404 fallback to the legacy ancestor-walk variant. Distinct from publishing: sharing grants named accounts in-app access, publishing exposes a public link.

### CSV / Document Import

- `appflowy_import_csv`
- `appflowy_get_csv_import_status`
- `appflowy_cancel_csv_import`
- `appflowy_import_document`
- `appflowy_get_document_import_status`

`appflowy_import_csv` creates/appends/replaces a database (grid, board or calendar) from a local CSV with dialect options (header, delimiter, quote, encoding). `appflowy_import_document` converts a local `.doc`/`.docx`/`.html`/`.pdf` file into a Document page under `parent_view_id`. Both are asynchronous server-side tasks (presigned upload, then poll the status tools).

### Notion Import

- `appflowy_import_notion_zip`
- `appflowy_get_import_task`
- `appflowy_cancel_import_task`

`appflowy_import_notion_zip` imports a Notion export `.zip` under a parent page (presigned upload incl. a multipart branch for large archives). Import runs as a background server-side task — poll `appflowy_get_import_task` (it reads the account-wide task list and filters your `task_id`), or abandon with `appflowy_cancel_import_task`.

### Notifications

- `appflowy_list_notifications`
- `appflowy_get_unread_count`
- `appflowy_mark_notifications_read`
- `appflowy_mark_all_notifications_read`
- `appflowy_archive_notifications`
- `appflowy_archive_all_notifications`

The workspace notification inbox (comments, mentions, share invitations) — the way for an agent to watch collaboration events.

### User And Files

- `appflowy_get_user_profile`
- `appflowy_upload_file`

`appflowy_upload_file` uploads a local file to AppFlowy file storage and returns a URL that image or file blocks can reference. Use the page `view_id` as `parent_dir`. Files of 5 MB and above upload automatically as multipart chunks (concurrent parts with retries, aborted cleanly on failure); smaller files use the single-shot upload.

