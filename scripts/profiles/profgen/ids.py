"""Стабильные id пресетов (С3) и закреплённые id машины и экструдера комплекта X-Max 4.

Процесс и пластик комплекта (607a0051…, 691567e5…) больше не закреплены за пресетами Orca: по Д3 они
выходят со значениями Protype из ручного комплекта (`hand_kit.py`), а пресеты Orca получают свои uuid5.
"""
import re
import uuid
from collections.abc import Mapping

from .errors import GeneratorError

ID_NAMESPACE: uuid.UUID = uuid.uuid5(uuid.NAMESPACE_URL, "https://protype.dev/profiles")

ID_PATTERN: re.Pattern[str] = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
)

PINNED_IDS: dict[tuple[str, str, str], str] = {
    ("qidi", "machine", "Qidi X-Max 4 0.4 nozzle"): "c425afad-bc30-42db-9744-c4573aae01b2",
    ("qidi", "extruder", "Qidi X-Max 4 0.4 nozzle"): "698c21b4-f1a2-432d-a808-d246520f32a5",
}

_VERSION_4_BYTE = 6
_VARIANT_BYTE = 8


def preset_id(vendor_id: str, kind: str, orca_name: str) -> str:
    """Детерминированный UUIDv4-подобный id пресета из uuid5 по его ключу."""
    raw = bytearray(uuid.uuid5(ID_NAMESPACE, f"{vendor_id}/{kind}/{orca_name}").bytes)
    raw[_VERSION_4_BYTE] = (raw[_VERSION_4_BYTE] & 0x0F) | 0x40
    raw[_VARIANT_BYTE] = (raw[_VARIANT_BYTE] & 0x3F) | 0x80
    return str(uuid.UUID(bytes=bytes(raw)))


class IdAllocator:
    """Выдаёт id пресетов одного производителя и проверяет закреплённые пары."""

    def __init__(
        self,
        vendor_id: str,
        pinned: Mapping[tuple[str, str, str], str] = PINNED_IDS,
    ) -> None:
        self._vendor_id = vendor_id
        self._pinned = dict(pinned)
        self._requested: dict[tuple[str, str], str] = {}
        self._owner: dict[str, tuple[str, str]] = {}
        self._clashes: list[tuple[str, str]] = []

    def id_for(self, kind: str, orca_name: str) -> str:
        """Закреплённый id пары либо вычисленный preset_id; пара запоминается."""
        pair = (kind, orca_name)
        if pair in self._requested:
            return self._requested[pair]
        pinned_id = self._pinned.get((self._vendor_id, kind, orca_name))
        new_id = pinned_id if pinned_id is not None else preset_id(self._vendor_id, kind, orca_name)
        self._requested[pair] = new_id
        owner = self._owner.setdefault(new_id, pair)
        if owner != pair:
            self._clashes.append((f"{owner[0]} {owner[1]}", f"{kind} {orca_name}"))
        return new_id

    def check(self) -> None:
        """Бросает GeneratorError при незапрошенной закреплённой паре или коллизии id."""
        missing = [
            name
            for (vendor, kind, name) in self._pinned
            if vendor == self._vendor_id and (kind, name) not in self._requested
        ]
        if missing:
            raise GeneratorError(
                "закреплённые пресеты не запрошены: " + ", ".join(sorted(missing))
            )
        if self._clashes:
            first, second = self._clashes[0]
            raise GeneratorError(f"коллизия id: «{first}» и «{second}» дали один id")
