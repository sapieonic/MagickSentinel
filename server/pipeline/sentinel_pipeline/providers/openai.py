"""OpenAI adapter, kept as a second source for the analysis and judge slots.

Having two model vendors behind the same interface is not redundancy for its own
sake: it is what makes a per-tenant model swap a configuration change, which is the
whole reason no AI keys live on the endpoint.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field

from .anthropic import _schema


@dataclass
class OpenAIProvider:
    api_key: str
    model: str = "gpt-4.1"
    schema_name: str = "analysis.json"
    max_tokens: int = 2_000
    timeout_s: int = 120
    name: str = field(init=False)
    version: str = field(init=False)
    _client: object = field(default=None, init=False, repr=False)

    def __post_init__(self) -> None:
        from openai import OpenAI  # noqa: PLC0415 - lazy, see providers/__init__

        self._client = OpenAI(api_key=self.api_key, timeout=self.timeout_s)
        self._schema = _schema(self.schema_name)
        self.name = self.model
        self.version = f"{self.model}/{self.schema_name}"

    def _call(self, prompt: str, max_output_tokens: int) -> tuple[dict, int, int]:
        resp = self._client.chat.completions.create(
            model=self.model,
            max_tokens=max_output_tokens or self.max_tokens,
            response_format={
                "type": "json_schema",
                "json_schema": {"name": "result", "schema": self._schema, "strict": False},
            },
            messages=[{"role": "user", "content": prompt}],
        )
        payload = json.loads(resp.choices[0].message.content or "{}")
        usage = resp.usage
        return payload, usage.prompt_tokens, usage.completion_tokens

    def complete(self, prompt: str, *, max_output_tokens: int) -> tuple[dict, int, int]:
        return self._call(prompt, max_output_tokens)

    def judge(self, prompt: str) -> tuple[dict, int, int]:
        return self._call(prompt, self.max_tokens)
