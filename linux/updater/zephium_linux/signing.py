"""Checks a download against upstream's minisign key, the one its updater trusts.

Tauri signs with minisign's prehashed mode ("ED"): Ed25519 over the file's
BLAKE2b-512 digest, plus a global signature over the signature and its trusted
comment. OpenSSL does the Ed25519 part, so nothing new needs installing.
"""

import base64
import hashlib
import subprocess
import tempfile
from pathlib import Path

# DER prefix of an Ed25519 SubjectPublicKeyInfo; the raw 32-byte key follows.
ED25519_SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")


class BadSignature(Exception):
    pass


def verify(data: bytes, tauri_signature: str, tauri_public_key: str) -> str:
    """Return the trusted comment when `data` carries a good signature."""
    key_id, public_key = _public_key(tauri_public_key)
    lines = base64.b64decode(tauri_signature).decode().splitlines()
    if len(lines) < 4 or not lines[2].startswith("trusted comment: "):
        raise BadSignature("not a minisign signature")
    raw = base64.b64decode(lines[1])
    algorithm, signed_by, signature = raw[:2], raw[2:10], raw[10:]
    if signed_by != key_id:
        raise BadSignature("signed with a different key")
    if algorithm == b"ED":
        message = hashlib.blake2b(data, digest_size=64).digest()
    elif algorithm == b"Ed":
        message = data
    else:
        raise BadSignature(f"unknown signature algorithm {algorithm!r}")
    _ed25519(public_key, message, signature)

    trusted_comment = lines[2].removeprefix("trusted comment: ")
    _ed25519(public_key, signature + trusted_comment.encode(), base64.b64decode(lines[3]))
    return trusted_comment


def _public_key(tauri_public_key: str) -> tuple[bytes, bytes]:
    raw = base64.b64decode(base64.b64decode(tauri_public_key).decode().splitlines()[1])
    if raw[:2] != b"Ed" or len(raw) != 42:
        raise BadSignature("not a minisign Ed25519 public key")
    return raw[2:10], raw[10:]


def _ed25519(public_key: bytes, message: bytes, signature: bytes) -> None:
    with tempfile.TemporaryDirectory() as scratch:
        directory = Path(scratch)
        (directory / "key.der").write_bytes(ED25519_SPKI_PREFIX + public_key)
        (directory / "message").write_bytes(message)
        (directory / "signature").write_bytes(signature)
        checked = subprocess.run(
            ["openssl", "pkeyutl", "-verify", "-pubin", "-keyform", "DER",
             "-inkey", "key.der", "-rawin", "-in", "message", "-sigfile", "signature"],
            cwd=directory,
            capture_output=True,
        )
    if checked.returncode != 0:
        raise BadSignature("signature does not match")
