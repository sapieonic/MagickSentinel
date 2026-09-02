"""Core value types shared by the workers.

Plain dataclasses rather than ORM rows: the pipeline reads from object storage and
writes through narrow SQL, and keeping the domain types free of a database session
is what lets the rule engine be tested against a fixture corpus with no Postgres.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import date, datetime
from enum import Enum


class Channel(int, Enum):
    """Channel 0 is the borrower, channel 1 the agent.

    They were captured separately and stay separate. Because of that there is no
    diarization step anywhere in this pipeline, and there must not be one: the
    speaker is known exactly rather than inferred.
    """

    FAR = 0
    NEAR = 1

    @property
    def speaker(self) -> str:
        return "borrower" if self is Channel.FAR else "agent"


class Severity(str, Enum):
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"
    CRITICAL = "critical"

    @property
    def rank(self) -> int:
        return {"low": 0, "medium": 1, "high": 2, "critical": 3}[self.value]


class Disposition(str, Enum):
    PTP = "ptp"
    REFUSAL = "refusal"
    DISPUTE = "dispute"
    WRONG_NUMBER = "wrong_number"
    NO_CONTACT = "no_contact"
    CALLBACK_REQUESTED = "callback_requested"
    PARTIAL_PAYMENT = "partial_payment"
    ESCALATION = "escalation"
    OTHER = "other"


@dataclass(frozen=True)
class Word:
    """One recognised token with its timing, in call-relative milliseconds."""

    text: str
    start_ms: int
    end_ms: int
    confidence: float | None = None


@dataclass
class ChannelTranscript:
    """One channel's transcript, as produced by the batch ASR path."""

    channel: Channel
    text: str
    words: list[Word] = field(default_factory=list)
    language: str = "hi"
    provider: str = "unknown"
    provider_version: str = "unknown"
    confidence: float | None = None

    def span_text(self, start_ms: int, end_ms: int) -> str:
        """Verbatim text between two timestamps.

        Every flag has to be traceable to the words it came from — a finding a
        reviewer cannot trace is not usable as evidence with a bank — so this is
        the primitive the rule engine builds evidence from.
        """
        if not self.words:
            return self.text if start_ms <= 0 else ""
        return " ".join(w.text for w in self.words if w.start_ms < end_ms and w.end_ms > start_ms)

    def words_within(self, start_ms: int, end_ms: int) -> list[Word]:
        return [w for w in self.words if w.start_ms < end_ms and w.end_ms > start_ms]


@dataclass
class CallContext:
    """Everything the analysers need about a call that is not the audio itself."""

    call_id: str
    tenant_id: str
    user_uid: str
    started_at: datetime
    duration_ms: int
    tenant_timezone: str = "Asia/Kolkata"
    account_ref: str | None = None
    direction: str = "outbound"
    capture_tier: str = "A"
    # Calls to the same account_ref in the preceding 24 h, supplied by the caller
    # because the rule engine does not talk to the database.
    prior_contacts_24h: int = 0
    interruptions: int | None = None


@dataclass
class Transcript:
    """Both channels of one call."""

    context: CallContext
    channels: dict[Channel, ChannelTranscript] = field(default_factory=dict)

    def get(self, channel: Channel) -> ChannelTranscript | None:
        return self.channels.get(channel)

    @property
    def near(self) -> ChannelTranscript | None:
        return self.channels.get(Channel.NEAR)

    @property
    def far(self) -> ChannelTranscript | None:
        return self.channels.get(Channel.FAR)


@dataclass(frozen=True)
class Finding:
    """A tier-1 rule hit, or an upheld tier-2 verdict.

    ``span_start_ms``/``span_end_ms`` and ``evidence_text`` are not optional in
    practice for anything a reviewer will act on: they are what makes the flag
    defensible.
    """

    rule_id: str
    severity: Severity
    tier: int
    span_start_ms: int | None = None
    span_end_ms: int | None = None
    evidence_text: str | None = None
    rationale: str | None = None
    confidence: float | None = None

    def with_rationale(self, rationale: str, confidence: float) -> "Finding":
        return Finding(
            rule_id=self.rule_id,
            severity=self.severity,
            tier=2,
            span_start_ms=self.span_start_ms,
            span_end_ms=self.span_end_ms,
            evidence_text=self.evidence_text,
            rationale=rationale,
            confidence=confidence,
        )


@dataclass
class PromiseToPay:
    present: bool
    amount_paise: int | None = None
    due_date: date | None = None
    confidence: float = 0.0
    evidence_span_ms: tuple[int, int] | None = None


@dataclass
class Analysis:
    """The output of one CallAnalyzer invocation, after schema validation."""

    summary: str
    disposition: Disposition
    ptp: PromiseToPay
    sentiment: dict
    talk_ratio: float
    interruptions: int
    next_action: str | None = None
    model: str = "unknown"
    prompt_version: str = "unknown"
    input_tokens: int = 0
    output_tokens: int = 0
    truncated: bool = False
