"""Cost controls.

Built in Phase 3, not bolted on later. A 200-seat floor at 5 h talk time per agent is
roughly 60,000 minutes a day; at that volume an unbounded per-call model spend turns
a healthy per-seat price into a loss without anyone noticing until the invoice.

Five controls, all of which have to exist before the first customer is live:

1. a per-tenant monthly budget with alerts at 70% and 90%,
2. a per-call token ceiling — calls over it are truncated with a marker, not dropped,
3. no analysis for calls under 15 seconds,
4. a kill switch that drops to tier-1 rules only when spend spikes, and
5. token counts and cost recorded per call, so the accounting is per tenant.

Money is in paise as an integer throughout, never rupees and never a float.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum


class BudgetState(str, Enum):
    OK = "ok"
    WARN_70 = "warn_70"
    WARN_90 = "warn_90"
    EXHAUSTED = "exhausted"


@dataclass(frozen=True)
class ModelPricing:
    """Per-million-token prices in paise.

    Held as data rather than constants so a price change is configuration. Paise
    per million tokens keeps the arithmetic in integers all the way through.
    """

    model: str
    input_paise_per_mtok: int
    output_paise_per_mtok: int

    def cost_paise(self, input_tokens: int, output_tokens: int) -> int:
        # Integer arithmetic, rounding up: under-reporting spend is the failure that
        # matters here, and a fraction of a paisa per call compounds across 60,000
        # minutes a day.
        total = (
            input_tokens * self.input_paise_per_mtok
            + output_tokens * self.output_paise_per_mtok
        )
        return -(-total // 1_000_000)


@dataclass
class TenantBudget:
    """One tenant's monthly spend against its cap."""

    tenant_id: str
    monthly_budget_paise: int | None
    spent_paise: int = 0
    # Set by an operator when spend spikes. While on, only tier-1 rules run: no
    # analysis, no judge. Capture and compliance keep working, which is the part the
    # customer is actually paying for.
    kill_switch: bool = False

    @property
    def state(self) -> BudgetState:
        if self.monthly_budget_paise is None or self.monthly_budget_paise <= 0:
            return BudgetState.OK
        ratio = self.spent_paise / self.monthly_budget_paise
        if ratio >= 1.0:
            return BudgetState.EXHAUSTED
        if ratio >= 0.9:
            return BudgetState.WARN_90
        if ratio >= 0.7:
            return BudgetState.WARN_70
        return BudgetState.OK

    @property
    def remaining_paise(self) -> int | None:
        if self.monthly_budget_paise is None:
            return None
        return max(0, self.monthly_budget_paise - self.spent_paise)

    def record(self, paise: int) -> None:
        self.spent_paise += paise


@dataclass
class Decision:
    """What the pipeline is allowed to do for one call."""

    analyse: bool
    judge: bool
    reason: str
    max_input_tokens: int = 0


@dataclass
class CostPolicy:
    """Decides, per call, what may run.

    Deliberately conservative in one direction only: exhausting a budget stops the
    model calls, never the deterministic compliance rules. Tier 1 is the thing the
    bank is being shown, and it costs nothing to run.
    """

    per_call_token_ceiling: int = 24_000
    min_call_ms: int = 15_000
    pricing: dict[str, ModelPricing] = field(default_factory=dict)

    def decide(self, budget: TenantBudget, duration_ms: int, tier1_hit: bool) -> Decision:
        if duration_ms < self.min_call_ms:
            # Logged, not analysed. Nothing useful comes out of a nine-second call
            # and this is the largest saving available.
            return Decision(False, False, "call shorter than the analysis floor")
        if budget.kill_switch:
            return Decision(False, False, "kill switch engaged: tier-1 rules only")
        if budget.state is BudgetState.EXHAUSTED:
            return Decision(False, False, "monthly budget exhausted: tier-1 rules only")
        if budget.state is BudgetState.WARN_90 and not tier1_hit:
            # Past 90% the sample-based judging stops first: calls tier 1 already
            # flagged still get judged, because those are the ones a reviewer will
            # be asked to defend.
            return Decision(True, False, "budget above 90%: judging flagged calls only",
                            self.per_call_token_ceiling)
        return Decision(True, True, "within budget", self.per_call_token_ceiling)

    def cost_paise(self, model: str, input_tokens: int, output_tokens: int) -> int:
        pricing = self.pricing.get(model)
        if pricing is None:
            # An unpriced model must not silently record zero spend — that is how a
            # budget gets blown by a model nobody added to the table.
            raise KeyError(f"no pricing configured for model {model!r}")
        return pricing.cost_paise(input_tokens, output_tokens)

    def truncation_budget_chars(self) -> int:
        """Roughly four characters per token, which is close enough for a ceiling."""
        return self.per_call_token_ceiling * 4


def alerts_for(previous: BudgetState, current: BudgetState) -> list[str]:
    """Alerts to raise on a budget state transition.

    Edge-triggered, so a tenant sitting at 91% for three weeks generates one alert
    rather than one per call.
    """
    if current is previous:
        return []
    order = [BudgetState.OK, BudgetState.WARN_70, BudgetState.WARN_90, BudgetState.EXHAUSTED]
    if order.index(current) <= order.index(previous):
        return []
    messages = {
        BudgetState.WARN_70: "tenant has used 70% of its monthly model budget",
        BudgetState.WARN_90: "tenant has used 90% of its monthly model budget; "
                             "sample judging suspended",
        BudgetState.EXHAUSTED: "tenant monthly model budget exhausted; "
                               "tier-1 compliance rules only",
    }
    return [messages[current]] if current in messages else []
