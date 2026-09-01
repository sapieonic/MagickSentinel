"""Anthropic adapter for the analyser and the compliance judge.

Both slots return parsed JSON. Structured output is requested through a tool
definition rather than by asking for JSON in prose, because the schema is then
enforced by the API rather than by hoping — and the analyser's retry path exists for
the cases where even that comes back wrong.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path

_CONTRACTS = Path(__file__).resolve().parents[4] / "contracts" / "schemas"


def _schema(name: str) -> dict:
    with (_CONTRACTS / name).open(encoding="utf-8") as fh:
        return json.load(fh)


@dataclass
class AnthropicProvider:
    """One adapter serving both the analysis and judge slots.

    ``schema_name`` selects which contract the tool is shaped from, so the same class
    backs ``CallAnalyzer`` and ``ComplianceJudge`` without either importing the SDK.
    """

    api_key: str
    model: str = "claude-sonnet-5"
    schema_name: str = "analysis.json"
    max_tokens: int = 2_000
    timeout_s: int = 120
    name: str = field(init=False)
    version: str = field(init=False)
    _client: object = field(default=None, init=False, repr=False)

    def __post_init__(self) -> None:
        import anthropic  # noqa: PLC0415 - lazy, see providers/__init__

        self._client = anthropic.Anthropic(api_key=self.api_key, timeout=self.timeout_s)
        self._schema = _schema(self.schema_name)
        self.name = self.model
        # The version travels with every stored artifact. When the model or the
        # schema changes, results from before and after must stay distinguishable.
        self.version = f"{self.model}/{self.schema_name}"

    def _call(self, prompt: str, max_output_tokens: int) -> tuple[dict, int, int]:
        tool_name = "record_analysis" if "analysis" in self.schema_name else "record_verdict"
        message = self._client.messages.create(
            model=self.model,
            max_tokens=max_output_tokens or self.max_tokens,
            tools=[{
                "name": tool_name,
                "description": "Record the structured result.",
                "input_schema": self._schema,
            }],
            tool_choice={"type": "tool", "name": tool_name},
            messages=[{"role": "user", "content": prompt}],
        )
        payload: dict = {}
        for block in message.content:
            if getattr(block, "type", None) == "tool_use":
                payload = dict(block.input)
                break
        return payload, message.usage.input_tokens, message.usage.output_tokens

    # The analyser slot.
    def complete(self, prompt: str, *, max_output_tokens: int) -> tuple[dict, int, int]:
        return self._call(prompt, max_output_tokens)

    # The judge slot.
    def judge(self, prompt: str) -> tuple[dict, int, int]:
        return self._call(prompt, self.max_tokens)
