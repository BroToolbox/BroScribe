#!/usr/bin/env python3
"""Archive an ad-hoc macOS build for CI preview with explicit unsigned provenance."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile

from desktop_artifacts import APP_ID, REPO, json_bytes, json_object, sha256_file, sibling

metadata = sibling("release-metadata")


def main() -> None:
    target = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target")).resolve()
    app = target / "BroScribe.app"
    resources = app / "Contents/Resources"
    inventory = resources / "third-party-inventory.json"
    build = json_object((resources / "build-info.json").read_bytes())
    codesign = subprocess.run(["codesign", "--display", "--verbose=4", str(app)],
                              check=True, capture_output=True, text=True).stderr
    if "Signature=adhoc" not in codesign.splitlines() or build.get("git", {}).get("dirty") is not False:
        raise ValueError("preview packaging requires a clean ad-hoc build")
    dist = target / "dist"
    dist.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".broscribe-preview-", dir=dist) as tmp:
        work = Path(tmp)
        name = f"BroScribe-{build['version']}-{build['build']}-macos-arm64-adhoc"
        artifact = work / (name + ".zip")
        sidecar = work / (artifact.name + ".sha256")
        provenance = work / (name + ".provenance.json")
        subprocess.run(["ditto", "-c", "-k", "--norsrc", "--noextattr", "--noqtn", "--noacl",
                        "--keepParent", str(app), str(artifact)], check=True)
        checksum = sha256_file(artifact)
        record = {
            "schema": 1,
            "artifact": {"name": artifact.name, "sha256": checksum, "size": artifact.stat().st_size,
                         **metadata.archive_counts(artifact)},
            **{key: build[key] for key in ("product", "version", "build", "target", "git", "toolchain",
                                           "cargo_lock_sha256", "sherpa_onnx")},
            "bundle_identifier": APP_ID,
            "third_party_inventory_sha256": sha256_file(inventory),
            "signing": {"kind": "ad-hoc"},
            "notarization": {"status": "Not submitted", "stapled": False},
        }
        provenance.write_bytes(json_bytes(record))
        sidecar.write_bytes(f"{checksum}  {artifact.name}\n".encode())
        subprocess.run([sys.executable, str(REPO / "scripts/verify-macos-artifact.py"), str(artifact),
                        "--provenance", str(provenance), "--require-complete-licences", "--codesign"],
                       check=True)
        for path in (sidecar, provenance, artifact):
            path.replace(dist / path.name)


if __name__ == "__main__":
    os.umask(0o022)
    try:
        main()
    except (KeyError, OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"package-macos-preview: {error}", file=sys.stderr)
        raise SystemExit(1)
