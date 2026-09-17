# Web UI

Lands in M5 (SPEC §11, §14).

One page at `http://127.0.0.1:8401/`: inbox (live over SSE), thread view,
compose with drag-and-drop attachments, and peers with pair/confirm buttons.

Constraints worth knowing before starting:

- No framework. Vanilla TypeScript, built with esbuild, assets embedded in the
  binary with `include_dir`.
- **Reading must work without JavaScript.** The message list is server-rendered
  with `askama`; JS enhances it. This is not an accessibility box-tick — it is
  what makes the UI usable when the daemon is up and something in the page is
  broken.
- Keyboard navigable, semantic HTML, `prefers-color-scheme`.
- `sender_kind` shows as a small badge: "human" or "agent".
