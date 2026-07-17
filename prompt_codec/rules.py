"""Deterministic, free prompt compression (no model required)."""

from __future__ import annotations

import re
from typing import Iterable


_MULTI_BLANK = re.compile(r"\n{3,}")
_MULTI_SPACE = re.compile(r"[ \t]{2,}")
_BOILERPLATE = [
    re.compile(r"(?im)^\s*please[, ]+(i would like you to |help me with |remember to )?"),
    re.compile(r"(?im)^\s*please\s+"),
    re.compile(r"(?i)\bthank you( so much)?( in advance)?[.!]?\s*"),
    re.compile(r"(?i)\bas an ai[^.!\n]*[.!]?\s*"),
    re.compile(r"(?i)\bi hope this helps[^.!\n]*[.!]?\s*"),
    re.compile(r"(?im)^\s*i would like you to\s+"),
    re.compile(r"(?i)\bplease also:\s*"),
    re.compile(r"(?i)\b(write clean code|follow best practices|make it production ready)\b[^\n]*"),
    re.compile(r"(?i)\badd comments where needed\b[^\n]*"),
    re.compile(r"(?im)^\s*also:\s*$"),
    re.compile(r"(?im)^\s*[-*•]\s*$"),
]
# Common agent noise lines
_AGENT_NOISE = re.compile(
    r"(?im)^\s*([-*•]\s*)?(note:|important:|reminder:|ps:|p\.s\.:).{0,100}$"
)
_SOFT_FLUFF_LINES = re.compile(
    r"(?im)^\s*([-*•]\s*)?(please be careful|be careful|this is important)\s*$"
)
_CODE_FENCE = re.compile(r"```[\w+-]*\n([\s\S]*?)```")


def collapse_whitespace(text: str) -> str:
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    text = _MULTI_BLANK.sub("\n\n", text)
    lines = [_MULTI_SPACE.sub(" ", ln).rstrip() for ln in text.split("\n")]
    return "\n".join(lines).strip()


def drop_duplicate_lines(text: str) -> str:
    seen: set[str] = set()
    out: list[str] = []
    for ln in text.split("\n"):
        key = ln.strip()
        if not key:
            out.append(ln)
            continue
        if key in seen:
            continue
        seen.add(key)
        out.append(ln)
    return "\n".join(out)


def strip_boilerplate(text: str) -> str:
    for pat in _BOILERPLATE:
        text = pat.sub("", text)
    text = _AGENT_NOISE.sub("", text)
    text = _SOFT_FLUFF_LINES.sub("", text)
    return text


def compress_long_lists(text: str, max_items: int = 12) -> str:
    """If a block looks like a long bullet list, keep head + count of omitted."""
    lines = text.split("\n")
    out: list[str] = []
    i = 0
    while i < len(lines):
        ln = lines[i]
        if re.match(r"^\s*([-*•]|\d+\.)\s+", ln):
            block = [ln]
            j = i + 1
            while j < len(lines) and re.match(r"^\s*([-*•]|\d+\.)\s+", lines[j]):
                block.append(lines[j])
                j += 1
            if len(block) > max_items:
                kept = block[: max_items - 1]
                omitted = len(block) - len(kept)
                kept.append(f"  … (+{omitted} more items omitted for cost)")
                out.extend(kept)
            else:
                out.extend(block)
            i = j
            continue
        out.append(ln)
        i += 1
    return "\n".join(out)


def trim_repeated_code_fences(text: str) -> str:
    """If identical fenced code appears twice, keep first only."""
    seen: set[str] = set()
    def repl(m: re.Match[str]) -> str:
        body = m.group(1).strip()
        key = body[:500]
        if key in seen:
            return "```\n# [duplicate code block removed]\n```"
        seen.add(key)
        return m.group(0)
    return _CODE_FENCE.sub(repl, text)


def rules_compress(text: str) -> str:
    if not text or not text.strip():
        return text
    t = text
    t = strip_boilerplate(t)
    t = trim_repeated_code_fences(t)
    t = drop_duplicate_lines(t)
    t = compress_long_lists(t)
    t = collapse_whitespace(t)
    return t


def rules_compress_messages(
    messages: list[dict],
    roles: Iterable[str],
) -> list[dict]:
    role_set = set(roles)
    out: list[dict] = []
    for m in messages:
        nm = dict(m)
        role = nm.get("role", "")
        content = nm.get("content")
        if role in role_set and isinstance(content, str):
            nm["content"] = rules_compress(content)
        elif role in role_set and isinstance(content, list):
            new_parts = []
            for part in content:
                if isinstance(part, dict) and part.get("type") == "text":
                    p = dict(part)
                    p["text"] = rules_compress(str(p.get("text", "")))
                    new_parts.append(p)
                else:
                    new_parts.append(part)
            nm["content"] = new_parts
        out.append(nm)
    return out
