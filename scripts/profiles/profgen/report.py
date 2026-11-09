"""Отчёт сборки производителя (`report.json`): потери ключей, правила, лимиты, счёт пресетов."""
import copy
from collections import Counter, defaultdict
from collections.abc import Iterable, Sequence

from .hand_kit import HandKit
from .keymap import KeyMap
from .orca_source import SourceTree
from .rules import RuleHit

HAND_KIT_DECISION = ("Д3, 07.11.2026: значения комплекта, на которых уже печатают, не меняются до сверки "
                     "с Corpo4; это значения Protype, а не Orca")


class ReportBuilder:
    """Копит события сборки; каждый пресет Orca сообщает о себе один раз."""

    def __init__(self, vendor_id: str, kinds: Sequence[str]):
        self._vendor_id = vendor_id
        self._kinds = tuple(kinds)
        self._dropped: dict[str, Counter] = defaultdict(Counter)
        self._rejected: dict[str, dict[str, Counter]] = defaultdict(lambda: defaultdict(Counter))
        self._carried: Counter = Counter()
        self._derived: Counter = Counter()
        self._rules: dict[str, set[str]] = defaultdict(set)
        self._limits: dict[str, dict] = {}
        self._skipped: list[dict] = []
        self._unique: dict[str, set[str]] = {kind: set() for kind in self._kinds}
        self._per_model: dict[str, dict[str, set[str]]] = {}
        self._hand: dict[str, dict] = {}

    def hand_kit(self, kit: HandKit) -> None:
        """Ручной комплект модели (Д3): что он заменил, откуда он и его id."""
        self._hand[kit.model_key] = {
            "decision": HAND_KIT_DECISION,
            "replaces": kit.replaces,
            "file": {"path": kit.path, "commit": kit.commit, "blob": kit.blob},
            "source": dict(kit.source),
            "presets": {kind: kit.presets[kind].id for kind in self._kinds},
            "additions": {"machine": kit.additions.as_report()} if kit.additions else {},
        }

    def drop(self, kind: str, key: str) -> None:
        """Ключ Orca, которого нет в схеме: `kind` — вид пресета-источника."""
        self._dropped[kind][key] += 1

    def reject(self, kind: str, key: str, reason: str) -> None:
        """Значение, которое схема не приняла: `kind` — область ключа схемы строчными."""
        self._rejected[kind][key][reason] += 1

    def carry(self, key: str) -> None:
        self._carried[key] += 1

    def derived_missing(self, key: str) -> None:
        """Производный ключ, который пресет хотел записать, но его нет в схеме."""
        self._derived[key] += 1

    def hit(self, rule_hit: RuleHit) -> None:
        self._rules[rule_hit.rule].add(rule_hit.preset)

    def limits(self, machine: str, rep: dict[str, dict]) -> None:
        """Запись `apply_limits` машины Orca; пустая не пишется."""
        if rep:
            self._limits[machine] = copy.deepcopy(rep)

    def skip(self, entry: dict) -> None:
        self._skipped.append(dict(entry))

    def count(self, kind: str, model_key: str, preset_id: str) -> None:
        """Пресет в пакете модели; общий для моделей пресет в `counts` считается один раз."""
        self._unique[kind].add(preset_id)
        per_kind = self._per_model.setdefault(model_key, {k: set() for k in self._kinds})
        per_kind[kind].add(preset_id)

    def build(self, source: SourceTree, keymap: KeyMap, limits_unverified: Iterable[str],
              rule_keys_outside_schema: Iterable[str]) -> dict:
        return {
            "vendor_id": self._vendor_id,
            "source": {"repo": source.repo, "tag": source.tag, "commit": source.commit},
            "inputs": {
                "schema_sha256": keymap.schema_sha256,
                "schema_version": keymap.schema_version,
                "libslic3r_keys_sha256": keymap.keys_sha256,
                "libslic3r_pin": keymap.libslic3r_pin,
            },
            "counts": {kind: len(self._unique[kind]) for kind in self._kinds},
            "per_model": {
                model: {kind: len(ids) for kind, ids in per_kind.items()}
                for model, per_kind in self._per_model.items()
            },
            "dropped_keys": {kind: dict(keys) for kind, keys in self._dropped.items() if keys},
            "rejected": {
                kind: {key: dict(reasons) for key, reasons in keys.items()}
                for kind, keys in self._rejected.items() if keys
            },
            "carried_ahead": dict(self._carried),
            "derived_not_in_schema": dict(self._derived),
            "rules": {rule: sorted(names) for rule, names in self._rules.items()},
            "rule_keys_outside_schema": sorted(rule_keys_outside_schema),
            "limits": copy.deepcopy(self._limits),
            "limits_unverified": sorted(limits_unverified),
            "skipped_assets": sorted((dict(e) for e in self._skipped), key=lambda e: (e["model"], e["file"])),
            "hand_kits": copy.deepcopy(dict(sorted(self._hand.items()))),
        }
