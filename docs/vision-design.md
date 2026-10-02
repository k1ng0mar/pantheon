# Vision pipeline design

Status: analysis only (2026-09-30, branch `chloe/tui-redesign`). No code changed.
Question answered: do image pixels attached to a chat message ever reach a
provider vision path? **No.** The pipeline breaks in three places after the
bytes are safely stored. This doc describes the current state, the target
architecture, and a sequenced build plan.

## 1. Current state - hop by hop

### Hop 1: upload -> disk - WORKS

`POST /api/uploads` (`crates/pantheon-dashboard/src/uploads.rs:149-195`):

- base64 body decoded and written to
  `<data_dir>/uploads/upl_<epochms>_<nnnn>_<sanitized-name>` (`uploads.rs:165`);
- sidecar `<id>.json` holds `{name, mime, size_bytes}` (`uploads.rs:171-185`);
- 25 MiB cap (`MAX_UPLOAD_BYTES`, `uploads.rs:29`), filename sanitized against
  traversal (`uploads.rs:107-136`), mime re-validated on download
  (`uploads.rs:242-252`).
- Unit-tested: create/resolve/download round trip in `uploads.rs` tests.

### Hop 2: message -> `[attachments]` block - WORKS (text only)

`send_message` (`crates/pantheon-dashboard/src/runs.rs:502-544`) resolves each
`upl_` id to an absolute on-disk path (`uploads::resolve`,
`uploads.rs:211-240`) and appends:

```
[attachments]
- name (mime, size, id: upl_xxx): /absolute/path
The agent can read these files with its file tools; pass image paths to the vision tool when one is configured.
```

The block rides the queue/steer paths too (built before the busy branching,
`runs.rs:497-501`). The block construction itself has **zero tests**
(`runs.rs` contains no attachment test).

### Hop 3: block -> vision tool - BROKEN

The block promises a "vision tool" the agent can pass image paths to.
**No such tool exists.**

- Zero hits for `"vision"`, `vision_tool`, or `fn vision` in
  `crates/pantheon-runtime`, `crates/pantheon-tools`, `crates/pantheon-agent`.
- `ToolGroup::Vision` exists as an enablement flag
  (`crates/pantheon-api/src/config.rs:985,1006,1033,1107,1137`;
  `crates/pantheon-runtime/src/tool_config.rs:241,263,287,309,331`),
  but unlike `ToolGroup::Voice` (consulted in
  `crates/pantheon-gateway/src/voice.rs:220` by `VoiceEdge::from_config`), **no
  registration site consults `tools.vision`**. The flag is dead.
- `Session::build_tool_registry()` (`crates/pantheon-runtime/src/session.rs:1668`)
  and `register_turn_tools()` (`session.rs:1977`) have no vision branch.
- The agent's only recourse today is its file tools, which return raw bytes.
  A model reading a JPEG through `read`/`cat` gets binary garbage, not pixels.

### Hop 4: `[vision]` aux slot -> provider call - BROKEN (no call sites)

The config surface exists and resolves:

- `AuxSlot { kind: Vision, name: "vision", env_prefix: "VISION", ...,
  auto: true }` (`crates/pantheon-api/src/config.rs:2044-2051`);
- the TUI's `auxiliaries()` resolves it into `ModelPolicy.auxiliary`,
  gated on the (dead) Vision tool-group toggle
  (`crates/pantheon-tui/src/config.rs:812-850`).

But the codebase documents its own gap - `crates/pantheon-api/src/model.rs:50-55`:

> Vision model (config `[vision]`): describes and answers about images
> attached to a turn. Host-orchestrated; never chat. Absent = `auto`: the
> run's default model. **No call sites yet - there is no image-input
> pipeline; the slot exists so a model can be pinned ahead of it landing.**

The only runtime consumers of `policy.auxiliary(...)` are `TitleGen`
(`session.rs:2862`) and `Compression` (`session.rs:2965`). The providers
crate's own test asserts vision aux is absent:
`crates/pantheon-providers/src/lib_tests.rs:42`.

### Hop 5: provider wire format - BROKEN (text-only)

- `pantheon_api::message::Message { content: String }`
  (`crates/pantheon-api/src/message.rs:12-14`).
- `openai::body_value` emits `"content": "<text>"`
  (`crates/pantheon-providers/src/openai.rs:34`); the Anthropic adapter is
  likewise string-content. Zero `image` hits anywhere in
  `crates/pantheon-providers/src/`.
- The aux plumbing is text-only end to end: `aux_request`
  (`crates/pantheon-providers/src/http.rs:313`) builds `Message::user(prompt)`
  and routes through both adapters.
- The catalog's per-model `vision: bool`
  (`crates/pantheon-providers/src/catalog.rs:105`, rows in
  `crates/pantheon-providers/catalog.yaml`) is **metadata only** - nothing
  branches on it for request building. Conservative default: unknown models
  get `vision: false` (`catalog.rs:235-239`).

### Verdict on fixture-vs-wired

- Real and tested: upload storage (`uploads.rs`).
- Real but untested: the `[attachments]` text block (`runs.rs:531-544`).
- Real but inert: the `[vision]` config slot and `AuxSlot` (resolves into
  `ModelPolicy`, never consulted).
- Fabricated by prose: the "vision tool" the attachment block tells the
  agent about; the catalog `vision: true` flags implying image support.

Net: **image pixels reach the disk and stop.** The model only ever receives
the file path as text.

## 2. Target architecture

Follow the enum doc's stated intent: **host-orchestrated; never chat.**
Two cooperating pieces:

### A. Host-orchestrated vision pass (primary path)

When a turn's user message carries an `[attachments]` block whose lines name
`image/*` mimes, the host (runtime, not the agent):

1. Parses the block for `(mime, absolute path)` pairs - reuse the exact format
   `runs.rs:537-542` emits.
2. For each image: validates the path is inside `<data_dir>/uploads`
   (never trust a path the model wrote; the dashboard canonicalizes at
   `uploads.rs:231`, re-check on read), downscales to a provider-safe size,
   base64-encodes.
3. Calls the vision aux model (`policy.auxiliary(&AuxiliaryKind::Vision)`,
   mirroring `TitleGenClient` in
   `crates/pantheon-providers/src/title.rs`) with a "describe this image /
   answer the user's question about it" prompt + the image part.
4. Injects the returned description into the transcript as data with an
   untrusted provenance envelope (same framing as `openai.rs:25-33`), e.g.
   `[vision: /path - description]`, before the main turn runs.

Why host-orchestrated instead of an agent tool: deterministic (no wasted
agent turns negotiating which tool reads the pixels), no prompt-injection
surface from the agent deciding *whether* to look, and the description
lands in the transcript with proper provenance marking. The attachment
block's current promise ("pass image paths to the vision tool") gets
replaced by a statement of what actually happens.

### B. On-demand `vision` tool (escape hatch)

Register a real `vision` tool in `Session::build_tool_registry()`
(`crates/pantheon-runtime/src/session.rs:1668`), gated by the now-live
`tools.vision` flag, for images the agent encounters *mid-turn* (e.g. a
screenshot the browser tool saved, an image found via file tools). Input:
`{ "path": "/abs/path", "question": "..." }`. Implementation calls the
same `VisionClient` as the host path (piece A), keeping one provider seam.
Output returns as tool-result text with untrusted provenance, like any
fetched data.

## 3. Message/content types

Additive change - `Message` is constructed via constructors
(`Message::user/system/assistant`, `message.rs:48-84`) with only one raw
struct literal in the tree (a test, `pantheon-exec/src/context_tests.rs:9`),
so a new field is low-blast-radius:

```rust
// crates/pantheon-api/src/message.rs
pub struct ImagePart {
    pub mime: String,      // "image/jpeg" etc., validated against image/*
    pub data_b64: String,  // base64 bytes, already downscaled by the caller
}

pub struct Message {
    pub content: String,   // unchanged
    pub images: Vec<ImagePart>, // NEW, default empty
    ...
}
```

Constructors initialize `images: Vec::new()`; add `Message::user_with_images`
(or a builder) for the vision path. `body_value` in each adapter checks
`m.images.is_empty()`: empty -> today's string `"content"` (byte-identical
wire output, zero regression risk); non-empty -> multipart content (see
provider notes below).

Deliberately NOT a `ContentBlock` enum: that would touch every construction
and read site across crates. The additive field keeps today's hot path
untouched.

Downscale policy (client-side, before base64): cap longest edge at 1568 px,
re-encode to JPEG quality ~85 unless the source is PNG with transparency
worth keeping. Rationale: Anthropic caps images at 5 MB each / 100 MB per
request; OpenAI caps ~20 MB per image; both charge per image token, so
unbounded phone photos are a cost bug. This needs an image crate
(`image` with jpeg/png features) in `pantheon-providers` or a small
`pantheon-vision` helper - decide at build time; the resize must happen in
one place both piece A and piece B call.

## 4. Provider-by-provider notes

The catalog (`crates/pantheon-providers/catalog.yaml`) has two wire modes;
image support rides the existing `api_mode` dispatch in `aux_request`
(`http.rs:313-341`), so **only two adapters need the multipart branch**:

- **`api_mode: openai`** (every cataloged provider except Anthropic
  openai, google, groq, mistral, xai, deepseek, qwen, openrouter, together,
  fireworks, azure, bedrock, vertex, huggingface, local/lmstudio, and the
  long tail): OpenAI chat-completions image format
  ```json
  "content": [
    {"type": "text", "text": "<prompt>"},
    {"type": "image_url", "image_url": {"url": "data:<mime>;base64,<data>"}}
  ]
  ```
  Nearly all OpenAI-compatible endpoints accept this shape; vision-capable
  models are marked `vision: true` in the catalog. Caveat: some local
  servers (llama.cpp, older vLLM) accept the shape but ignore images
  nothing to do in code, but the vision call should surface "model
  returned no image-grounded answer" distinctly from a transport error.
- **`api_mode: anthropic`** (provider id `anthropic`): Messages API format
  ```json
  "content": [
    {"type": "text", "text": "<prompt>"},
    {"type": "image", "source": {"type": "base64", "media_type": "<mime>", "data": "<data>"}}
  ]
  ```
  Anthropic additionally accepts `source.type: "url"`, but Pantheon stores
  bytes locally with no public URL, so base64 is the only option.

Model selection for the aux call: when the `[vision]` slot is `auto`
(TUI `auto()` pins the run's default model, `tui/src/config.rs:826`), the
host MUST check the catalog `vision` flag for the resolved
(provider, model) pair before sending images. Sending image parts to a
text-only model wastes a full request (400 from strict providers, silent
ignore from lax ones). Fail closed: if `vision == false` and the user
pinned nothing, return a clear `VISION_NO_CAPABLE_MODEL` error naming the
remedy (`pantheon model set vision ...` / `[vision]` section), not a
provider 400.

## 5. Config surface

Already exists and stays: the `[vision]` aux slot
(`api/src/config.rs:2044-2051`, env `VISION_PROVIDER`/`VISION_MODEL`,
vault `PANTHEON_VISION_API_KEY`, `auto: true`). New knobs, all optional:

- The `[tools]` `vision` toggle (exists, currently dead
  `api/src/config.rs:1107`) becomes the master switch for both piece A
  (host pass) and piece B (tool registration), mirroring how the TUI
  already gates the aux entry (`tui/src/config.rs:830-843`).
- Optional `[vision]` additions (defaults sane, document in
  `docs/user-guide`): `max_image_px = 1568` (downscale cap),
  `max_images_per_turn = 4` (cost guard; extras get "skipped N images"
  note). Keep it to these two - no new sections.

## 6. Sequenced build plan

1. **Content model** (`pantheon-api`): add `ImagePart` + `Message.images`
   (`message.rs`), constructor coverage, unit tests that `images` defaults
   empty and serializes round-trip. No other crate changes needed
   additive field.
2. **Adapter branches** (`pantheon-providers`): multipart `content` in
   `openai::body_value` (`openai.rs:22-60`) and the Anthropic equivalent;
   fixture tests with a golden JSON body for each mode. Verify byte-identical
   output when `images` is empty (existing adapter tests cover this).
3. **Vision client** (`pantheon-providers/src/vision.rs`, new): `VisionClient`
   mirroring `TitleGenClient` (`title.rs`) - resolve aux wire via
   `resolve_aux_wire`, catalog `vision`-flag check with fail-closed error,
   timeout ~60 s, hard output bound. Downscale+encode helper lives here or
   beside it; unit-test the resize math and the fail-closed path.
4. **Host pass** (`pantheon-runtime`): at turn start in `session.rs`, parse
   the `[attachments]` block for `image/*` lines, call `VisionClient` per
   image, inject `[vision: ...]` data blocks with untrusted provenance
   before the agent loop runs. Gate on `tools.vision`.
5. **Tool registration** (`pantheon-runtime`): register the `vision` tool
   in `build_tool_registry()` (`session.rs:1668`), same `tools.vision`
   gate, delegating to the piece-3 client. Schema: `{path, question}`.
6. **Truthful block copy** (`pantheon-dashboard`): rewrite the attachment
   trailer in `runs.rs:543` to describe what actually happens
   (host describes attached images; `vision` tool for mid-turn images) and
   add the missing unit tests for block construction.
7. **Live verification** (not fixture): one real image through
   `POST /api/uploads` -> `send_message` -> turn, against one
   `api_mode: openai` provider and Anthropic, confirming pixels in the
   request body and a grounded description back. Fixture tests cover the
   JSON shape; only a live call proves the pixels flow.

## 7. What was NOT determined

- Whether the TUI/desktop clients render `[attachments]` blocks or rely on
  the server text (only the dashboard HTTP path was traced).
- The exact Anthropic adapter function name/line for its `body_value`
  equivalent (confirmed text-only via zero `image` hits; the precise edit
  site needs a read at build time).
- Which image-codec dependency the tree prefers (`image` crate vs.
  shelling to an existing tool) - no image decoding exists anywhere in
  the tree today, so this is a new dependency decision.
- `docs/user-guide/providers.md` documents per-provider key env vars
  referenced by the catalog; the new `[vision]` knobs should be
  documented alongside the other aux slots wherever they live.
