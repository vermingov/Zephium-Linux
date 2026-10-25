"""What is installed, what the last run did, and the lock that keeps runs apart."""

import fcntl
import json
import time
from contextlib import contextmanager
from dataclasses import asdict, dataclass, field
from pathlib import Path

from .config import STATE_DIR

STATE_FILE = STATE_DIR / "state.json"
LOCK_FILE = STATE_DIR / "lock"
LOG_DIR = STATE_DIR / "logs"


@dataclass
class Installed:
    id: str
    version: str
    # "port" is our build of upstream plus the patches; "official" is
    # upstream's own Linux release.
    kind: str
    upstream_commit: str = ""
    patch_digest: str = ""
    installed_at: float = field(default_factory=time.time)


@dataclass
class State:
    installed: list[Installed] = field(default_factory=list)
    last_run: dict = field(default_factory=dict)
    # Upstream's updater key, from the source of the build we installed: an
    # official download must be signed by it, as Tauri's own updater requires.
    trusted_key: str = ""
    # A build rolled back from is not rebuilt until upstream or the patches move.
    hold: dict = field(default_factory=dict)
    # The install last published to the GitHub fork, and its tag.
    published: dict = field(default_factory=dict)

    @property
    def current(self) -> Installed | None:
        return self.installed[-1] if self.installed else None


def load() -> State:
    if not STATE_FILE.exists():
        return State()
    raw = json.loads(STATE_FILE.read_text())
    return State(
        installed=[Installed(**entry) for entry in raw.get("installed", [])],
        last_run=raw.get("last_run", {}),
        trusted_key=raw.get("trusted_key", ""),
        hold=raw.get("hold", {}),
        published=raw.get("published", {}),
    )


def save(state: State) -> None:
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    temporary = STATE_FILE.with_suffix(".tmp")
    temporary.write_text(json.dumps(asdict(state), indent=2) + "\n")
    temporary.replace(STATE_FILE)


@contextmanager
def exclusive_run():
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    with LOCK_FILE.open("w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit("another zephium-linux run is in progress") from None
        yield


def new_log() -> Path:
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    logs = sorted(LOG_DIR.glob("*.log"))
    for old in logs[:-20]:
        old.unlink()
    return LOG_DIR / time.strftime("%Y-%m-%dT%H%M%S.log")
