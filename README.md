# pdf-plugin (documents)

> **⚠️ DRAFT — experimental, not for production use.** It depends on an unmerged
> host hook ([Mesh-LLM/mesh-llm#3011](https://github.com/Mesh-LLM/mesh-llm/pull/3011),
> draft) and has only been tested on one dev machine. See [Gaps](#gaps).

A mesh-llm plugin for PDF attachments. The host hands it every PDF part in a
locally received request (`/v1/chat/completions`, `/v1/responses`, or an
Anthropic `document` block on `/v1/messages`); the plugin extracts the text
with Apache Tika and answers one of three ways:

| Request | What the model sees |
| --- | --- |
| Total text ≤ `inline_max_chars` (40,000) | The text inline, with `--- Page N ---` markers |
| Larger, the caller sent `tools`, and it is on this machine | A short note with two local paths: the extracted text and the original PDF, plus a preview |
| Larger, no `tools` | A reference to the `mesh-docs` reading model, which the request is routed to |
| Larger, `tools`, from another machine | `413 document_too_large` |

`mesh-docs` answers from the document without putting it all in one prompt.
With a tool-capable model it searches, reads page ranges and asks a helper
model about ranges (up to 12 steps, 8,000 characters per tool result, 240 s in
all); otherwise it sends the best-matching pages in one prompt and says so.
Every nested prompt stays within 60% of the model's context, counted in
tokens with the tool schemas, and leaves room for the answer.

A caller can choose with `x-mesh-documents: inline | pointer | rlm`.

Extracted documents are kept in the store (`~/.mesh-llm/documents`, owner-only)
for 24 hours after last use, so follow-ups and retries are not extracted
again. Scanned PDFs without a text layer get `422 pdf_no_text_layer`; there is
no OCR.

## Install

Needs Java 11+ and the Tika server jar
(`tika-server-standard-3.x.jar` from https://tika.apache.org/download.html),
or a Tika server already running on loopback.

```sh
scripts/package.sh            # prints dist/documents-0.1.0-<target>.tar.gz
mesh-llm plugins install --archive dist/documents-0.1.0-<target>.tar.gz --name documents
```

Then in `~/.mesh-llm/config.toml`:

```toml
[[plugin]]
name = "documents"

[plugin.settings]
tika_jar = "/path/to/tika-server-standard-3.3.3.jar"
java = "/opt/homebrew/opt/openjdk/bin/java"   # default: java on PATH
# tika_url = "http://127.0.0.1:9998"          # instead of tika_jar
```

Other settings, with their defaults: `inline_max_chars = 40000`,
`pointer = "auto"` (or `"header"`: only on request, `"off"`: never),
`store_dir`, `ttl_hours = 24`, `max_store_mb = 2048`, `max_total_pdf_mb = 64`,
`max_total_chars = 5000000`, `tika_concurrency = 2`, `root_model` and
`sub_model` (the models `mesh-docs` reads with; by default the caller's model,
else the best candidate).

`POST /api/plugins/documents/tools/status` on the console port reports
extractions, cache hits and what the last `mesh-docs` turn did.

## Build

Until the host hook lands, `Cargo.toml` pins `mesh-llm-plugin` to the PR
branch commit, and the plugin only works with a mesh-llm built from that PR.

```sh
cargo test && cargo clippy --all-targets -- -D warnings
```

## Gaps

- **Host hook not merged.** Needs mesh-llm#3011; stock mesh-llm ignores this plugin.
- **`mesh-docs` (RLM) is not validated with real models.** The search/read/ask
  loop has only run in unit tests against a scripted model. Live runs used the
  one-prompt fallback (best-matching pages in a single prompt).
- **Pointer mode defaults to `auto`.** Any tool-using caller on loopback gets
  local paths; loopback + tools does not prove the caller can read the files.
  `header` (explicit opt-in) may be the better default — undecided.
- **No OCR.** Scanned PDFs get `422 pdf_no_text_layer`.
- **Size.** Host chat bodies cap at 8 MiB (≈6 MB PDF inline, ≈11 MB via blob upload).
- **Model names.** A request using a public model name (`Qwen/…:q4_k_m`) is listed
  to the plugin under its internal id, so `mesh-docs` may pick a different model.
- **`/v1/responses` flat `file_data`** is rejected by the host translator; nested
  `input_file` works.
- **No client-disconnect signal** to plugins; an abandoned `mesh-docs` turn runs
  up to its 240 s budget.
- **Real agents don't send PDFs this way yet** (goose sends none; Claude Code's
  Anthropic `document` blocks need #3011). Tested with curl/scripts.
- **Heavy:** needs Java + the ~300 MB Tika jar on each node that receives PDFs.
- PDF only; no CI in this repo yet.
