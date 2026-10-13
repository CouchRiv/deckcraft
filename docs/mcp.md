# MCP server

> **Last updated:** 2026-10-13 · Ask Agent over the authenticated control channel.

`deckcraft-cli mcp` runs a Model Context Protocol server on stdio. By default it drives its own
headless engine (no window); with `--connect PORT` it drives a running DeckCraft started with
`--control PORT`, so an agent and a person can work on the same deck. `--sample` opens the sample
deck.

```json
{
  "mcpServers": {
    "deckcraft": { "command": "deckcraft-cli", "args": ["mcp", "--connect", "7990"] }
  }
}
```

## Tools

| Tool | What it does |
|---|---|
| `list_commands` | Every command (id, label, menu, shortcut, params doc), optionally filtered |
| `run_command` | Run one command: `{command, params}` — every menu item, ribbon button and gesture is a command |
| `batch` | Run several commands in order and stop at the first error |
| `new_presentation`, `open_presentation`, `save_presentation`, `export` | Files: native `.deckcraft`, PowerPoint `.pptx`, PDF (slides, notes pages, handouts), PNG/JPEG, outline |
| `inspect_document`, `inspect_slide` | Structure of the deck / one slide (shapes, text, placeholders, animations) |
| `render_slide` | A slide as an image the model can look at |
| `add_slide`, `go_to_slide` | Slides by layout |
| `add_shape`, `add_text_box`, `set_text` | Shapes and text |
| `insert_picture`, `insert_table`, `insert_chart` | Content |
| `select`, `pointer`, `select_tool`, `key`, `type_text` | The same selection, mouse and keyboard paths as the UI |
| `screenshot` | The whole window, UI included (connected mode) |
| `call_app` | Any [control protocol](control-protocol.md) method (connected mode) |
| `wait_for_request`, `reply_to_request` | Ask Agent, connected mode only: see [Ask Agent](#ask-agent) |

Useful commands for building decks: `slide.new {layout, title, body}`, `shape.insert {preset,
rect, text}`, `shape.connect {from, to, preset}`, `shape.freeform {points, closed, smooth}`,
`shape.merge {op, ids}`, `format.*`, `design.theme`, `transition.set`, `animation.set`,
`chart.*`, `table.*`, `file.export {path, layout}`. `deckcraft-cli describe ID` prints any
command's parameters.

## Ask Agent

With `--connect`, the person can hand work to the agent from the DeckCraft window: they select
shapes, text in a shape or nothing (the slide), click **Ask Agent** (the chip beside the
selection, Home › Ask Agent, the context menu, or Ctrl+Shift+O) and type an instruction. Two tools
carry it, for any MCP client:

| Tool | What it does |
|---|---|
| `wait_for_request` | waits up to `timeout_s` (default 30, at most 60) for the next request: `{id, instruction, selection, document}`, or `{none: true}` |
| `reply_to_request` | `{id, text, done?}`: shows `text` in the bubble next to the selection; `done: false` is a progress note that keeps the request open; a request the person cancelled answers `{cancelled: true}` |

`selection` and `document` are described in [Control protocol](control-protocol.md#ask-agent):
`selection.kind` is `text`, `shapes`, `slide` or `deck`, with the shapes as `inspect_slide` gives them.
The agent's loop: `wait_for_request`; at once `reply_to_request {id, text: "Rewriting…", done:
false}`, so the person sees the agent has it; the work with `run_command`, `select`,
`set_text`…; `reply_to_request` with the answer; `wait_for_request` again. In Claude Code, for
example: "wait for my DeckCraft requests and handle them". While `wait_for_request` waits this
server answers nothing else, which is why the wait is short. The chip only shows while an agent
listens, so the window never offers a button nobody answers. Without the app (`deckcraft-cli
mcp`), both tools say how to start it.

## Revision history

| Date | Change |
|---|---|
| 2026-10-13 | Ask Agent requests, replies, document identity and grouped Undo. |

### One Undo per request

The control server and connected MCP server add `request` to `engine.execute` automatically while handling the
request returned by `wait_for_request`, and releases it after a final `reply_to_request`.
Finish one request before waiting for the next. While a request is open, use engine commands
for edits; direct pointer/key/text/UI calls are refused because they bypass grouped Undo.
A request stays tied to its original presentation (`document.id`). If the person edits during
the request, further agent edits are refused: finish with an explanation and ask again.
Use `run_command app.askAgent {instruction, scope: "deck"}` to queue a whole-deck request.
