#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The Quire contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Render the Qwen 3.5 template fixtures of the AI engine.

    python3 packaging/qwen35-template-fixtures.py <chat_template.jinja>

Reads apps/quire-ai-engine/tests/fixtures/qwen35-template/conversations.json
(Quire's message form), turns each conversation into the form
transformers' `apply_chat_template` takes, renders it with the given
template text and writes `<name>.txt` next to it, plus `template.sha256`.
The engine's `render_qwen35` must give the same bytes (its test
`renders_qwen35_like_the_published_template`).

The template is Qwen's `chat_template.jinja` for Qwen 3.5 4B or 9B. The
environment is transformers' (`utils/chat_template_utils.py`): a sandboxed
Jinja with trim_blocks and lstrip_blocks, loop controls, `tojson` as
`json.dumps(x, ensure_ascii=False)` and `raise_exception`. With transformers
installed, `--transformers <model dir>` renders through
`AutoTokenizer.apply_chat_template` instead.
"""

import hashlib
import json
import sys
from pathlib import Path

DIR = Path(__file__).resolve().parent.parent / "apps" / "quire-ai-engine" / "tests" / "fixtures" / "qwen35-template"
JSON_RULE = "Reply with one JSON value only: no prose, no code fence."


def text_of(m):
    return "".join(p["text"] for p in m.get("parts", []) if p.get("type") == "text")


def to_template_form(conv):
    """Quire's (system, messages, tools, json) as transformers' messages and tools."""
    sys_parts = [conv.get("system", "").strip()] + [text_of(m) for m in conv["messages"] if m["role"] == "system"]
    system = "\n\n".join(p for p in sys_parts if p)
    if conv.get("json"):
        system = system + "\n\n" + JSON_RULE if system else JSON_RULE
    messages = [{"role": "system", "content": system}] if system else []
    for m in conv["messages"]:
        role = m["role"]
        if role == "system":
            continue
        if role == "tool":
            for p in m["parts"]:
                if p["type"] == "tool_result":
                    content = ("Error: " if p.get("is_error") else "") + p["content"]
                elif p["type"] == "text":
                    content = p["text"]
                else:
                    continue
                messages.append({"role": "tool", "content": content})
        elif role == "assistant":
            msg = {"role": "assistant", "content": text_of(m)}
            calls = [{"type": "function", "function": {"name": p["name"], "arguments": p["input"]}} for p in m["parts"] if p["type"] == "tool_use"]
            if calls:
                msg["tool_calls"] = calls
            messages.append(msg)
        else:
            messages.append({"role": "user", "content": text_of(m)})
    tools = [{"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["input_schema"]}} for t in conv.get("tools", [])]
    return messages, tools or None


def jinja_renderer(template):
    import jinja2
    import jinja2.ext
    from jinja2.sandbox import ImmutableSandboxedEnvironment

    def raise_exception(message):
        raise jinja2.exceptions.TemplateError(message)

    def tojson(x, ensure_ascii=False, indent=None, separators=None, sort_keys=False):
        return json.dumps(x, ensure_ascii=ensure_ascii, indent=indent, separators=separators, sort_keys=sort_keys)

    env = ImmutableSandboxedEnvironment(trim_blocks=True, lstrip_blocks=True, extensions=[jinja2.ext.loopcontrols])
    env.filters["tojson"] = tojson
    env.globals["raise_exception"] = raise_exception
    compiled = env.from_string(template)
    return lambda messages, tools: compiled.render(messages=messages, tools=tools, add_generation_prompt=True, enable_thinking=False)


def main():
    args = sys.argv[1:]
    if not args:
        sys.exit(__doc__)
    if args[0] == "--transformers":
        from transformers import AutoTokenizer

        tok = AutoTokenizer.from_pretrained(args[1])
        render = lambda messages, tools: tok.apply_chat_template(messages, tools=tools, tokenize=False, add_generation_prompt=True, enable_thinking=False)
        digest = hashlib.sha256(tok.chat_template.encode()).hexdigest()
    else:
        template = Path(args[0]).read_text()
        render = jinja_renderer(template)
        digest = hashlib.sha256(template.encode()).hexdigest()
    convs = json.loads((DIR / "conversations.json").read_text())
    for conv in convs:
        messages, tools = to_template_form(conv)
        (DIR / f"{conv['name']}.txt").write_text(render(messages, tools))
    (DIR / "template.sha256").write_text(digest + "\n")
    print(f"wrote {len(convs)} fixtures; template sha256 {digest}")


if __name__ == "__main__":
    main()
