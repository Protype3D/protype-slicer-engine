"""Дерево профилей OrcaSlicer по тегу с пином коммита и локальным кэшем."""
import os
import shutil
import subprocess
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path

from .errors import GeneratorError

ORCA_REPO_URL = "https://github.com/OrcaSlicer/OrcaSlicer.git"
# тег -> (коммит, штамп выхода)
ORCA_PINS: dict[str, tuple[str, str]] = {
    "v2.5.0": ("40ecc66f78ed72dc368e0c1ebe40d77f00cc8a19", "2026-11-07T00:00:00Z"),
}

# Независимость от пользовательской конфигурации git: ни хуков, ни подписи, ни предупреждений.
_GIT_OPTS = (
    "-c", "core.hooksPath=/dev/null",
    "-c", "commit.gpgsign=false",
    "-c", "init.defaultBranch=main",
    "-c", "advice.detachedHead=false",
)
_GIT_ENV = {
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_TERMINAL_PROMPT": "0",
}


@dataclass(frozen=True)
class SourceTree:
    profiles_root: Path
    repo: str
    tag: str
    commit: str
    released_at: str


def _git(cwd: Path, *args: str) -> str:
    cmd = ["git", *_GIT_OPTS, *args]
    try:
        done = subprocess.run(
            cmd, cwd=cwd, check=True, capture_output=True, text=True,
            env={**os.environ, **_GIT_ENV},
        )
    except subprocess.CalledProcessError as err:
        raise GeneratorError(f"{' '.join(cmd)}: {err.stderr.strip()}") from err
    except OSError as err:
        raise GeneratorError(f"{' '.join(cmd)}: {err}") from err
    return done.stdout.strip()


def _is_clean_copy(checkout: Path, commit: str, vendor_dir: str) -> bool:
    if not (checkout / ".git").exists():
        return False
    if not (checkout / "resources/profiles" / f"{vendor_dir}.json").is_file():
        return False
    try:
        return (_git(checkout, "rev-parse", "HEAD") == commit
                and _git(checkout, "status", "--porcelain") == "")
    except GeneratorError:
        return False


def _remote_tag_sha(checkout: Path, tag: str) -> str | None:
    out = _git(checkout, "ls-remote", "origin", f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}")
    found: dict[str, str] = {}
    for line in out.splitlines():
        sha, _, ref = line.partition("\t")
        found[ref] = sha
    return found.get(f"refs/tags/{tag}^{{}}") or found.get(f"refs/tags/{tag}")


def _build(checkout: Path, tag: str, commit: str, vendor_dir: str, repo_url: str) -> None:
    checkout.mkdir(parents=True)
    _git(checkout, "init", "-q")
    _git(checkout, "remote", "add", "origin", repo_url)
    _git(checkout, "sparse-checkout", "set", "--no-cone",
         f"resources/profiles/{vendor_dir}/", f"resources/profiles/{vendor_dir}.json")
    found = _remote_tag_sha(checkout, tag)
    if found != commit:
        raise GeneratorError(
            f"тег {tag} указывает на {found}, ожидался {commit}: "
            "пин не совпал, сборка из других данных отменена")
    _git(checkout, "fetch", "-q", "--depth", "1", "--filter=blob:none", "origin", commit)
    _git(checkout, "checkout", "-q", "--detach", "FETCH_HEAD")
    head = _git(checkout, "rev-parse", "HEAD")
    if head != commit:
        raise GeneratorError(f"после checkout HEAD={head}, ожидался {commit} (тег {tag})")


def fetch_vendor_tree(
    tag: str,
    vendor_dir: str,
    cache_dir: Path,
    *,
    repo_url: str = ORCA_REPO_URL,
    pins: Mapping[str, tuple[str, str]] = ORCA_PINS,
) -> SourceTree:
    if tag not in pins:
        raise GeneratorError(f"тег {tag} не закреплён: известные теги {sorted(pins)}")
    commit, released_at = pins[tag]
    checkout = Path(cache_dir) / "orca" / f"{commit}-{vendor_dir}"
    if not _is_clean_copy(checkout, commit, vendor_dir):
        shutil.rmtree(checkout, ignore_errors=True)
        try:
            _build(checkout, tag, commit, vendor_dir, repo_url)
        except BaseException:
            shutil.rmtree(checkout, ignore_errors=True)
            raise
    return SourceTree(
        profiles_root=checkout / "resources/profiles",
        repo=repo_url.removesuffix(".git"),
        tag=tag,
        commit=commit,
        released_at=released_at,
    )
