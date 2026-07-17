from prompt_codec.rules import rules_compress
from prompt_codec.codec import PromptCodec
from prompt_codec.config import AppConfig, EncoderConfig


def test_rules_removes_dupes_and_fluff():
    raw = """
Please help me.

auth uses JWT
auth uses JWT

Thank you!
"""
    out = rules_compress(raw)
    assert "auth uses JWT" in out
    assert out.count("auth uses JWT") == 1
    assert "Thank you" not in out
    assert "Please" not in out or "Please help" not in out


def test_encode_rules_saves_tokens():
    cfg = AppConfig(encoder=EncoderConfig(mode="rules", min_chars_to_compress=10))
    codec = PromptCodec(cfg)
    sample = ("Please help. " * 20) + "\n".join([f"- item {i}" for i in range(30)])
    sample += "\nThank you!"
    r = codec.encode_text(sample, mode="rules")
    assert r.stats is not None
    assert r.stats.after_tokens <= r.stats.before_tokens
    assert r.stats.saved_tokens >= 0
