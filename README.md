# pdf-plugin

> **⚠️ DRAFT: experimental, not for production use.** It needs the draft
> mesh-llm hook [`request_transform.v1` (#3012)](https://github.com/Mesh-LLM/mesh-llm/pull/3012),
> which is not on `main` yet. It has been tested on one Mac. See [Gaps](#gaps).

This [mesh-llm](https://github.com/Mesh-LLM/mesh-llm) plugin reads PDFs that
arrive **inside inference requests** and turns them into text the model can
use. It uses [Apache Tika](https://tika.apache.org/), with OCR (Tesseract) for
scanned pages. Clients don't change anything: same base URL, same API.

## What it does

mesh-llm calls the plugin on the node the client sent the request to, before
routing. By then the host has already converted Responses and Messages bodies
to the Chat shape. Every PDF attachment is replaced with text:

| Request | The model sees |
| --- | --- |
| has `tools` (an agent) | the paths of the extracted text (`text.md`, with `--- Page N ---` markers) and of the original PDF, plus the full text if it is ≤ `inline_max_chars` (40,000), otherwise a preview, so the agent can grep or read the file |
| no `tools`, text ≤ 40,000 chars | the full text inline |
| no `tools`, larger | `400`: too large to inline (querying large documents, "RLM", is not built yet) |

Accepted PDF forms (base64 only):

- Chat Completions: `{"type":"file","file":{"file_data":"data:application/pdf;base64,…","filename":"a.pdf"}}`
- Responses: `{"type":"input_file","filename":"a.pdf","file_data":"data:application/pdf;base64,…"}`
- Anthropic Messages: `{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"…"}}`,
  including inside `tool_result` content (Claude Code's Read tool)

File ids are rejected with a clear error, since the mesh has no files API.
Extractions are cached by the PDF's SHA-256 in `~/.mesh-llm/pdf` (owner-only)
and removed 24 hours after last use.

It also keeps the `extract` tool (MCP `pdf.extract` on the console's `/mcp`,
and HTTP `POST /api/plugins/pdf/tools/extract`) for agents that have a PDF on
disk: arguments `path` or `data_base64`, and `output` = `auto` | `text` | `file`.

## Install

You need Java 11+, the Tika server jar (`tika-server-standard-3.x.jar` from
https://tika.apache.org/download.html), Tesseract for OCR
(`brew install tesseract`), and a mesh-llm built from #3012.

```sh
scripts/package.sh     # prints dist/pdf-0.1.0-<target>.tar.gz
mesh-llm plugins install --archive dist/pdf-0.1.0-<target>.tar.gz --name pdf
```

`~/.mesh-llm/config.toml`:

```toml
[[plugin]]
name = "pdf"

[plugin.settings]
tika_jar = "/path/to/tika-server-standard-3.3.3.jar"
java = "/opt/homebrew/opt/openjdk/bin/java"   # default: java on PATH
# tika_url = "http://127.0.0.1:9998"          # instead of tika_jar
```

Other settings, with their defaults: `inline_max_chars = 40000`, `ocr = true`,
`ocr_language = "eng"`, `tesseract_dir`, `timeout_secs = 300`,
`max_pdf_mb = 64`, `store_dir`, `ttl_hours = 24`, `max_store_mb = 2048`,
`tika_concurrency = 2`. The plugin starts Tika on a random loopback port. It
never fetches URLs.

## What was tested

Tested on an M-series Mac with mesh-llm #3012 (b3d776c3f), Qwen2.5-3B-Instruct
Q4_K_M, Tika 3.3.3 and Tesseract (Homebrew), using plain `curl` against `/v1`:

- A 3-page text PDF (code word on page 3) sent to `/v1/chat/completions`,
  `/v1/responses` and `/v1/messages`. All three answered `PELICAN`.
- An image-only scanned page sent to `/v1/messages`, and to `/v1/responses`
  with `stream: true`. It was OCR'd and the model answered from its text
  (revenue grew `12` percent).
- A 300-page PDF (464k chars) with no tools. It was rejected with `400` and the
  message above.
- The same 300-page PDF with tools on `/v1/responses`. The model got the paths
  and a preview, and called the `grep` tool.
- Claude Code 2.1.310 against the node (`ANTHROPIC_BASE_URL`). Its Read tool
  sent `small.pdf` as a `document` inside `tool_result`, and the model answered
  `PELICAN`.

## Gaps

- **Needs the unmerged hook** ([#3012](https://github.com/Mesh-LLM/mesh-llm/pull/3012)).
- **Large PDFs without tools are rejected.** A local RLM trial (Qwen2.5-14B
  on mesh, Monty REPL) was unreliable on a real 47-page report and doesn't
  fit this hook: it produces an answer, not a rewrite, and needs its own model
  calls. Picking the question's most relevant pages (BM25) and inlining them
  did better in the same trial; that is the likely next step instead.
- **Tika can outlive mesh-llm.** Once, after SIGTERM to mesh-llm, the Tika
  JVM was still running 5 s later and had to be killed.
- **Claude Code often sends PDFs as page images.** When its Read tool is
  given `pages` (at most 20 per call), Claude Code sends JPEG page images
  instead of a document. The 3B model asked for `pages` unprompted, including
  on the 300-page PDF. This plugin doesn't touch
  those. OCR'ing such images with Tika would be the next step.
- **No file ids or URLs:** only base64 PDFs are read.
- **The mesh console doesn't send raw PDFs.** It already extracts text in the
  browser with pdf.js.
- **`file` paths only help agents on the same machine** as the ingress node.
- The `extract` tool's `path` argument reads any PDF the mesh-llm user can
  read, for anyone who can reach the console API. Don't expose that API.
- OCR quality is Tesseract's, and layout and tables are flattened. It needs
  Java, a ~300 MB jar and Tesseract. PDF only. No CI.

## Build

```sh
cargo test && cargo clippy --all-targets -- -D warnings
```
