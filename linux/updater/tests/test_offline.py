"""Checks that run without network, build or display: python3 -m unittest discover updater/tests"""

import base64
import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from zephium_linux import publish, repair, signing, smoke, source, upstream  # noqa: E402
from zephium_linux.log import Log  # noqa: E402


def release(*names, manifest=None, sums=""):
    assets = {name: f"https://example.invalid/{name}" for name in names}
    downloads = {}
    if manifest is not None:
        assets["latest.json"] = "https://example.invalid/latest.json"
        downloads[assets["latest.json"]] = json.dumps({"platforms": manifest}).encode()
    if sums:
        assets["SHA256SUMS"] = "https://example.invalid/SHA256SUMS"
        downloads[assets["SHA256SUMS"]] = sums.encode()
    for name in names:
        if name.endswith(".sig"):
            downloads[assets[name]] = b"c2lnbmF0dXJl"
    return upstream.Release(tag="v9.9.9", version="9.9.9", assets=assets), downloads


class OfficialLinuxBuild(unittest.TestCase):
    def find(self, *names, **extra):
        found, downloads = release(*names, **extra)
        with mock.patch.object(upstream, "fetch", side_effect=lambda url, **_: downloads[url]):
            return upstream.official_linux_build(found)

    def test_todays_release_has_none(self):
        self.assertIsNone(self.find("Zephium-macOS-arm64.dmg", "Zephium-Windows-x64-setup.exe",
                                    manifest={"windows-x86_64": {"url": "x/Zephium-Windows-x64-setup.exe"}}))

    def test_manifest_prefers_the_deb(self):
        build = self.find(manifest={
            "linux-x86_64": {"url": "https://x/Zephium.AppImage", "signature": "a"},
            "linux-x86_64-deb": {"url": "https://x/Zephium_amd64.deb", "signature": "b"},
        })
        self.assertEqual((build.name, build.signature), ("Zephium_amd64.deb", "b"))

    def test_assets_without_manifest(self):
        build = self.find("Zephium-Linux-arm64.deb", "Zephium-Linux-x64.AppImage", "Zephium-Linux-x64.deb",
                          "Zephium-Linux-x64.deb.sig",
                          sums="abc123  Zephium-Linux-x64.deb\nfff  Zephium-Linux-x64.AppImage\n")
        self.assertEqual(build.name, "Zephium-Linux-x64.deb")
        self.assertEqual(build.sha256, "abc123")
        self.assertEqual(build.signature, "c2lnbmF0dXJl")

    def test_other_architectures_are_ignored(self):
        self.assertIsNone(self.find("Zephium-Linux-aarch64.AppImage", "Zephium_arm64.deb"))


class Minisign(unittest.TestCase):
    """A throwaway key in the format Tauri writes, signed through OpenSSL."""

    def setUp(self):
        self.scratch = Path(tempfile.mkdtemp())
        subprocess.run(["openssl", "genpkey", "-algorithm", "ed25519", "-out", self.scratch / "key.pem"],
                       check=True, capture_output=True)
        der = subprocess.run(["openssl", "pkey", "-in", self.scratch / "key.pem", "-pubout", "-outform", "DER"],
                             check=True, capture_output=True).stdout
        self.key_id = b"\x01\x02\x03\x04\x05\x06\x07\x08"
        line = base64.b64encode(b"Ed" + self.key_id + der[-32:]).decode()
        self.public_key = base64.b64encode(f"untrusted comment: test key\n{line}\n".encode()).decode()

    def sign(self, message: bytes) -> bytes:
        (self.scratch / "message").write_bytes(message)
        return subprocess.run(["openssl", "pkeyutl", "-sign", "-inkey", self.scratch / "key.pem", "-rawin",
                               "-in", self.scratch / "message"], check=True, capture_output=True).stdout

    def signature_for(self, data: bytes, comment: str = "timestamp:1\tfile:Zephium.deb") -> str:
        signature = self.sign(hashlib.blake2b(data, digest_size=64).digest())
        global_signature = self.sign(signature + comment.encode())
        text = (f"untrusted comment: signature from tauri secret key\n"
                f"{base64.b64encode(b'ED' + self.key_id + signature).decode()}\n"
                f"trusted comment: {comment}\n{base64.b64encode(global_signature).decode()}\n")
        return base64.b64encode(text.encode()).decode()

    def test_good_signature(self):
        self.assertIn("Zephium.deb", signing.verify(b"payload", self.signature_for(b"payload"), self.public_key))

    def test_changed_file_is_refused(self):
        with self.assertRaises(signing.BadSignature):
            signing.verify(b"payloaD", self.signature_for(b"payload"), self.public_key)

    def test_changed_trusted_comment_is_refused(self):
        lines = base64.b64decode(self.signature_for(b"payload")).decode().splitlines()
        lines[2] = "trusted comment: timestamp:1\tfile:Evil.deb"
        forged = base64.b64encode(("\n".join(lines) + "\n").encode()).decode()
        with self.assertRaises(signing.BadSignature):
            signing.verify(b"payload", forged, self.public_key)


class ScreenColours(unittest.TestCase):
    def colours(self, pixels: bytes, width: int) -> int:
        image = f"P6\n{width} 1\n255\n".encode() + pixels
        with mock.patch.object(smoke.subprocess, "run", return_value=mock.Mock(stdout=image)):
            return smoke._colours_on_screen(Path("/nonexistent"))

    def test_flat_screen(self):
        self.assertEqual(self.colours(b"\x30\x34\x3c" * 700, 700), 1)

    def test_pixels_that_start_with_whitespace_bytes(self):
        self.assertEqual(self.colours(b"\x20\x0a\x09" * 7 + b"\x01\x02\x03" * 7, 14), 2)


class RepairedCommitsBelongToTheOwner(unittest.TestCase):
    def test_trailers_are_dropped_and_authorship_reset(self):
        tree = Path(tempfile.mkdtemp())
        git = lambda *args: subprocess.run(["git", "-C", tree, *args], check=True, capture_output=True,
                                           text=True).stdout.strip()
        git("init", "-q")
        git("config", "user.name", "Owner")
        git("config", "user.email", "owner@example.invalid")
        (tree / "file").write_text("base\n")
        git("add", "file")
        git("commit", "-q", "-m", "base")
        base = git("rev-parse", "HEAD")
        (tree / "file").write_text("changed\n")
        git("commit", "-q", "-a", "--author", "Someone Else <else@example.invalid>", "-m",
            "fix(linux): keep the change\n\nWhy it matters.\n\nCo-Authored-By: Claude <noreply@anthropic.com>\n"
            "Claude-Session: https://example.invalid/session")
        with mock.patch.object(repair, "TREE", tree):
            repair._own_the_commits(source.Target(ref="t", commit=base, version="0"),
                                    Log(Path(tempfile.mkdtemp()) / "log"))
        self.assertEqual(git("log", "-1", "--format=%an <%ae>"), "Owner <owner@example.invalid>")
        self.assertEqual(git("log", "-1", "--format=%B"), "fix(linux): keep the change\n\nWhy it matters.")
        self.assertEqual((tree / "file").read_text(), "changed\n")


class ReleaseTags(unittest.TestCase):
    def next_tag(self, listing: str) -> str:
        log = mock.Mock()
        log.run.return_value = listing
        return publish._next_tag("1.0.3", log)

    def test_first_build_of_a_version(self):
        self.assertEqual(self.next_tag(""), "v1.0.3-linux.1")

    def test_counts_past_annotated_tags(self):
        listing = ("a\trefs/tags/v1.0.3-linux.1\nb\trefs/tags/v1.0.3-linux.1^{}\n"
                   "c\trefs/tags/v1.0.3-linux.2\nd\trefs/tags/v1.0.3-linux.2^{}\n")
        self.assertEqual(self.next_tag(listing), "v1.0.3-linux.3")


if __name__ == "__main__":
    unittest.main()
