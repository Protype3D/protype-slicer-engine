"""Совместимость с движком: переменные шаблонов, которых нет у пина, заменяются константами."""
import re
from collections.abc import Mapping
from typing import Any

from .rules import RuleHit, RuleResult

RULE_NAME = "engine_template_constants"
GCODE_SUFFIX = "_gcode"

# Имя переменной -> (значение, причина).
TEMPLATE_CONSTANTS: dict[str, tuple[str, str]] = {
    "timelapse_type": (
        "0",
        "парсер плейсхолдеров пина движка 2.3.1 отвергает timelapse_type; 0 — Traditional, умолчание Orca; "
        "ни один пресет QIDI его не задаёт; убрать, когда движок перейдёт на Orca 2.5",
    ),
}

_PATTERNS = tuple((re.compile(rf"\b{re.escape(name)}\b"), value) for name, (value, _) in TEMPLATE_CONSTANTS.items())


def _substitute(text: str) -> str:
    for pattern, value in _PATTERNS:
        text = pattern.sub(value, text)
    return text


def _convert(value: Any) -> Any:
    if isinstance(value, str):
        return _substitute(value)
    if isinstance(value, list):
        return [_substitute(item) if isinstance(item, str) else item for item in value]
    return value


def apply_template_constants(values: Mapping[str, Any], preset_name: str) -> RuleResult:
    """Новый словарь, где в `*_gcode` целые слова-переменные заменены константами; вход не меняется."""
    result = {key: _convert(value) if key.endswith(GCODE_SUFFIX) else value for key, value in values.items()}
    changed = result != dict(values)
    hits = (RuleHit(RULE_NAME, preset_name),) if changed else ()
    return RuleResult(result, hits, {})
