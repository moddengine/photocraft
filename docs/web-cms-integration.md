# CMS integration for the web editor

PhotoCraft works without configuration: Open uses the browser picker and Save / Save As /
Export download files. The optional API below replaces media selection and persistence only.
The editor still decodes and encodes files in Rust. Authentication, permissions, asset IDs,
query parameters and network requests belong to your host application.

The API is available as `window.PhotoCraft` in both the Trunk build and the packaged web build.
Install your host script before the generated loader, or check for `window.PhotoCraft` and listen
for `photocraft-api-ready`. This event means the API exists; **await `PhotoCraft.ready` before
using the editor**. A failed WebRunner startup rejects readiness. A wasm download failure
happens before any API exists; retain your own page-load timeout and the packaged loading hint.
The same functions are exported by the app's generated wasm-bindgen module (`ready()` returns
the readiness promise). Do not initialize a second copy of the wasm module.

## Direct API

```typescript
// Contract only: the editor and adapter implementations are Rust.
type EditorFile = { name: string; bytes: Uint8Array };
type HostIntegration = {
  open(): Promise<EditorFile | null>;
  close?(): void;
  save(file: EditorFile, action: "save" | "saveAs" | "export"): Promise<void>;
};
PhotoCraft.setHostIntegration(hooks: HostIntegration): void;
PhotoCraft.ready: Promise<void>;
PhotoCraft.loadDocument(file: EditorFile): Promise<void>;
PhotoCraft.connectParent(trustedParentOrigin: string): void;
```

Configure hooks once, before letting the user interact. Reload to change a configured host.
Passing `null` before configuring leaves standalone behavior in place. Both methods are required.
File names must be nonempty, contain no control characters and be at most 4096 UTF-8 bytes;
input must be a nonempty `Uint8Array` no larger than 256 MiB. Importers apply their usual format,
dimension and allocation limits after this boundary validation. Use a filename with its format
extension. Preset files are not documents.

- **Open** calls `open()` once. Returning `null` cancels without changing the document. A rejected
  promise reports a picker error. Other pickers, such as Import Brushes, retain their local behavior.
- **Exit** calls the optional `close()` hook after the editor's unsaved-work prompts and pending saves finish. Embedded hosts should close their editor dialog here; standalone browser tabs must be closed by the user.
- **Save** sends a `.pcraft` native editable document with action `save`, preserving supported
  layers, masks, effects and document data. Store it as the asset's editable source; its filename
  and MIME type may differ from the originally selected JPEG/PNG. PhotoCraft does not overwrite
  a layered document with a flattened rendition.
- **Save As** also sends `.pcraft`, with action `saveAs`. Create an asset and retain its identity
  for later Save. Menu actions and keyboard shortcuts share these paths.
- **Export**, including Quick Export and Save for Web, sends encoded files with action `export`.
  It never changes the document's saved revision or asset binding. Layer/slice exports can call
  the hook several times; each file needs its own acknowledgement.
- Resolve `save()` only after durable persistence. Throw/reject on upload failure or cancellation.
  Save remains pending until acknowledgement; failed saves retain unsaved changes. Edits made
  while uploading remain dirty after the captured revision is saved. Another Save waits for the
  previous one to finish. Iframe calls time out after 120 seconds; direct hooks own their timeout.
- `loadDocument()` waits for readiness and submits the supplied bytes once. It resolves after
  decoding and any profile/Camera Raw prompts finish; rejection leaves the editor usable.
  It refuses loading while there is unsaved work, a save or another opening/dialog in progress.
  Save or close that work and retry. It retains the previous clean document until the new file
  opens, then closes the old clean document. This keeps the CMS's single selected asset aligned
  with the editor. Cancelling an opening prompt rejects the load.

CMS mode expects one selected asset at a time. Do not change the current CMS identity until
initial `loadDocument()` succeeds. For a picker, retain a pending selection separately from the
current asset: `open()` supplies bytes but decoding can still fail or be cancelled. The save
hook receives the editable filename; use it to select the corresponding pending asset only
when that document is saved. Never make a failed picker/import retarget the existing document.
For workflows needing transactional picker binding, drive your own picker button with
`await PhotoCraft.loadDocument(file)` before committing the selected asset.

## Minimal CMS host example

This is host-side integration code, not an editor build dependency. `cms.pick`, `cms.read`,
`cms.replaceEditable`, `cms.createEditable`, and `cms.createRendition` stand for your authenticated
CMS SDK. They must enforce permissions and throw on failure. Keep pending selections by the
editable filename; production CMSs can use a host-generated unique filename per selection.

```javascript
async function integrate(cms) {
  const editor = window.PhotoCraft ?? await new Promise(resolve => {
    window.addEventListener("photocraft-api-ready", () => resolve(window.PhotoCraft), { once: true });
  });
  let currentAsset;
  const pending = new Map();
  async function editorFile(asset) {
    const file = await cms.read(asset); // { name, bytes: Uint8Array }
    // Use a stable, host-unique stem, retaining the original extension for decoding.
    const extension = file.name.split(".").pop();
    const stem = `cms-${crypto.randomUUID()}`;
    pending.set(`${stem}.pcraft`, asset);
    return { name: `${stem}.${extension}`, bytes: file.bytes };
  }
  editor.setHostIntegration({
    async open() {
      const asset = await cms.pick();
      return asset == null ? null : editorFile(asset);
    },
    async save(file, action) {
      const selected = pending.get(file.name);
      if (action === "save") {
        if (selected == null) throw new Error("This document has no CMS asset binding; use Save As");
        await cms.replaceEditable(selected, file);
        currentAsset = selected; // Commit identity only after persistence.
      } else if (action === "saveAs") {
        const created = await cms.createEditable(file);
        currentAsset = created;
        pending.set(file.name, created);
      } else {
        await cms.createRendition(currentAsset, file);
      }
    }
  });
  // URL parsing and authenticated fetching are deliberately outside PhotoCraft.
  const assetId = new URL(location.href).searchParams.get("assetId");
  if (assetId != null) {
    try {
      const file = await editorFile(assetId);
      await editor.ready;
      await editor.loadDocument(file); // Exactly one submission, after readiness.
      currentAsset = assetId;
    } catch (error) {
      showHostError(`Could not open the selected asset: ${error}. Choose another image or retry.`);
    }
  }
}
```

Do not use original CMS filenames as unique asset IDs. Two CMS assets may have the same name.
CMS identifiers remain entirely in the host's closure/map; the editor sees ordinary filenames.
For Export on a newly opened pending selection, use the host's selected asset or create a new
asset instead of a rendition if it has no committed identity yet.

## Iframe adapter

In the **editor page**, configure the built-in adapter once from trusted deployment configuration:

```javascript
window.addEventListener("photocraft-api-ready", () => {
  window.PhotoCraft.connectParent("https://cms.example");
}, { once: true });
```

This installs the same file hooks. It accepts messages only from `window.parent` with that exact
origin, and always replies with that explicit target origin. Do not take this origin from a URL
parameter, the first message received, `document.referrer`, or `"*"`. The adapter is absent unless
explicitly configured. Use HTTPS in production and a restrictive `frame-ancestors` policy.

Each envelope has `channel: "photocraft.files.v1"`, `kind: "request" | "response"` and a string `id`.
IDs are canonical positive integers through `9007199254740991`, strictly increasing for requests
from each side. Replies reuse the request ID. Duplicate/out-of-order requests, malformed IDs,
wrong channels, origins and sources are ignored. Unknown methods return an error. Responses
need a boolean `ok`, with `value` on success or `error` on failure. Unknown response IDs are ignored.
The adapter caps concurrent outgoing requests at 16 and rejects a request after 120 seconds.
Treat timed-out uploads as ambiguous: make CMS operations idempotent before retrying.

| Direction | Request `method` | Additional fields | Success `value` |
|---|---|---|---|
| parent → editor | `ready` | none | absent |
| parent → editor | `loadDocument` | `file: EditorFile` | absent |
| editor → parent | `open` | none | `EditorFile` or `null` |
| editor → parent | `save` | `file: EditorFile`, `action` | absent |

Register the parent listener **before** loading the iframe. Send `ready` after the editor page
has configured `connectParent` (for example, its `load` event, with a retry of the handshake
using a fresh ID if wasm is still downloading). Only after the successful readiness response
send `loadDocument`. The following parent helper includes that handshake retry. The `cmsHooks`
object has the same Open/Save contract above and retains all CMS identity and authentication.

```javascript
function connectPhotoCraft(frame, editorOrigin, cmsHooks) {
  if (new URL(editorOrigin).origin !== editorOrigin || editorOrigin === "null") throw new Error("Explicit editor origin required");
  const channel = "photocraft.files.v1", pending = new Map();
  let sequence = 0, lastRequest = 0, closed = false;
  function send(message) { frame.contentWindow.postMessage({ channel, ...message }, editorOrigin); }
  function validId(id) { return typeof id === "string" && /^[1-9][0-9]{0,15}$/.test(id) && Number.isSafeInteger(Number(id)); }
  function request(method, extra = {}, timeout = 120000) {
    if (closed) return Promise.reject(new Error("Editor disconnected"));
    const id = String(++sequence);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        pending.delete(id); const error = new Error(`${method} timed out`); error.name = "TimeoutError"; reject(error);
      }, timeout);
      pending.set(id, { resolve, reject, timer });
      try { send({ kind: "request", id, method, ...extra }); }
      catch (error) { pending.delete(id); clearTimeout(timer); reject(error); }
    });
  }
  async function receive(event) {
    if (event.origin !== editorOrigin || event.source !== frame.contentWindow) return;
    const m = event.data;
    if (!m || m.channel !== channel || !validId(m.id)) return;
    if (m.kind === "response") {
      const call = pending.get(m.id);
      if (!call || typeof m.ok !== "boolean") return;
      pending.delete(m.id); clearTimeout(call.timer);
      m.ok ? call.resolve(m.value) : call.reject(new Error(String(m.error)));
    } else if (m.kind === "request" && Number(m.id) > lastRequest) {
      lastRequest = Number(m.id);
      try {
        let value;
        if (m.method === "open") value = await cmsHooks.open();
        else if (m.method === "save" && ["save", "saveAs", "export"].includes(m.action)
                 && m.file && typeof m.file.name === "string" && m.file.bytes instanceof Uint8Array) {
          await cmsHooks.save(m.file, m.action);
        } else throw new Error("Invalid CMS request");
        send({ kind: "response", id: m.id, ok: true, value });
      } catch (error) { send({ kind: "response", id: m.id, ok: false, error: String(error) }); }
    }
  }
  window.addEventListener("message", receive);
  return {
    async ready() {
      for (let attempt = 0; attempt < 60; ++attempt) {
        try { return await request("ready", {}, 2000); }
        catch (error) { if (closed || error.name !== "TimeoutError" || attempt === 59) throw error; }
      }
    },
    loadDocument(file) { return request("loadDocument", { file }); },
    dispose() {
      closed = true;
      window.removeEventListener("message", receive);
      for (const call of pending.values()) { clearTimeout(call.timer); call.reject(new Error("Editor disconnected")); }
      pending.clear();
    }
  };
}
// const connection = connectPhotoCraft(frame, "https://editor.example", cmsHooks);
// await connection.ready();
// await connection.loadDocument(await cms.read(assetId));
```

Only retry the readiness handshake automatically. Do not retry loads or uploads automatically:
that can open twice or create duplicate assets. Structured cloning carries `Uint8Array` bytes;
no base64 encoding or transfer of authentication credentials is needed. The parent should discard
its connection on iframe navigation and create a new one for each editor instance.

## Validation

Run `cargo test -p photocraft-ui-egui host_files` for persistence, cancellation, initial loading,
failed decode and native-format layer round trips. Run `cargo test -p photocraft-web` for protocol
ID validation and existing web-shell tests. With the matching `wasm-bindgen-cli` installed, run
`CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner cargo test -p photocraft-web --target wasm32-unknown-unknown`
for readiness, initial-load queueing and JavaScript-hook tests in Node.
`cargo xtask wasm` checks the Rust browser boundary.
After `trunk build --release`, serve the result and verify browser Open, Save, Save As and Export
with no hooks, then with hooks that resolve, reject and cancel. For iframe deployments also test
wrong-origin/source messages, replayed IDs, malformed payloads and an upload timeout.
