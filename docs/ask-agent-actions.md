# Ask Agent actions

The action catalog is presentation data: `{id, label, menu, scope, instruction, apply}`.
Scope is `shape`, `text`, `slide` or `deck`; every prompt names existing engine commands and
asks the external agent to inspect identities and preserve unrelated content. DeckCraft does
not call a model or an image service.

Actions cover text, slide and deck design, externally supplied images, animations, transitions
and deck structure. Text prompts preserve selected ranges and rich formatting where possible;
whole-shape `text.set` keeps only its first run's formatting. Masters are inspected before
applying theme, color and font changes. Consistency findings are added as comments.

Every request uses the attribution and grouped Undo contract from the base Ask Agent branch.
A final answer does not auto-accept a visual result. Keep / Undo decide only this request's
changes. Undo must refuse if the person or another request edited after it, or if its history
entry is no longer the latest entry. Closing or Escape never invokes Undo.

## Before/after preview design

Designed, not implemented in this stage. Render the immutable before/after document snapshots
at fixed thumbnail resolution on a background worker after a request finishes, never per
frame. Show Original / Result side by side above Keep / Undo. A slide action uses its original
slide id; deck actions show the changed slides with pagination. Include notes and animation /
transition summaries because a still image cannot show them. Cache by request and snapshot
identity and release thumbnails when a request is dismissed, evicted or its document closes.
If the document changes during review, Keep remains available; Undo refuses safely rather than
reverting unrelated work. Engine changes remain immediately visible in the ordinary canvas.

## Image transport

The external agent supplies image bytes as base64 to `insert.picture {data, name, rect}` or
`picture.change {data, name, ids}`; `design.background {picture}` sets a background image.
Images are limited to **16 MiB decoded**, at most 22,369,624 base64 bytes. The limit is checked before decoding and again after it; file reads are bounded too. A control JSON line is limited to **24 MiB**, including its newline. Oversized images report a 16 MiB error; oversized lines report a 24 MiB error and close that connection. Send separate commands for several large images; they still form one Undo step for the request. The engine does not impose this picture-specific byte limit on audio and video; the control-frame limit applies to every message, so use file paths for larger local media.
Never pass a URL as if it were image bytes. Confirm provenance and license before inserting.

## Interface and protocol

`app.askAgent {action: "design.redesignSlide"}` queues an action; the bubble offers Rewrite,
Text, Translate, Design, Images, Animations and Deck menus, enabling actions for their context.
The request extends `{id, instruction, selection, document}` with `action: {id, label, scope}`
and `apply`. Typed requests carry `action: null, apply: "auto"`. `ui.inspect.askAgent.actions`
exposes the full catalog, including prompts. `replyOnly` requests refuse mutating commands.

`app.askAgent {keep: id}` records the person's review without changing the deck;
`{undo: id}` undoes exactly the request's single history entry, or refuses safely if other work
followed it. Both require a finished or cancelled request. Regular Undo/Redo remain available.
Keep / Undo appear in both the result bubble and request card when that request has edits.
Dismiss and Escape only hide results; they do not select a review outcome.
