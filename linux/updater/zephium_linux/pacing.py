"""Uploads that leave room on a small home uplink."""

import time

CHUNK = 64 * 1024


def paced(source, kilobytes_per_second: int):
    """Yield `source` (a binary file) in chunks no faster than the given rate."""
    started, sent = time.monotonic(), 0
    while chunk := source.read(CHUNK):
        yield chunk
        sent += len(chunk)
        ahead = sent / (kilobytes_per_second * 1024) - (time.monotonic() - started)
        if ahead > 0:
            time.sleep(ahead)
