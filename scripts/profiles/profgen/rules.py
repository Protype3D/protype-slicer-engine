"""Общие типы результата правил производителя."""
from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True)
class RuleHit:
    """Правило, которое действительно изменило пресет."""

    rule: str
    preset: str


@dataclass(frozen=True)
class RuleResult:
    """Новый словарь пресета, сработавшие правила и запись отчёта о лимитах."""

    values: dict[str, Any]
    hits: tuple[RuleHit, ...]
    limits: dict[str, dict]
