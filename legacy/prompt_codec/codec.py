"""Core coder/decoder agent."""

from __future__ import annotations

from copy import deepcopy
from dataclasses import dataclass, field
from typing import Any, Literal

from .config import AppConfig
from .local_llm import LocalLLM
from .rules import rules_compress, rules_compress_messages
from .tokens import TokenStats, count_messages_tokens, count_tokens


@dataclass
class CodecResult:
    text: str = ""
    messages: list[dict[str, Any]] = field(default_factory=list)
    stats: TokenStats | None = None
    mode_used: str = ""
    notes: list[str] = field(default_factory=list)

    def as_dict(self) -> dict[str, Any]:
        return {
            "text": self.text,
            "messages": self.messages,
            "stats": self.stats.as_dict() if self.stats else None,
            "mode_used": self.mode_used,
            "notes": self.notes,
        }


class PromptCodec:
    def __init__(self, cfg: AppConfig, llm: LocalLLM | None = None):
        self.cfg = cfg
        self.llm = llm or LocalLLM(cfg.local)

    def encode_text(
        self,
        text: str,
        mode: Literal["rules", "local", "hybrid"] | None = None,
    ) -> CodecResult:
        mode = mode or self.cfg.encoder.mode
        notes: list[str] = []
        before = count_tokens(text, self.cfg.stats.encoding)
        out = text
        used = mode

        if mode in ("rules", "hybrid") and self.cfg.encoder.rules_enabled:
            out = rules_compress(out)
            notes.append("rules_compress")

        if mode in ("local", "hybrid"):
            if len(out) < self.cfg.encoder.min_chars_to_compress:
                notes.append("skipped_local_short")
            else:
                try:
                    compressed = self.llm.encode_text(
                        out, target_ratio=self.cfg.encoder.target_ratio
                    )
                    if compressed and len(compressed.strip()) > 20:
                        # Guard: reject expansions
                        if count_tokens(compressed, self.cfg.stats.encoding) < before:
                            out = compressed.strip()
                            notes.append("local_encode")
                        else:
                            notes.append("local_rejected_no_savings")
                    else:
                        notes.append("local_empty_response")
                except Exception as e:
                    notes.append(f"local_failed:{e}")
                    if mode == "local":
                        used = "rules_fallback"
                        out = rules_compress(text) if self.cfg.encoder.rules_enabled else text

        after = count_tokens(out, self.cfg.stats.encoding)
        stats = TokenStats(
            before_tokens=before,
            after_tokens=after,
            encoding=self.cfg.stats.encoding,
            usd_per_mtok_input=self.cfg.stats.usd_per_mtok_input,
        )
        return CodecResult(text=out, stats=stats, mode_used=used, notes=notes)

    def decode_text(self, text: str) -> CodecResult:
        if not self.cfg.decoder.enabled or self.cfg.decoder.mode == "off":
            return CodecResult(text=text, mode_used="off", notes=["decoder_disabled"])
        before = count_tokens(text, self.cfg.stats.encoding)
        try:
            expanded = self.llm.decode_text(text)
            after = count_tokens(expanded, self.cfg.stats.encoding)
            return CodecResult(
                text=expanded,
                stats=TokenStats(
                    before_tokens=before,
                    after_tokens=after,
                    encoding=self.cfg.stats.encoding,
                    usd_per_mtok_input=self.cfg.stats.usd_per_mtok_input,
                ),
                mode_used="local",
                notes=["local_decode"],
            )
        except Exception as e:
            return CodecResult(
                text=text,
                mode_used="passthrough",
                notes=[f"decode_failed:{e}"],
            )

    def encode_messages(
        self,
        messages: list[dict[str, Any]],
        mode: Literal["rules", "local", "hybrid"] | None = None,
    ) -> CodecResult:
        mode = mode or self.cfg.encoder.mode
        notes: list[str] = []
        roles = set(self.cfg.encoder.roles)
        before = count_messages_tokens(messages, self.cfg.stats.encoding)
        out = deepcopy(messages)

        if mode in ("rules", "hybrid") and self.cfg.encoder.rules_enabled:
            out = rules_compress_messages(out, roles)
            notes.append("rules_compress_messages")

        if mode in ("local", "hybrid"):
            try:
                for i, m in enumerate(out):
                    role = m.get("role", "")
                    if role not in roles:
                        continue
                    content = m.get("content")
                    if not isinstance(content, str):
                        continue
                    if len(content) < self.cfg.encoder.min_chars_to_compress:
                        continue
                    if (
                        role == "system"
                        and len(content) < self.cfg.encoder.protect_system_under_chars
                    ):
                        notes.append(f"protect_system_msg_{i}")
                        continue
                    compressed = self.llm.encode_text(
                        content, target_ratio=self.cfg.encoder.target_ratio
                    )
                    if (
                        compressed
                        and count_tokens(compressed, self.cfg.stats.encoding)
                        < count_tokens(content, self.cfg.stats.encoding)
                    ):
                        out[i] = {**m, "content": compressed.strip()}
                        notes.append(f"local_encode_msg_{i}")
                    else:
                        notes.append(f"local_keep_msg_{i}")
            except Exception as e:
                notes.append(f"local_failed:{e}")
                if mode == "local" and self.cfg.encoder.rules_enabled:
                    out = rules_compress_messages(messages, roles)
                    notes.append("rules_fallback")

        after = count_messages_tokens(out, self.cfg.stats.encoding)
        stats = TokenStats(
            before_tokens=before,
            after_tokens=after,
            encoding=self.cfg.stats.encoding,
            usd_per_mtok_input=self.cfg.stats.usd_per_mtok_input,
        )
        return CodecResult(messages=out, stats=stats, mode_used=mode, notes=notes)
