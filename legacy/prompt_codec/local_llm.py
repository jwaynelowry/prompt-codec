"""Local OpenAI-compatible LLM client for encode/decode."""

from __future__ import annotations

from typing import Any

import httpx

from .config import LocalConfig


ENCODE_SYSTEM = """You are a PROMPT COMPRESSOR for paid LLM APIs.
Your job: rewrite the user's message so a strong cloud model still does the task correctly,
but with far fewer tokens.

Hard rules:
1. Preserve: goals, constraints, file paths, function/class names, error text, exact quotes, IDs, URLs, numbers, acceptance criteria.
2. Remove: fluff, politeness, repetition, obvious commentary, restated instructions, markdown decoration that adds no info.
3. Prefer: short imperative bullets, dense technical English, tables only if denser than prose.
4. Do NOT answer the task. Only output the compressed prompt text.
5. Do NOT invent requirements. If unsure, keep the original phrase.
6. Keep code blocks if they are necessary evidence; otherwise summarize with path + signature + 1-line intent.
7. Target roughly the requested compression ratio, but never drop task-critical detail.
8. Output ONLY the compressed prompt — no preamble, no "here's the compressed version"."""


DECODE_SYSTEM = """You are a RESPONSE EXPANDER running on a free local model.
The cloud model returned a dense/telegraphic answer to save tokens.
Rewrite it into clear, complete prose for a human, without inventing facts.
Keep code blocks exact. Expand bullets into full sentences when helpful.
Output ONLY the expanded answer."""


class LocalLLM:
    def __init__(self, cfg: LocalConfig):
        self.cfg = cfg

    def chat(
        self,
        messages: list[dict[str, str]],
        *,
        temperature: float | None = None,
        max_tokens: int | None = None,
    ) -> str:
        url = self.cfg.base_url.rstrip("/") + "/chat/completions"
        payload: dict[str, Any] = {
            "model": self.cfg.model,
            "messages": messages,
            "temperature": self.cfg.temperature if temperature is None else temperature,
            "max_tokens": self.cfg.max_tokens if max_tokens is None else max_tokens,
            "stream": False,
        }
        headers = {
            "Content-Type": "application/json",
            "Authorization": f"Bearer {self.cfg.api_key}",
        }
        with httpx.Client(timeout=self.cfg.timeout_s) as client:
            r = client.post(url, json=payload, headers=headers)
            r.raise_for_status()
            data = r.json()
        try:
            return data["choices"][0]["message"]["content"] or ""
        except (KeyError, IndexError, TypeError) as e:
            raise RuntimeError(f"Unexpected local LLM response: {data!r}") from e

    def encode_text(self, text: str, target_ratio: float = 0.45) -> str:
        pct = max(5, min(95, int(target_ratio * 100)))
        user = (
            f"Target length: about {pct}% of the original token count.\n\n"
            f"--- ORIGINAL PROMPT ---\n{text}\n--- END ---"
        )
        return self.chat(
            [
                {"role": "system", "content": ENCODE_SYSTEM},
                {"role": "user", "content": user},
            ]
        ).strip()

    def decode_text(self, text: str) -> str:
        return self.chat(
            [
                {"role": "system", "content": DECODE_SYSTEM},
                {"role": "user", "content": text},
            ]
        ).strip()

    def health(self) -> dict[str, Any]:
        base = self.cfg.base_url.rstrip("/")
        models_url = base + "/models"
        try:
            with httpx.Client(timeout=5.0) as client:
                r = client.get(
                    models_url,
                    headers={"Authorization": f"Bearer {self.cfg.api_key}"},
                )
                ok = r.status_code < 500
                body = r.json() if r.headers.get("content-type", "").startswith("application/json") else r.text[:200]
                return {
                    "ok": ok,
                    "status_code": r.status_code,
                    "base_url": self.cfg.base_url,
                    "model": self.cfg.model,
                    "models": body,
                }
        except Exception as e:
            return {
                "ok": False,
                "error": str(e),
                "base_url": self.cfg.base_url,
                "model": self.cfg.model,
            }
