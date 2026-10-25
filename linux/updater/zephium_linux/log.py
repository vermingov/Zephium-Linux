"""One log per run: what was decided, and everything the commands printed."""

import subprocess
import sys
import time
from pathlib import Path


class CommandFailed(Exception):
    def __init__(self, args: list[str], code: int, tail: str):
        self.tail = tail
        super().__init__(f"{' '.join(map(str, args))} exited with {code}")


class Log:
    def __init__(self, path: Path):
        self.path = path
        self.file = path.open("a", buffering=1)

    def say(self, message: str) -> None:
        line = f"{time.strftime('%H:%M:%S')} {message}"
        print(line, file=sys.stderr, flush=True)
        self.file.write(line + "\n")

    def run(self, args, *, cwd=None, env=None, timeout=None, capture=False) -> str:
        """Run a command with its output in the log; return stdout when captured."""
        self.file.write(f"$ {' '.join(map(str, args))}\n")
        process = subprocess.run(
            [str(arg) for arg in args],
            cwd=cwd,
            env=env,
            timeout=timeout,
            stdout=subprocess.PIPE if capture else self.file,
            stderr=subprocess.STDOUT if not capture else self.file,
            text=True,
        )
        if process.returncode != 0:
            raise CommandFailed(args, process.returncode, self.tail())
        return process.stdout if capture else ""

    def tail(self, lines: int = 60) -> str:
        self.file.flush()
        return "\n".join(self.path.read_text(errors="replace").splitlines()[-lines:])
