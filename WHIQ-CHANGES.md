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
(`cargo xtask models`, plus `models/` for Arabic); the Arabic tests need craft-fonts
(`CRAFT_FONTS_DIR`).

## 4. Release

`.github/workflows/whiq-release.yml`: dispatch with a `whiq-v*` tag. `pdfcraft-cli` for Linux
x86_64 (musl, static) and Windows x86_64 (static CRT), craft-fonts at PdfCraft's own pin, the OCR
models in `models/` beside the binary (the ocrs pair fetched by SHA-256; the Arabic model and its
character list from the repository's `models/`, with `LICENSE-APACHE-2.0.txt`); the contract suite runs against the built binary on both
before anything is published. Assets `pdfcraft-cli-<tag>-linux-x86_64.tar.gz`,
`pdfcraft-cli-<tag>-windows-x86_64.zip`, `SHA256SUMS`, `engines.lock`.

## 5. Arabic OCR (`crates/ocr`)

PdfCraft's OCR reads Arabic as well as Latin, with no setting, so every scanned PDF can be read and
made searchable here, mixed Arabic and English pages included (construction documents in Egypt
and the Gulf). The platform's vision OCR can be kept for pictures.

**Decision (2026-10-31).** Scored 1–5 against the scans in §5.3 and the needs of a commercial,
static binary for Windows and Linux:

| Option | Arabic | Mixed | Licence | Size | CPU speed | Win + Linux build | Effort | Total /35 |
|---|---|---|---|---|---|---|---|---|
| **PaddleOCR Arabic recogniser (PP-OCRv5 mobile) as ONNX, run by rten on ocrs's detector** | 4 | 5 | 5 (Apache-2.0) | 4 (8 MB) | 3 | 5 (pure Rust; rten is already in the tree) | 4 | **30** |
| Tesseract 5 `ara`+`eng` (tessdata_best) through a Rust binding | 4 | 3 | 5 (Apache-2.0) | 3 (28 MB) | 4 | 1 (C++ libtesseract + leptonica, static for musl and MSVC) | 2 | 22 |
| EasyOCR Arabic (CRNN) as ONNX | 2 | 2 | 5 | 4 | 3 | 5 | 3 | 24 (estimated, not measured) |
| An Arabic model trained for ocrs (ocrs-models) | ? | ? | 5 | 5 | 3 | 5 | 1 (no Arabic in HierText; needs data and weeks of GPU time) | rejected |
| Surya; VLM OCR (Qari, Qwen-VL) | 5 | 5 | 1 (GPL or restricted weights; GBs) | 1 | 1 | 3 | 3 | excluded |

Tesseract was measured on the same scans (5.3.4, `-l ara+eng`, best data). EasyOCR and the VLMs
are scored from their published figures only. ocrs has no Arabic model; its models can be trained
(the alphabet is a parameter), but there is no Arabic training set for them.

### 5.1 How it reads

- ocrs's detector finds the words and lines as before. It finds Arabic words well.
- The Arabic model also reads every line (`models/arabic-recognition.onnx`: PaddleOCR's
  `arabic_PP-OCRv5_mobile_rec`, Apache-2.0, converted to ONNX without changing its weights; its
  character list is `models/arabic-recognition.txt`). It reads a deskewed strip cut along the line,
  48 px high. The strip has little room above and below, because more room shrinks the letters. A
  line read unsurely is read again with room for vowel marks. A line where the model reads Arabic
  letters is an Arabic line; every other line keeps ocrs's reading.
- Inside an Arabic line, the Arabic model drops or garbles Latin words and numbers. A word box is
  read again alone by both models when the line's reading of it is mostly Latin or digits, is empty,
  holds far fewer characters than its width, or is unsure. In a line mostly in Latin, its Arabic
  words are read alone too. Then an Arabic word keeps the Arabic model's reading, a Latin word takes
  ocrs's, and a number takes the Arabic model's.
- `crates/ocr/src/bidi.rs` turns the visual line (as drawn, left to right) into typed order. It
  follows UAX #9 as one line's display applies it, after `pdfcraft-edit`'s `bidi::to_logical`:
  Arabic reads right to left, Latin words and numbers left to right where they stand, brackets are
  mirrored, and `%` goes with the line. A mixed line takes its paragraph's direction: the margin it
  keeps to, else the page's majority.
- An Arabic page's lines come out in its reading order: rows top to bottom, side-by-side lines right
  to left. `read` mirrors its column cut on such pages.
- With the Arabic model loaded, two kinds of line are dropped as noise: lines of specks, and lines
  both models read unsurely as six characters or fewer.

### 5.2 Text layer and `read`

Words Helvetica (WinAnsi) can hold stay in `/PCHelv`, exactly as before. Other words (Arabic) go
in `/PCOcrU`, a glyphless Type0 font: Identity-H, one code per UTF-16 code unit, one empty glyph
1 em wide, and an identity `/ToUnicode`. The font is generated in code
(`pdfcraft_fonts::glyphless`), so no font file ships, and `pdfcraft_edit::stamp` adds it once per
document. Arabic words are drawn in visual order and each line's words left to right, so
extraction gives them back in typed order. Poppler's `pdftotext` returned the minutes page
exactly, and PdfCraft's own `read` of the layer was right on the Arabic contract page. Arabic runs
inside English lines were last checked before the final fix to word order, when both extractors
still misplaced them; the fix is covered by a unit test, not by a fresh read-back. `search` finds Arabic words. `read` with OCR returns lines in typed order.
`server_status.ocr.languages` says what is installed, and `PDFCRAFT_OCR_ARABIC=0` turns Arabic off.

### 5.3 Measured

The test scans are synthetic and contributor-original construction text: contract clauses,
specifications, meeting minutes with English terms, codes and numbers, a bilingual bill of
quantities, an English transmittal with Arabic names, a Gulf letter with Arabic-Indic digits, a
fully vowelled page, and an English specification. LibreOffice laid them out (Noto Naskh and Noto
Sans Arabic, Liberation), and they were rasterised at 300 and 200 dpi. Each page also has a noisy
copy (200 dpi, blur, grain, specks, JPEG 35) and a copy turned 3°. The work machine had no Arabic
scans, so no customer files were used.

`cargo run -p pdfcraft-ocr --release --example ocr_eval -- OUT page.png…` reproduces the readings.
CER is the character error rate in reading order. Word acc. is 1 minus the word edit distance.
Tables, whose cell order is ambiguous, are scored by word recall.

| Pages | Before (ocrs only) | Now | Tesseract 5 best `ara+eng` |
|---|---|---|---|
| Arabic, clean 300 dpi (2 pages) | CER 88 %, words 0 % | **CER 0.9 %, word acc. 98.2 %** | CER 2.8 %, 96.9 % |
| Arabic, clean 200 dpi | CER 88 %, 0 % | **CER 0.9 %, 97.3 %** | CER 0.9 %, 99.2 % |
| Arabic, noisy | 0 % | **CER 2.6 % and 12.2 %, word recall 97 %** | CER 6.7 %, 88 % |
| Arabic, turned 3° | 0 % | **CER 0.9 %, 97.6 %** | CER 3.4 %, 92.7 % |
| Mixed Arabic/English, clean 300 dpi (minutes, transmittal, Gulf letter) | CER 63 %, 26 % | **CER 3.8 %, 91.2 %** (minutes 97 %) | CER 10 %, 83 % |
| Mixed, turned 3° | CER 64 %, 23 % | **CER 4.3 %, 90.5 %** | CER 10 %, 83 % |
| Bilingual BOQ table (word recall) | about 30 % | **82–85 %** | 26–65 % |
| Fully vowelled Arabic (tashkeel), word recall | 0 % | 7–39 % (weak) | 29–61 % |
| English, clean and turned | 99.4 % (200 dpi), 91.3 % (300), 78.5 % (3°) | **byte-identical to before** | 99–100 % |
| English, noisy | word acc. −22 %, recall 90.7 % | word acc. 32 % (fewer junk lines), recall 90.7 % | 95 % |

Speed is wall-clock time on a 4-core machine shared with other builds (load 12–16), so the absolute
times are inflated. An A4 page at 300 dpi takes about 2.3 times as long as ocrs alone: Arabic pages
7.5–10 s against 3.3 s, Latin pages 11.7 s against 5.4 s, because the Arabic model reads every line
to tell the scripts apart. A noisy page takes about 20 s. Tesseract took 3–4 s a page under the
same load.

### 5.4 Limits

- Fully vowelled text: the model drops most harakat and misreads such lines. Tesseract reads it
  better. Construction documents rarely carry full tashkeel.
- Every line is read by both models, so Latin-only documents pay the extra time too.
- English through ocrs remains weaker than Tesseract's (91 % at 300 dpi on these pages). This
  change does not touch it.
- Heavy noise: most junk lines are dropped, but not all.
- Tables: cell order across a row is approximate.
- Pages turned 90° or upside down are still not straightened.
- Hebrew and other scripts are not read.
