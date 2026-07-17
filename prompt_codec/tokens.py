"""Token counting + rough USD estimates."""

from __future__ import annotations

from dataclasses import dataclass
from functools import lru_cache
from typing import Any


@lru_cache(maxsize=4)
def _encoding(name: str):
    try:
        import tiktoken

        return tiktoken.get_encoding(name)
    except Exception:
        return None


def count_tokens(text: str, encoding_name: str = "cl100k_base") -> int:
    if not text:
        return 0
    enc = _encoding(encoding_name)
    if enc is not None:
        return len(enc.encode(text))
    # fallback ~4 chars/token
    return max(1, len(text) // 4)


def messages_text(messages: list[dict[str, Any]]) -> str:
    parts: list[str] = []
    for m in messages:
        role = m.get("role", "")
        content = m.get("content", "")
        if isinstance(content, list):
            # multimodal: join text parts
            chunks = []
            for part in content:
                if isinstance(part, dict) and part.get("type") == "text":
                    chunks.append(str(part.get("text", "")))
                elif isinstance(part, str):
                    chunks.append(part)
            content = "\n".join(chunks)
        parts.append(f"{role}: {content}")
    return "\n".join(parts)


def count_messages_tokens(
    messages: list[dict[str, Any]], encoding_name: str = "cl100k_base"
) -> int:
    # +4 per message for role framing (rough OpenAI-style overhead)
    total = 0
    for m in messages:
        content = m.get("content", "")
        if isinstance(content, list):
            text = " ".join(
                str(p.get("text", "")) if isinstance(p, dict) else str(p)
                for p in content
            )
        else:
            text = str(content or "")
        total += count_tokens(text, encoding_name) + 4
    return total + 2


@dataclass
class TokenStats:
    before_tokens: int
    after_tokens: int
    encoding: str = "cl100k_base"
    usd_per_mtok_input: float = 3.0

    @property
    def saved_tokens(self) -> int:
        return max(0, self.before_tokens - self.after_tokens)

    @property
    def ratio(self) -> float:
        if self.before_tokens <= 0:
            return 1.0
        return self.after_tokens / self.before_tokens

    @property
    def pct_saved(self) -> float:
        return (1.0 - self.ratio) * 100.0

    @property
    def usd_saved(self) -> float:
        return (self.saved_tokens / 1_000_000.0) * self.usd_per_mtok_input

    def as_dict(self) -> dict[str, Any]:
        return {
            "before_tokens": self.before_tokens,
            "after_tokens": self.after_tokens,
            "saved_tokens": self.saved_tokens,
            "ratio": round(self.ratio, 4),
            "pct_saved": round(self.pct_saved, 2),
            "usd_saved_est": round(self.usd_saved, 6),
        }
