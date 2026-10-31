# WHIQ changes to PdfCraft

This is the White Hat IQ fork of PdfCraft (branch `whiq`). The platform runs `pdfcraft-cli mcp`
headless as its PDF craft engine, one process per chat and document, behind its supervisor
(`craftSupervisor.ts`), with the read and write roots set to the chat's staging folders and the
author set to the caller. The server answers with the shared "no dead stops" contract of the
WordCraft, GridCraft and DeckCraft forks (their `CONTRACT-PARITY.md`): same error object and
eighteen codes, flags, `server_status`, `job_status`, `list_jobs`, `cancel`, `evict`, document
handles, `whiq/files` annotations, checkpoint and recovered-file names.

`pdfcraft-cli mcp --classic` (or the upstream-only `--compact` / `--root`) still runs upstream's
tool-table MCP server unchanged.

## 1. The server (`crates/automation/src/whiq/`)

- `mod.rs` — the runtime. One worker thread owns the automation session; the stdio loop never
  waits on it. A call arriving while another runs waits `--busy-after-ms` (300) then answers
  `busy` with `retry_after_ms`, `running`, `job_id`. A call still running after `--job-after-ms`
  (15000) answers `{job_id, status: running}` and carries on (`job_status` keeps its result and
  progress; `async: true` or a file of `--open-job-mb` (20) or more is a job at once). `cancel`
  stops a call at its next step; a cancelled `edit` leaves the document as it was. Progress
  notifications for a `_meta.progressToken`. Unsaved work is written to
  `<name>.recovered-<YYYYMMDD-HHMMSS>.pdf` in the write root on `evict`, idle
  (`--idle-evict-secs`), memory (`--max-memory-mb`, after a call, never during one), panic,
  hang-up and SIGTERM (exit 143), and every `--checkpoint-edits` (20) edits or
  `--checkpoint-secs` (60); a later process names the files it finds (`recovered_files`,
  reason `found`). A released document answers `evicted` with `recovered_path` / `original_path`.
- `contract.rs` — the error object, the eighteen codes, forgiving tool and argument names
  (case, camelCase, synonyms, numbers and booleans as strings, enum values in any case), page
  selections (`"1-3,7"`, `"10-"`, `"last"`, lists), and results cut at their largest list with
  `continue_from`.
- `defs.rs` — the tools and their schemas, annotations and `_meta["whiq/files"]`.
- `engine.rs` — documents by handle (`doc-N`), paths (reads in the read root, then the write
  root; writes in the write root), `OLD_<name>` renames unless `overwrite` or
  `--automation-no-old-copies`, checkpoints and eviction.
- `read.rs` — structured, windowed reading with OCR for pages without a text layer.
- `preview.rs` — the preview pane's renderer: SVG (vector, with an invisible selectable text
  layer) or PNG, windowed.
- `edit.rs` — the batch editor and `list_operations`.
- `convert.rs` — images, text, HTML and Markdown to PDF (a typesetter for HTML and Markdown).

Other changes, each small and additive: `pdfcraft-render` gains `svg` (`SvgRenderer`,
hayro-svg 0.7 over the same vendored hayro-interpret); `pdfcraft-engine` gains
`Document::export_pages_window` (the export model for a window of pages) and
`Session::batch_checkpoint` / `batch_restore` (all-or-nothing batches beyond the 100-step undo
limit) and `Session::create_from_document`; `pdfcraft-export` gains `markdown_pages` (Markdown per
page); `pdfcraft-create` exposes `append_page` and `set_document_title`.

## 2. Tools

| Tool | What it does |
|---|---|
| `server_status`, `job_status`, `list_jobs`, `cancel`, `evict` | the shared craft-engine tools |
| `open_document`, `new_document`, `save_document`, `close_document`, `document_info` | documents by handle |
| `read` | pages as Markdown (headings, pipe tables, paragraphs) or plain text, OCR for scans, metadata, windowed |
| `search` | find text: page and rectangles per match |
| `list_content` | comments, fields, bookmarks, images, links, attachments, fonts, layers, pages |
| `render_preview` | SVG (vector + text layer) or PNG of a window of pages |
| `edit` | a batch of operations (named ones, or any automation tool), atomic or keep_going |
| `list_operations` | the operations and their schemas |
| `convert` | images / text / HTML / Markdown to PDF |
| `combine` | files to one PDF |
| `ocr` | a searchable PDF from a scan |

## 3. Tests

`cargo test -p pdfcraft-cli --test whiq` drives the binary over stdio as the supervisor does
(`PDFCRAFT_CLI=path` for another binary — the release workflow's). The OCR tests need the models
(`cargo xtask models`); the Arabic test needs craft-fonts (`CRAFT_FONTS_DIR`).

## 4. Release

`.github/workflows/whiq-release.yml`: dispatch with a `whiq-v*` tag. `pdfcraft-cli` for Linux
x86_64 (musl, static) and Windows x86_64 (static CRT), craft-fonts at PdfCraft's own pin, the OCR
models in `models/` beside the binary; the contract suite runs against the built binary on both
before anything is published. Assets `pdfcraft-cli-<tag>-linux-x86_64.tar.gz`,
`pdfcraft-cli-<tag>-windows-x86_64.zip`, `SHA256SUMS`, `engines.lock`.
