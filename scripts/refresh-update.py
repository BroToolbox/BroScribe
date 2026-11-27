#!/usr/bin/env python3
"""Qualify a published release's asset set and rebuild its stable update manifest."""

import argparse
from datetime import datetime, timezone
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile

spec = importlib.util.spec_from_file_location("release_metadata", Path(__file__).with_name("release-metadata.py"))
metadata = importlib.util.module_from_spec(spec)
spec.loader.exec_module(metadata)
ENVELOPE = "broscribe-update.json"


def release_assets(release: dict) -> tuple[str, dict[str, tuple[str, str, str]]]:
    tag = release["tag_name"]
    if not tag.startswith("v") or not metadata.RELEASE_VERSION.fullmatch(tag[1:]):
        raise ValueError("expected a vMAJOR.MINOR.PATCH release")
    if release["draft"] is not False or release["prerelease"] is not False or not release["published_at"]:
        raise ValueError("expected a published stable release")
    assets = [asset["name"] for asset in release["assets"]]
    if len(set(assets)) != len(assets):
        raise ValueError("release contains duplicate asset names")
    packages, builds = {}, set()
    for label, (_, archive) in {"macos-arm64": ("aarch64-apple-darwin", "zip"),
                                **metadata.DESKTOP_TARGETS}.items():
        prefix = f"BroScribe-{tag[1:]}-"
        candidates = []
        for name in assets:
            if not name.startswith(prefix):
                continue
            build, _, suffix = name[len(prefix):].partition("-")
            if build.isascii() and build.isdecimal() and not build.startswith("0"):
                if suffix == f"{label}.{archive}":
                    candidates.append((label, name, build))
                elif label == "macos-arm64" and suffix == f"{label}-adhoc.{archive}":
                    candidates.append((label + "-adhoc", name, build))
        if len(candidates) != 1:
            raise ValueError(f"expected exactly one {label} release archive")
        folder, name, build = candidates[0]
        builds.add(build)
        packages[folder] = (name, name + ".sha256",
                            name.removesuffix(f".{archive}") + ".provenance.json")
    expected = {name for names in packages.values() for name in names}
    if set(assets) - {ENVELOPE} != expected or len(builds) != 1:
        raise ValueError("release must have exactly four packages of the same build with both sidecars")
    return tag, packages


def refresh_manifest(release: dict, downloads: Path, packages: Path, now: datetime) -> dict:
    tag, layout = release_assets(release)
    expected = {asset["name"] for asset in release["assets"]}
    if {path.name for path in downloads.iterdir()} != expected:
        raise ValueError("downloaded assets do not match the published release")
    for folder, names in layout.items():
        target = packages / folder
        target.mkdir(parents=True)
        for name in names:
            (downloads / name).rename(target / name)
    return metadata.combined_manifest(packages, tag, now, allow_legacy_layout=True)


def require_baseline(release: dict, event: str, allow_missing: bool) -> bool:
    if any(asset["name"] == ENVELOPE for asset in release["assets"]):
        return True
    if event != "workflow_dispatch" or not allow_missing:
        raise ValueError("no update envelope to authenticate; use the manual bootstrap input only after verifying the release")
    print("::warning::Explicit manual bootstrap: signing a release without an authenticated baseline.",
          file=sys.stderr)
    return False


def verify(cli: Path, envelope: bytes, *, allow_expired: bool = False) -> None:
    command = [str(cli), "update", "verify-manifest", "--envelope-stdin"]
    if allow_expired:
        command.append("--allow-expired")
    subprocess.run(command, input=envelope, stdout=subprocess.DEVNULL, check=True)


def authenticate_baseline(path: Path, cli: Path) -> dict:
    envelope = path.read_bytes()
    verify(cli, envelope, allow_expired=True)
    return json.loads(json.loads(envelope)["manifest"])


def check_previous(old: dict, fresh: dict) -> None:
    """Only the two timestamps may change when replacing an existing envelope."""
    timestamps = {"published_at", "expires_at"}
    if {k: v for k, v in old.items() if k not in timestamps} != {
        k: v for k, v in fresh.items() if k not in timestamps
    }:
        raise ValueError("the existing update envelope describes different release artifacts")


def publish_envelope(tag: str, cli: Path, signed: Path, backup: Path | None) -> None:
    def replace(path: Path, *, allow_expired: bool = False) -> None:
        expected = path.read_bytes()
        subprocess.run(["gh", "release", "upload", tag, str(path), "--clobber"],
                       timeout=120, check=True)
        with tempfile.TemporaryDirectory() as download:
            subprocess.run(["gh", "release", "download", tag, "--pattern", ENVELOPE,
                            "--dir", download], timeout=120, check=True)
            public = (Path(download) / ENVELOPE).read_bytes()
        verify(cli, public, allow_expired=allow_expired)
        if public != expected:
            raise ValueError("downloaded update envelope differs from the uploaded bytes")

    signed_bytes = signed.read_bytes()
    verify(cli, signed_bytes)
    if backup is not None:
        old = authenticate_baseline(backup, cli)
        check_previous(old, json.loads(json.loads(signed_bytes)["manifest"]))
    try:
        replace(signed)
    except (subprocess.SubprocessError, OSError, ValueError):
        if backup is not None:
            print("::warning::Envelope replacement failed. Restoring the authenticated baseline.",
                  file=sys.stderr)
            try:
                replace(backup, allow_expired=True)
            except (subprocess.SubprocessError, OSError, ValueError) as error:
                print(f"::error::Could not restore the update envelope: {error}", file=sys.stderr)
        raise


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("release", type=Path)
    parser.add_argument("--downloads", type=Path)
    parser.add_argument("--packages", type=Path)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--cli", type=Path)
    parser.add_argument("--event", default="schedule")
    parser.add_argument("--allow-missing-baseline", action="store_true")
    parser.add_argument("--publish", type=Path)
    args = parser.parse_args()
    if args.publish is not None:
        if args.cli is None or args.downloads is None or args.packages or args.out:
            parser.error("--publish requires --cli and --downloads")
    elif any((args.downloads, args.packages, args.out, args.cli)) and not all(
        (args.downloads, args.packages, args.out, args.cli)
    ):
        parser.error("--downloads, --packages, --out and --cli must be supplied together")
    try:
        release = json.loads(args.release.read_bytes())
        has_baseline = require_baseline(release, args.event, args.allow_missing_baseline)
        tag, _ = release_assets(release)
        if args.publish is not None:
            backup = args.downloads / ENVELOPE if has_baseline else None
            publish_envelope(tag, args.cli, args.publish, backup)
        elif args.downloads is None:
            for asset in release["assets"]:
                print(asset["name"])
        else:
            old = authenticate_baseline(args.downloads / ENVELOPE, args.cli) if has_baseline else None
            manifest = refresh_manifest(release, args.downloads, args.packages, datetime.now(timezone.utc))
            if old is not None:
                check_previous(old, manifest)
            args.out.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    except (metadata.MetadataError, ValueError, OSError, KeyError, TypeError,
            subprocess.SubprocessError) as error:
        print(f"refresh-update: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
