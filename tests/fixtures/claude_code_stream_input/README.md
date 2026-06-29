# claude_code stream-json input fixtures

`SDKUserMessage` JSON payloads piped into `claude --input-format stream-json` for end-to-end
verification of the multimodal bridge path. Each `*.json` is a single-line payload ready to be
sent on stdin. Each `*.png` / `*.pdf` is the source binary that was base64-inlined into the
matching JSON.

## Why

`src/bridge/builtin/claude_code.rs` writes these payloads when `AGENT_IMAGE_URL` / `AGENT_FILE_URL`
are set. Unit tests cover the JSON shape, but they cannot exercise the real CLI. Use these
fixtures to manually verify a CLI version still accepts the protocol before shipping a release.

## Files

| File | Content block | Source bytes |
| --- | --- | --- |
| `image_stone.json` | `[text, image]` — 128×128 PNG, "What color?" | `stone.png` (695 B) |
| `pdf.json`         | `[text, document]` — minimal PDF, "In one short sentence, what does this PDF say?" | `sample.pdf` (612 B) |

## Reproduce

```bash
# Image — expects "Grayscale"
(cat image_stone.json; sleep 30) | timeout 60 claude \
  --input-format stream-json --output-format stream-json --verbose \
  --model MiniMax-M3 --dangerously-skip-permissions \
  --disallowed-tools AskUserQuestion -p

# PDF — expects "这是一个测试 PDF，页面 1 的内容是 ..."
(cat pdf.json; sleep 30) | timeout 60 claude \
  --input-format stream-json --output-format stream-json --verbose \
  --model MiniMax-M3 --dangerously-skip-permissions \
  --disallowed-tools AskUserQuestion -p
```

Notes:

- The trailing `sleep 30` keeps stdin open; without it the CLI receives EOF before processing
  the input and exits with no model call.
- `--model MiniMax-M3` pins the model. Without it, the local account's default routing may pick a
  model that returns `<synthetic>` + `400 invalid params (2013)` for these payloads.
- Last verified: Claude Code `2.1.177`, model `MiniMax-M3`, 2026-06-18.

## Regenerate

```bash
# stone.png — any 128x128 grayscale-ish PNG will do; this one came from macOS Solid Colors.
sips -s format png "/System/Library/Desktop Pictures/Solid Colors/Stone.png" --out stone.png

# sample.pdf — minimal hand-written PDF with "Hello from test PDF page 1." on page 1.
# See the original 612-byte file or rebuild via any PDF library.

# Embed both into SDKUserMessage JSON:
python3 - <<'PY'
import base64, json
for src, kind, prompt, media in [
    ("stone.png",  "image",    "Is this image grayscale (black/white/gray) or colored? One word.", "image/png"),
    ("sample.pdf", "document", "In one short sentence, what does this PDF say?",                 "application/pdf"),
]:
    data = base64.b64encode(open(src,"rb").read()).decode()
    msg = {
        "type": "user",
        "message": {"role":"user","content":[
            {"type":"text","text":prompt},
            {"type":kind,"source":{"type":"base64","media_type":media,"data":data}},
        ]},
        "parent_tool_use_id": None,
        "session_id": "",
    }
    out = {"image":"image_stone.json","document":"pdf.json"}[kind]
    json.dump(msg, open(out,"w"))
PY
```