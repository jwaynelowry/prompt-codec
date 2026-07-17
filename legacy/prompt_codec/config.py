from __future__ import annotations

from pathlib import Path
from typing import Any, Literal

import yaml
from pydantic import BaseModel, Field


class LocalConfig(BaseModel):
    base_url: str = "http://127.0.0.1:11434/v1"
    api_key: str = "ollama"
    model: str = "gemma4:12b-mlx"
    timeout_s: float = 120.0
    temperature: float = 0.1
    max_tokens: int = 2048


class EncoderConfig(BaseModel):
    mode: Literal["rules", "local", "hybrid"] = "hybrid"
    target_ratio: float = 0.45
    protect_system_under_chars: int = 800
    min_chars_to_compress: int = 400
    rules_enabled: bool = True
    roles: list[str] = Field(default_factory=lambda: ["user", "system", "tool"])


class DecoderConfig(BaseModel):
    enabled: bool = False
    mode: Literal["local", "off"] = "off"


class ProxyConfig(BaseModel):
    host: str = "127.0.0.1"
    port: int = 8787
    upstream_base_url: str = "https://api.x.ai/v1"
    upstream_api_key_env: str = "X_API_KEY"
    pass_client_auth: bool = True
    log_stats: bool = True


class StatsConfig(BaseModel):
    encoding: str = "cl100k_base"
    usd_per_mtok_input: float = 3.0


class AppConfig(BaseModel):
    local: LocalConfig = Field(default_factory=LocalConfig)
    encoder: EncoderConfig = Field(default_factory=EncoderConfig)
    decoder: DecoderConfig = Field(default_factory=DecoderConfig)
    proxy: ProxyConfig = Field(default_factory=ProxyConfig)
    stats: StatsConfig = Field(default_factory=StatsConfig)


def default_config_path() -> Path:
    return Path(__file__).resolve().parent.parent / "config.yaml"


def load_config(path: str | Path | None = None) -> AppConfig:
    p = Path(path) if path else default_config_path()
    if not p.exists():
        example = p.parent / "config.example.yaml"
        if example.exists():
            data = yaml.safe_load(example.read_text()) or {}
            return AppConfig.model_validate(data)
        return AppConfig()
    data: dict[str, Any] = yaml.safe_load(p.read_text()) or {}
    return AppConfig.model_validate(data)
