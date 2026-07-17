"""OpenAI-compatible reverse proxy that encodes prompts before upstream."""

from __future__ import annotations

import json
import logging
import os
from typing import Any

import httpx
from fastapi import FastAPI, Request, Response
from fastapi.responses import JSONResponse, StreamingResponse

from .codec import PromptCodec
from .config import AppConfig, load_config
from .local_llm import LocalLLM

log = logging.getLogger("prompt_codec.proxy")


def create_app(cfg: AppConfig | None = None) -> FastAPI:
    cfg = cfg or load_config()
    codec = PromptCodec(cfg, LocalLLM(cfg.local))
    app = FastAPI(title="Prompt Codec Proxy", version="0.1.0")

    @app.get("/health")
    async def health():
        local = codec.llm.health()
        return {
            "ok": True,
            "encoder_mode": cfg.encoder.mode,
            "upstream": cfg.proxy.upstream_base_url,
            "local": local,
        }

    @app.get("/v1/models")
    async def models(request: Request):
        return await _proxy(request, "/models")

    @app.post("/v1/chat/completions")
    async def chat_completions(request: Request):
        body = await request.json()
        messages = body.get("messages") or []
        if not isinstance(messages, list) or not messages:
            return JSONResponse({"error": "messages required"}, status_code=400)

        result = codec.encode_messages(messages)
        body = dict(body)
        body["messages"] = result.messages

        if cfg.proxy.log_stats and result.stats:
            log.info(
                "encode %s → %s tokens (%.1f%% saved, ~$%.6f) notes=%s",
                result.stats.before_tokens,
                result.stats.after_tokens,
                result.stats.pct_saved,
                result.stats.usd_saved,
                result.notes,
            )
            # Attach non-standard header stats via custom field clients can ignore
            body.setdefault("metadata", {})
            if isinstance(body["metadata"], dict):
                body["metadata"]["prompt_codec"] = result.stats.as_dict()

        stream = bool(body.get("stream"))
        return await _forward_chat(request, body, stream=stream, stats=result.stats)

    @app.post("/v1/completions")
    async def completions(request: Request):
        body = await request.json()
        prompt = body.get("prompt", "")
        if isinstance(prompt, str) and prompt:
            result = codec.encode_text(prompt)
            body = dict(body)
            body["prompt"] = result.text
            if cfg.proxy.log_stats and result.stats:
                log.info(
                    "encode prompt %s → %s tokens (%.1f%% saved)",
                    result.stats.before_tokens,
                    result.stats.after_tokens,
                    result.stats.pct_saved,
                )
        stream = bool(body.get("stream"))
        return await _forward(request, "/completions", body, stream=stream)

    @app.api_route("/v1/{path:path}", methods=["GET", "POST", "PUT", "DELETE", "PATCH"])
    async def catch_all(path: str, request: Request):
        # passthrough for other endpoints
        if request.method == "GET":
            return await _proxy(request, f"/{path}")
        try:
            body = await request.json()
        except Exception:
            body = None
        return await _forward(request, f"/{path}", body, stream=False)

    async def _auth_headers(request: Request) -> dict[str, str]:
        headers: dict[str, str] = {"Content-Type": "application/json"}
        client_auth = request.headers.get("authorization")
        if cfg.proxy.pass_client_auth and client_auth:
            headers["Authorization"] = client_auth
        else:
            key = os.environ.get(cfg.proxy.upstream_api_key_env, "")
            if key:
                headers["Authorization"] = f"Bearer {key}"
        return headers

    async def _proxy(request: Request, path: str):
        url = cfg.proxy.upstream_base_url.rstrip("/") + path
        headers = await _auth_headers(request)
        async with httpx.AsyncClient(timeout=120.0) as client:
            r = await client.get(url, headers=headers, params=dict(request.query_params))
        return Response(
            content=r.content,
            status_code=r.status_code,
            media_type=r.headers.get("content-type", "application/json"),
        )

    async def _forward(
        request: Request,
        path: str,
        body: Any,
        *,
        stream: bool,
    ):
        url = cfg.proxy.upstream_base_url.rstrip("/") + path
        headers = await _auth_headers(request)
        if stream:
            client = httpx.AsyncClient(timeout=None)

            async def gen():
                try:
                    async with client.stream(
                        "POST", url, headers=headers, json=body
                    ) as r:
                        async for chunk in r.aiter_bytes():
                            yield chunk
                finally:
                    await client.aclose()

            return StreamingResponse(gen(), media_type="text/event-stream")

        async with httpx.AsyncClient(timeout=300.0) as client:
            r = await client.post(url, headers=headers, json=body)
        return Response(
            content=r.content,
            status_code=r.status_code,
            media_type=r.headers.get("content-type", "application/json"),
        )

    async def _forward_chat(
        request: Request,
        body: dict[str, Any],
        *,
        stream: bool,
        stats: Any,
    ):
        resp = await _forward(request, "/chat/completions", body, stream=stream)
        if stream or not cfg.decoder.enabled:
            return resp
        # Optional decode of assistant content (non-stream only)
        try:
            raw = resp.body if hasattr(resp, "body") else None
            if raw is None and isinstance(resp, Response):
                # Response already built
                return resp
        except Exception:
            return resp
        return resp

    return app


def run(host: str | None = None, port: int | None = None, config_path: str | None = None):
    import uvicorn

    cfg = load_config(config_path)
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    app = create_app(cfg)
    uvicorn.run(
        app,
        host=host or cfg.proxy.host,
        port=port or cfg.proxy.port,
        log_level="info",
    )


if __name__ == "__main__":
    run()
