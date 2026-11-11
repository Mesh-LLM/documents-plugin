# pdf-plugin

> **⚠️ DRAFT — experimental, not for production use.** Tested on one Mac
> against mesh-llm `main` (fe87794b6). See [Gaps](#gaps).

A [mesh-llm](https://github.com/Mesh-LLM/mesh-llm) plugin that turns PDFs into
text with [Apache Tika](https://tika.apache.org/), including OCR (Tesseract)
for scanned pages. It needs no changes to mesh-llm.

It adds one tool, `extract`. mesh-llm publishes it:

- over **MCP**, as `pdf.extract`, on the console's `/mcp` endpoint (e.g.
  `http://127.0.0.1:3131/mcp`), together with every other plugin tool;
- over **HTTP**, as `POST /api/plugins/pdf/tools/extract` on the console port.

## Using it

**Agents** (goose, Claude Code, …) have PDFs as files on disk. Add mesh-llm's
`/mcp` as an MCP server and the agent can call `pdf.extract` on a path:

```sh
goose run --with-builtin developer \
  --with-streamable-http-extension "http://127.0.0.1:3131/mcp timeout=300" \
  -t "Use the pdf extract tool on ~/contract.pdf and list the termination terms"
```

**Anything else** (a UI, a script) can send the bytes over HTTP:

```sh
curl -XPOST localhost:3131/api/plugins/pdf/tools/extract \
  -H 'content-type: application/json' \
  -d "{\"data_base64\": \"$(base64 -i doc.pdf)\", \"filename\": \"doc.pdf\"}"
```

Arguments: `path` (absolute, on the mesh-llm machine) **or** `data_base64`
(a `data:application/pdf;base64,` prefix is fine), optional `filename`, and
`output`:

| `output` | Result |
| --- | --- |
| `auto` (default) | `text` if the document has ≤ `inline_max_chars` (40,000) characters, else `file` |
| `text` | `text`: the whole document, each page starting with `--- Page N ---` |
| `file` | `text_path` (the same text as a file), `pdf_path` (a copy of the original) and a `preview` |

`file` is for large documents: the agent greps or reads `text_path` with its
own tools instead of putting the whole document into the model's context.
Every result also has `filename`, `pages`, `chars` and `cached`.

Extractions are cached by the PDF's SHA-256 in `~/.mesh-llm/pdf` (owner-only)
and removed 24 hours after last use. Errors: `not_a_pdf`, `invalid_pdf`
(damaged or encrypted), `pdf_no_text` (blank, or OCR found nothing),
`document_too_large`, `document_timeout`, `document_extractor_unavailable`.

## Install

Needs Java 11+, the Tika server jar (`tika-server-standard-3.x.jar` from
https://tika.apache.org/download.html) and, for OCR, Tesseract
(`brew install tesseract`).

```sh
scripts/package.sh     # prints dist/pdf-0.1.0-<target>.tar.gz
mesh-llm plugins install --archive dist/pdf-0.1.0-<target>.tar.gz --name pdf
```

Then in `~/.mesh-llm/config.toml`:

```toml
[[plugin]]
name = "pdf"

[plugin.settings]
tika_jar = "/path/to/tika-server-standard-3.3.3.jar"
java = "/opt/homebrew/opt/openjdk/bin/java"   # default: java on PATH
# tika_url = "http://127.0.0.1:9998"          # instead of tika_jar
```

Other settings, with defaults: `inline_max_chars = 40000`, `ocr = true`,
`ocr_language = "eng"` (e.g. `"eng+deu"`), `tesseract_dir` (if `tesseract`
is not on PATH; `/opt/homebrew/bin` and `/usr/local/bin` are always tried),
`timeout_secs = 300`, `max_pdf_mb = 64`, `store_dir`, `ttl_hours = 24`,
`max_store_mb = 2048`, `tika_concurrency = 2`.

The plugin starts Tika on a random loopback port and stops it with itself.
It never fetches URLs.

## What was tested

On an M-series Mac, mesh-llm `main` fe87794b6, Tika 3.3.3, Tesseract via Homebrew:

- text PDF (3 pages) → `text`; 300-page PDF (1.2M chars) → `file`, second call cached;
- image-only scanned page → OCR text; mixed PDF (30 text pages + 1 scanned) → all 31 pages;
- blank, corrupt and non-PDF inputs → errors; `data_base64` and `path` input;
- the same tool over HTTP and over MCP (`pdf.extract`);
- goose 1.59 with mesh-llm `/mcp` as an extension: it called `pdf.extract` on both files,
  grepped `text_path` of the 300-page PDF for an item on page 287, and read the scanned memo.

## Gaps

- **The mesh console doesn't use it.** The console still extracts PDF text in the
  browser (pdf.js) and only describes scanned PDFs with a vision model. Using this
  tool there would be a UI change in mesh-llm.
- **PDFs attached to inference requests are not intercepted.** A base64 PDF inside
  `/v1/chat/completions` still goes to the model as-is; this plugin only works when
  the client calls the tool.
- **`path` reads any PDF the mesh-llm user can read**, for anyone who can reach the
  console's `/mcp` or HTTP API. That API is meant for this machine; don't expose it.
- **`file` paths only help callers on the same machine** (or with the same filesystem).
- OCR quality is Tesseract's; tables and layout are flattened to text.
- Heavy: Java + a ~300 MB jar + Tesseract on the node that runs it.
- PDF only (Tika can do Office formats too; not wired up). No CI yet.

## Build

```sh
cargo test && cargo clippy --all-targets -- -D warnings
```
