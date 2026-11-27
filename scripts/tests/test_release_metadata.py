"""Tests for the update manifest and envelope stages of scripts/release-metadata.py.

Bundles, archives, provenance records and signer outputs are synthetic; nothing
is signed, notarized, stored in a keychain or sent anywhere. Signatures are made
and verified by `broscribe update sign-manifest` (Rust, tested there); here they
are placeholders the envelope stage checks only for shape and agreement.
"""

from __future__ import annotations

import hashlib
import json
import tempfile
import unittest
import zipfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

from support import load_script
from test_release_artifacts import DEV_ID, make_bundle, quiet, write_zip_entry

metadata = load_script("release-metadata.py")
verify = load_script("verify-macos-artifact.py")

NAME = "BroScribe-0.2.0-12-macos-arm64.zip"
URL = f"https://updates.example/stable/{NAME}"
NOW = datetime(2026, 11, 17, 10, 0, 0, 123456, tzinfo=timezone.utc)
SIGNING = {"authorities": [DEV_ID, "Developer ID Certification Authority"],
           "team_identifier": "SYNTH12345", "designated_requirement": "identifier x"}
# The fields of the updater's schema 1 manifest (crates/broscribe-updates/src/manifest.rs).
MANIFEST_FIELDS = {"format", "schema", "product", "channel", "version", "build", "published_at",
                   "expires_at", "artifacts"}
ARTIFACT_FIELDS = {"target", "app_id", "url", "size", "sha256", "archive", "root", "entries",
                   "installed_size"}
PUBLIC_A = "11" * 32
PUBLIC_B = "22" * 32


def zip_app(app: Path, artifact: Path, extra: tuple[tuple[str, bytes], ...] = ()) -> Path:
    with zipfile.ZipFile(artifact, "w", zipfile.ZIP_DEFLATED) as archive:
        write_zip_entry(archive, "BroScribe.app/", b"")
        for path in sorted(app.rglob("*")):
            if path.is_file():
                write_zip_entry(archive, (Path("BroScribe.app") / path.relative_to(app)).as_posix(),
                                path.read_bytes())
        for name, data in extra:
            write_zip_entry(archive, name, data)
    return artifact


def signer(public_key: str, label: str = "release-a") -> dict:
    return {"label": label, "key_id": hashlib.sha256(bytes.fromhex(public_key)).hexdigest()[:16],
            "public_key": public_key}


def signer_output(manifest_text: str, signers: list[dict]) -> dict:
    """The shape `broscribe --json update sign-manifest` prints."""
    envelope = {"format": "broscribe-update-envelope", "schema": 1, "manifest": manifest_text,
                "signatures": [{"key_id": s["key_id"], "ed25519": "ab" * 64} for s in signers]}
    return {"envelope": json.dumps(envelope), "signers": signers}


class ManifestTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.dist = self.root / "dist"
        self.dist.mkdir()
        self.app = make_bundle(self.root)
        self.artifact = zip_app(self.app, self.dist / NAME)

    def record(self, **changes) -> dict:
        record = metadata.provenance(self.artifact, self.app, SIGNING, "synthetic-id", "Accepted", self.dist)
        record.update(changes)
        return record

    def manifest(self, record=None, url=URL, channel="stable", days=30) -> dict:
        return metadata.release_manifest(self.artifact, record or self.record(), url, channel, days, NOW)

    def refused(self, message: str, **kwargs) -> None:
        with self.assertRaises(metadata.MetadataError) as caught:
            self.manifest(**kwargs)
        self.assertIn(message, str(caught.exception))

    def test_manifest_describes_the_stapled_artifact_in_the_updater_schema(self):
        manifest = self.manifest()
        self.assertEqual(set(manifest), MANIFEST_FIELDS)
        self.assertEqual(len(manifest["artifacts"]), 1)
        artifact = manifest["artifacts"][0]
        self.assertEqual(set(artifact), ARTIFACT_FIELDS)
        with zipfile.ZipFile(self.artifact) as archive:
            problems, entries, installed = verify.archive_layout(archive)
        self.assertEqual(problems, [])
        self.assertEqual(
            (manifest["format"], manifest["schema"], manifest["product"], manifest["channel"],
             manifest["version"], manifest["build"]),
            ("broscribe-release-manifest", 1, "BroScribe", "stable", "0.2.0", 12))
        self.assertEqual(manifest["published_at"], "2026-11-17T10:00:00Z")
        self.assertEqual(manifest["expires_at"], "2026-12-17T10:00:00Z")
        self.assertEqual(artifact, {
            "target": "aarch64-apple-darwin", "app_id": "org.broscribe.BroScribe", "url": URL,
            "size": self.artifact.stat().st_size,
            "sha256": hashlib.sha256(self.artifact.read_bytes()).hexdigest(),
            "archive": "zip", "root": "BroScribe.app", "entries": entries, "installed_size": installed})

    def test_cli_writes_the_manifest_from_the_provenance_file(self):
        provenance = self.dist / "record.provenance.json"
        provenance.write_text(json.dumps(self.record()))
        out = self.root / "manifest.json"
        code, err = quiet(metadata.main, ["manifest", "--artifact", str(self.artifact), "--provenance",
                                          str(provenance), "--url", URL, "--channel", "beta",
                                          "--expires-days", "7", "--out", str(out)])
        self.assertEqual(code, 0, err)
        manifest = json.loads(out.read_bytes())
        self.assertEqual(manifest["channel"], "beta")
        published = datetime.fromisoformat(manifest["published_at"].replace("Z", "+00:00"))
        expires = datetime.fromisoformat(manifest["expires_at"].replace("Z", "+00:00"))
        self.assertEqual(expires - published, timedelta(days=7))

    def test_parser_errors_are_reported(self):
        provenance = self.root / "broken.provenance.json"
        provenance.write_text("{not json")
        code, err = quiet(metadata.main, ["manifest", "--artifact", str(self.artifact), "--provenance",
                                          str(provenance), "--url", URL, "--channel", "stable",
                                          "--expires-days", "30", "--out", str(self.root / "m.json")])
        self.assertEqual(code, 1)
        self.assertIn("release-metadata manifest: Expecting property name", err)
        self.assertFalse((self.root / "m.json").exists())

    def test_only_a_notarized_stapled_developer_id_artifact_gets_a_manifest(self):
        record = self.record()
        self.refused("not notarized and stapled", record=record | {"notarization": {
            "submission_id": "synthetic-id", "status": "Accepted", "stapled": False}})
        self.refused("not notarized and stapled", record=record | {"notarization": {
            "submission_id": "synthetic-id", "status": "Invalid", "stapled": True}})
        self.refused("Developer ID", record=record | {"signing": {"authorities": ["Apple Development: X"]}})
        self.refused("not schema 1", record=record | {"schema": 2})

    def test_the_artifact_must_still_match_its_provenance(self):
        record = self.record()
        self.refused("describes", record=record | {"artifact": record["artifact"] | {"name": "Other.zip"}})
        self.refused("entry count", record=record | {"artifact": record["artifact"] | {"entries": 1}})
        with self.artifact.open("ab") as handle:
            handle.write(b"tampered")
        self.refused("size or SHA-256", record=record)

    def test_a_zip_the_updater_would_refuse_gets_no_manifest(self):
        for extra in (
            (("BroScribe.app/Contents/._Info.plist", b"x"),),
            (("Other.app/x", b"x"),),
        ):
            with self.subTest(extra=extra):
                zip_app(self.app, self.artifact, extra)
                with self.assertRaises(metadata.MetadataError):
                    self.manifest(record=self.record())

    def test_release_fields_follow_the_updater_rules(self):
        record = self.record()
        for url, message in (("http://updates.example/" + NAME, "https"),
                             ("https://user@updates.example/" + NAME, "credentials"),
                             (URL + "#x", "fragment"),
                             ("https://updates.example/other.zip", f"end in /{NAME}"),
                             ("https://updates.example/a b/" + NAME, "whitespace")):
            with self.subTest(url=url):
                self.refused(message, record=record, url=url)
        self.refused("channel", record=record, channel="Stable")
        self.refused("expire", record=record, days=0)
        self.refused("expire", record=record, days=366)
        self.refused("MAJOR.MINOR.PATCH", record=record | {"version": "0.02.0"})
        self.refused("positive integer", record=record | {"build": True})


class EnvelopeTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.text = json.dumps({"format": "broscribe-release-manifest", "build": 12}, indent=2) + "\n"

    def refused(self, signed, message: str, trusted=(PUBLIC_A,)) -> None:
        with self.assertRaises(metadata.MetadataError) as caught:
            metadata.release_envelope(signed, self.text, list(trusted))
        self.assertIn(message, str(caught.exception))

    def test_the_signed_envelope_is_written_exactly(self):
        signed = signer_output(self.text, [signer(PUBLIC_A)])
        manifest, signed_path, out = self.root / "m.json", self.root / "signed.json", self.root / "e.json"
        manifest.write_bytes(self.text.encode())
        signed_path.write_text(json.dumps(signed, indent=2))
        code, err = quiet(metadata.main, ["envelope", "--manifest", str(manifest), "--signed", str(signed_path),
                                          "--trusted-key", PUBLIC_B, "--trusted-key", PUBLIC_A,
                                          "--out", str(out)])
        self.assertEqual(code, 0, err)
        self.assertEqual(out.read_bytes(), signed["envelope"].encode())

    def test_unsigned_or_tampered_envelopes_are_refused(self):
        signed = signer_output(self.text, [signer(PUBLIC_A)])
        envelope = json.loads(signed["envelope"])
        self.refused(signed | {"envelope": json.dumps(envelope | {"signatures": []})}, "unsigned")
        self.refused(signed | {"envelope": json.dumps(envelope | {"manifest": self.text.rstrip()})},
                     "carry this manifest exactly")
        self.refused(signed_output_with(envelope, ed25519="AB" * 64, signers=signed["signers"]), "128 hex")
        self.refused(signed | {"envelope": json.dumps(envelope | {"extra": 1})}, "exactly format")
        self.refused(signed | {"envelope": "{"}, "envelope: Expecting property name")

    def test_signers_must_match_the_signatures_and_include_a_pinned_key(self):
        self.refused(signer_output(self.text, [signer(PUBLIC_B)]), "pins", trusted=(PUBLIC_A,))
        self.refused(signer_output(self.text, [signer(PUBLIC_A)]), "--trusted-key", trusted=())
        self.refused(signer_output(self.text, [signer(PUBLIC_A)]), "64 lowercase", trusted=("AB" * 32,))
        signed = signer_output(self.text, [signer(PUBLIC_A)])
        self.refused(signed | {"signers": [signer(PUBLIC_A) | {"key_id": "0" * 16}]}, "matching key ID")
        self.refused(signed | {"signers": [signer(PUBLIC_A), signer(PUBLIC_B, "release-b")]}, "differ")
        self.refused({"envelope": signed["envelope"]}, "{envelope, signers}")
        overlap = signer_output(self.text, [signer(PUBLIC_B, "new"), signer(PUBLIC_A, "old")])
        self.assertEqual(metadata.release_envelope(overlap, self.text, [PUBLIC_A]), overlap["envelope"])

    def test_a_failed_signing_run_is_reported_and_writes_nothing(self):
        manifest, signed_path, out = self.root / "m.json", self.root / "signed.json", self.root / "e.json"
        manifest.write_bytes(self.text.encode())
        signed_path.write_text(json.dumps({"error": {
            "code": "update_signing_key_invalid",
            "message": "the secret store entry for release key release-a is not a release key"}}))
        code, err = quiet(metadata.main, ["envelope", "--manifest", str(manifest), "--signed", str(signed_path),
                                          "--trusted-key", PUBLIC_A, "--out", str(out)])
        self.assertEqual(code, 1)
        self.assertIn("signing failed: update_signing_key_invalid", err)
        self.assertFalse(out.exists())


def signed_output_with(envelope: dict, ed25519: str, signers: list[dict]) -> dict:
    changed = envelope | {"signatures": [s | {"ed25519": ed25519} for s in envelope["signatures"]]}
    return {"envelope": json.dumps(changed), "signers": signers}


if __name__ == "__main__":
    unittest.main()
