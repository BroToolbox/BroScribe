#!/usr/bin/env python3
"""Release metadata for verified BroScribe artifacts.

Subcommands (all offline; none reads credentials):

  icon --out DIR.iconset
      Copy BroScribe's app icon (the mascot's tile, rendered from the design
      system into crates/broscribe-app/assets/brand/AppIcon.iconset) as the
      PNG sizes `iconutil -c icns` expects.

  build-info --out FILE --version V --build N --target T --git-commit C
             --git-dirty true|false --rustc S --cargo S --cargo-lock FILE
             --sherpa-archive A --sherpa-sha256 H --profile P --copyright S
      Validate and write Contents/Resources/build-info.json. N, the
      CFBundleVersion, is an integer from 1 to 999999999 (at most 9 digits).

  notary-log --in RAW.json --out SANITIZED.json [--redact TEXT ...]
      Keep only the diagnostic fields of an Apple notarization log and redact
      the given strings (builder paths, user names) and e-mail addresses.

  provenance --artifact ZIP --app APP --signing-json FILE --submission-id ID
             --notary-status S [--dist-dir DIR]
      Write <artifact-stem>.provenance.json and <artifact>.sha256 beside the
      artifact, refusing a build number that does not increase over another
      commit's provenance already in the dist folder.

  manifest --artifact ZIP --provenance FILE --url URL --channel C
           --expires-days N --out FILE
      Write the updater's release manifest (schema 1, unsigned) for a
      notarized, stapled artifact whose provenance it re-checks: size,
      SHA-256, updater archive layout, entry count and installed size.

  manifest --packages DIR --tag vMAJOR.MINOR.PATCH [--expires-days N] --out FILE
      Write one stable manifest for the four verified GitHub release packages.
      Ad-hoc macOS previews are omitted. Expiry defaults to 90 days.

  envelope --manifest FILE --signed FILE --trusted-key HEX [...] --out FILE
      Check the output of `broscribe --json update sign-manifest` (which
      signs with keys from the platform secret store and verifies the
      envelope before printing it) against the manifest and the public keys
      clients pin, then write the envelope exactly as signed.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import plistlib
import re
import struct
import sys
import tarfile
import zipfile
from datetime import datetime, timedelta, timezone
from pathlib import Path


class MetadataError(Exception):
    pass


# ------------------------------------------------------------------- icon

ICONSET = {
    "icon_16x16.png": 16, "icon_16x16@2x.png": 32,
    "icon_32x32.png": 32, "icon_32x32@2x.png": 64,
    "icon_128x128.png": 128, "icon_128x128@2x.png": 256,
    "icon_256x256.png": 256, "icon_256x256@2x.png": 512,
    "icon_512x512.png": 512, "icon_512x512@2x.png": 1024,
}
# The mascot's app-icon tile on Apple's icon grid, rendered from the design
# system's source by `cargo run -p broscribe-app --example render_brand`.
ICON_SOURCE = Path(__file__).resolve().parent.parent / "crates" / "broscribe-app" / "assets" / "brand" / "AppIcon.iconset"


def png_size(data: bytes) -> tuple[int, int]:
    """Width and height of an RGBA PNG; refuses anything else."""
    if data[:8] != b"\x89PNG\r\n\x1a\n" or data[12:16] != b"IHDR":
        raise MetadataError("not a PNG")
    width, height, depth, colour = struct.unpack(">IIBB", data[16:26])
    if (depth, colour) != (8, 6):
        raise MetadataError("the icon must be 8-bit RGBA")
    return width, height


def write_iconset(out: Path, source: Path = ICON_SOURCE) -> None:
    """Copy the committed icon PNGs into `out`, checking every size
    `iconutil -c icns` expects is there at its pixel size."""
    if out.suffix != ".iconset":
        raise MetadataError("the icon folder must end in .iconset (iconutil requires it)")
    images = {}
    for name, size in ICONSET.items():
        path = source / name
        try:
            data = path.read_bytes()
        except OSError:
            raise MetadataError(f"{name} is missing from {source.name}; run the render_brand example") from None
        if png_size(data) != (size, size):
            raise MetadataError(f"{name} is not {size}x{size} pixels")
        images[name] = data
    out.mkdir(parents=True, exist_ok=True)
    for name, data in images.items():
        (out / name).write_bytes(data)


# ------------------------------------------------------------- build info

VERSION = re.compile(r"\d+\.\d+\.\d+")
COMMIT = re.compile(r"[0-9a-f]{40}")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def build_info(args) -> dict:
    if not VERSION.fullmatch(args.version):
        raise MetadataError(f"marketing version must be MAJOR.MINOR.PATCH, got {args.version!r}")
    if not re.fullmatch(r"[1-9][0-9]{0,8}", args.build):
        raise MetadataError(f"build number must be an integer from 1 to 999999999 (at most 9 digits), got {args.build!r}")
    if not COMMIT.fullmatch(args.git_commit):
        raise MetadataError(f"git commit must be a full SHA-1, got {args.git_commit!r}")
    if args.git_dirty not in ("true", "false"):
        raise MetadataError("--git-dirty must be true or false")
    if not re.fullmatch(r"[0-9a-f]{64}", args.sherpa_sha256):
        raise MetadataError("--sherpa-sha256 must be a SHA-256")
    if not args.copyright.startswith("Copyright "):
        raise MetadataError("copyright line must start with 'Copyright '")
    return {
        "schema": 1,
        "product": "BroScribe",
        "version": args.version,
        "build": int(args.build),
        "target": args.target,
        "profile": args.profile,
        "git": {"commit": args.git_commit, "dirty": args.git_dirty == "true"},
        "toolchain": {"rustc": args.rustc, "cargo": args.cargo},
        "cargo_lock_sha256": sha256_file(args.cargo_lock),
        "sherpa_onnx": {"archive": args.sherpa_archive, "sha256": args.sherpa_sha256},
        "copyright": args.copyright,
    }


# ------------------------------------------------------------ notary log

NOTARY_FIELDS = ("jobId", "status", "statusSummary", "statusCode", "archiveFilename", "uploadDate", "sha256")
ISSUE_FIELDS = ("severity", "code", "path", "message", "docUrl", "architecture")
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")


def sanitize_notary_log(raw: dict, redact: list[str]) -> dict:
    secrets = sorted({r for r in redact if r and len(r) >= 3}, key=len, reverse=True)

    def clean(value):
        if isinstance(value, str):
            for secret in secrets:
                value = value.replace(secret, "<redacted>")
            return EMAIL.sub("<redacted-email>", value)
        if isinstance(value, (int, float, bool)) or value is None:
            return value
        return clean(json.dumps(value, sort_keys=True))

    out = {key: clean(raw[key]) for key in NOTARY_FIELDS if key in raw}
    issues = raw.get("issues") or []
    out["issues"] = [{key: clean(issue[key]) for key in ISSUE_FIELDS if key in issue}
                     for issue in issues if isinstance(issue, dict)]
    return out


# ------------------------------------------------------------- provenance


def archive_counts(artifact: Path) -> dict:
    """The updater manifest's `entries` (every ZIP entry, folders included) and
    `installed_size` (file sizes plus symlink target lengths)."""
    with zipfile.ZipFile(artifact) as archive:
        infos = archive.infolist()
    return {"entries": len(infos), "installed_size": sum(i.file_size for i in infos if not i.is_dir())}


def read_plist(path: Path) -> dict:
    with path.open("rb") as handle:
        return plistlib.load(handle)


def provenance(artifact: Path, app: Path, signing: dict, submission_id: str, notary_status: str,
               dist_dir: Path) -> dict:
    if notary_status != "Accepted":
        raise MetadataError(f"notarization status is {notary_status!r}, not Accepted")
    info = read_plist(app / "Contents" / "Info.plist")
    build = json.loads((app / "Contents" / "Resources" / "build-info.json").read_text())
    inventory_path = app / "Contents" / "Resources" / "third-party-inventory.json"
    inventory = json.loads(inventory_path.read_text())
    if str(build["build"]) != info.get("CFBundleVersion") or build["version"] != info.get("CFBundleShortVersionString"):
        raise MetadataError("Info.plist version/build disagree with build-info.json")
    if build["git"]["dirty"]:
        raise MetadataError("refusing provenance for a build from a modified working tree")
    if not inventory.get("complete"):
        raise MetadataError("refusing provenance: the third-party licence inventory is incomplete")
    for other in sorted(dist_dir.glob("*.provenance.json")):
        previous = json.loads(other.read_text())
        if previous.get("git", {}).get("commit") != build["git"]["commit"] and previous.get("build", 0) >= build["build"]:
            raise MetadataError(f"build number {build['build']} does not increase over {other.name} "
                                f"(build {previous.get('build')})")
    return {
        "schema": 1,
        "artifact": {"name": artifact.name, "sha256": sha256_file(artifact), "size": artifact.stat().st_size,
                     **archive_counts(artifact)},
        "product": build["product"],
        "version": build["version"],
        "build": build["build"],
        "target": build["target"],
        "bundle_identifier": info.get("CFBundleIdentifier"),
        "git": build["git"],
        "toolchain": build["toolchain"],
        "cargo_lock_sha256": build["cargo_lock_sha256"],
        "sherpa_onnx": build["sherpa_onnx"],
        "third_party_inventory_sha256": sha256_file(inventory_path),
        "signing": {key: signing[key] for key in ("authorities", "team_identifier", "designated_requirement")},
        "notarization": {"submission_id": submission_id, "status": notary_status, "stapled": True},
    }


def write_provenance(args) -> None:
    signing = json.loads(args.signing_json.read_text())
    if not any(a.startswith("Developer ID Application:") for a in signing.get("authorities", [])):
        raise MetadataError("signing authority is not Developer ID Application")
    record = provenance(args.artifact, args.app, signing, args.submission_id, args.notary_status, args.dist_dir)
    stem = args.artifact.name.removesuffix(".zip")
    (args.dist_dir / f"{stem}.provenance.json").write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
    (args.dist_dir / f"{args.artifact.name}.sha256").write_text(f"{record['artifact']['sha256']}  {args.artifact.name}\n")


# -------------------------------------------------------- update manifest

MANIFEST_FORMAT = "broscribe-release-manifest"
ENVELOPE_FORMAT = "broscribe-update-envelope"
# The updater's version rule: MAJOR.MINOR.PATCH, no leading zeros, nothing else.
RELEASE_VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
CHANNEL = re.compile(r"[a-z0-9-]{1,32}")
TARGET = re.compile(r"[A-Za-z0-9_-]{1,64}")
LOWER_HEX = re.compile(r"[0-9a-f]*")
MAX_EXPIRY_DAYS = 365


def load_sibling(file_name: str):
    """scripts/<file_name> (hyphenated) as a module, imported once."""
    module_name = file_name.removesuffix(".py").replace("-", "_")
    if module_name not in sys.modules:
        spec = importlib.util.spec_from_file_location(module_name, Path(__file__).with_name(file_name))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        sys.modules[module_name] = module
    return sys.modules[module_name]


def is_hex(value, length: int) -> bool:
    return isinstance(value, str) and len(value) == length and LOWER_HEX.fullmatch(value) is not None


def check_artifact_url(url: str, artifact_name: str) -> None:
    """The updater's URL rule (plain https, no credentials or fragment), and the
    URL must name the artifact's own file."""
    if len(url) > 2048 or any(c.isspace() or ord(c) < 0x20 or 0x7F <= ord(c) <= 0x9F for c in url):
        raise MetadataError("the artifact URL is too long or has whitespace or control characters")
    if not url.startswith("https://"):
        raise MetadataError("the artifact URL must be https")
    if "#" in url:
        raise MetadataError("the artifact URL must not have a fragment")
    rest = url[len("https://"):]
    authority = re.split(r"[/?]", rest, maxsplit=1)[0]
    if not authority or authority.startswith(":") or "@" in authority or "\\" in authority:
        raise MetadataError("the artifact URL needs a host and no credentials")
    path = rest[len(authority):].split("?", 1)[0]
    if path.rsplit("/", 1)[-1] != artifact_name:
        raise MetadataError(f"the artifact URL must end in /{artifact_name}")


def rfc3339(moment: datetime) -> str:
    return moment.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def macos_update_artifact(artifact: Path, record: dict, url: str) -> dict:
    """An updater artifact for a notarized and stapled macOS package."""
    if not isinstance(record, dict) or record.get("schema") != 1:
        raise MetadataError("the provenance is not schema 1")
    notarization = record.get("notarization") or {}
    if notarization.get("status") != "Accepted" or notarization.get("stapled") is not True:
        raise MetadataError("refusing a manifest for an artifact that is not notarized and stapled")
    authorities = (record.get("signing") or {}).get("authorities") or []
    if not any(isinstance(a, str) and a.startswith("Developer ID Application:") for a in authorities):
        raise MetadataError("refusing a manifest for an artifact not signed by a Developer ID Application")
    recorded = record.get("artifact") or {}
    if recorded.get("name") != artifact.name:
        raise MetadataError(f"the provenance describes {recorded.get('name')!r}, not {artifact.name!r}")
    size, digest = artifact.stat().st_size, sha256_file(artifact)
    if (recorded.get("size"), recorded.get("sha256")) != (size, digest):
        raise MetadataError("the artifact's size or SHA-256 differs from its provenance")
    with zipfile.ZipFile(artifact) as archive:
        problems, entries, installed = load_sibling("verify-macos-artifact.py").archive_layout(archive)
    if problems:
        raise MetadataError("the artifact is not an updater archive: " + "; ".join(problems))
    if (recorded.get("entries"), recorded.get("installed_size")) != (entries, installed):
        raise MetadataError("the artifact's entry count or installed size differs from its provenance")
    return {
        "target": record["target"], "app_id": record["bundle_identifier"], "url": url,
        "size": size, "sha256": digest, "archive": "zip", "root": "BroScribe.app",
        "entries": entries, "installed_size": installed,
    }


def manifest_document(record: dict, artifacts: list[dict], channel: str, expires_days: int,
                      now: datetime) -> dict:
    version, build = record.get("version"), record.get("build")
    if not isinstance(version, str) or not RELEASE_VERSION.fullmatch(version):
        raise MetadataError(f"version {version!r} is not MAJOR.MINOR.PATCH")
    if type(build) is not int or build <= 0:
        raise MetadataError(f"build {build!r} is not a positive integer")
    product = record.get("product")
    if not isinstance(product, str) or not 0 < len(product) <= 64:
        raise MetadataError("the provenance has no product")
    for entry in artifacts:
        if not isinstance(entry["target"], str) or not TARGET.fullmatch(entry["target"]):
            raise MetadataError("the provenance has an invalid target")
        if not isinstance(entry["app_id"], str) or not 0 < len(entry["app_id"]) <= 255:
            raise MetadataError("the provenance has no bundle identifier")
    if not CHANNEL.fullmatch(channel):
        raise MetadataError("the channel must be 1-32 lowercase letters, digits or '-'")
    if not 1 <= expires_days <= MAX_EXPIRY_DAYS:
        raise MetadataError(f"the manifest must expire within 1-{MAX_EXPIRY_DAYS} days")
    published = now.replace(microsecond=0)
    return {
        "format": MANIFEST_FORMAT, "schema": 1, "product": product, "channel": channel,
        "version": version, "build": build, "published_at": rfc3339(published),
        "expires_at": rfc3339(published + timedelta(days=expires_days)), "artifacts": artifacts,
    }


def release_manifest(artifact: Path, record: dict, url: str, channel: str, expires_days: int,
                     now: datetime) -> dict:
    check_artifact_url(url, artifact.name)
    return manifest_document(record, [macos_update_artifact(artifact, record, url)], channel, expires_days, now)


MACOS_TEAM = "3CHB92P2NM"
RELEASE_ASSETS = "https://github.com/BroToolbox/BroScribe/releases/download"
DESKTOP_TARGETS = {
    "linux-x86_64": ("x86_64-unknown-linux-gnu", "tar.gz"),
    "linux-aarch64": ("aarch64-unknown-linux-gnu", "tar.gz"),
    "windows-x64": ("x86_64-pc-windows-msvc", "zip"),
}


def combined_manifest(packages: Path, tag: str, now: datetime, expires_days: int = 90, *,
                      allow_legacy_layout: bool = False) -> dict:
    """Build the stable feed from the four separately verified CI packages."""
    if not tag.startswith("v") or not RELEASE_VERSION.fullmatch(tag[1:]):
        raise MetadataError("the GitHub release tag must be vMAJOR.MINOR.PATCH")
    artifacts, records = [], []
    for label in ("macos-arm64", *DESKTOP_TARGETS):
        folders = [path for path in packages.iterdir()
                   if path.name == label or path.name.startswith(label + "-")]
        if len(folders) != 1 or not folders[0].is_dir():
            raise MetadataError(f"expected exactly one verified {label} package")
        folder = folders[0]
        mac = label == "macos-arm64"
        target, archive = ("aarch64-apple-darwin", "zip") if mac else DESKTOP_TARGETS[label]
        candidates = list(folder.glob(f"*.{archive}"))
        if len(candidates) != 1:
            raise MetadataError(f"expected exactly one {label} {archive} archive")
        artifact = candidates[0]
        record = json.loads(artifact.with_name(artifact.name.removesuffix(f".{archive}")
                                               + ".provenance.json").read_bytes())
        build = record.get("build")
        if (record.get("schema"), record.get("product"), record.get("version"), record.get("target")) != (
                1, "BroScribe", tag[1:], target) or type(build) is not int or build <= 0:
            raise MetadataError(f"{artifact.name} provenance disagrees with the release identity")
        git = record.get("git", {})
        if git.get("dirty") is not False or not is_hex(git.get("commit"), 40):
            raise MetadataError(f"{artifact.name} does not identify a clean release commit")
        records.append(record)
        adhoc = mac and folder.name == "macos-arm64-adhoc"
        expected = f"BroScribe-{tag[1:]}-{build}-{label}" + ("-adhoc" if adhoc else "") + f".{archive}"
        if artifact.name != expected:
            raise MetadataError(f"expected release asset {expected}")
        url = f"{RELEASE_ASSETS}/{tag}/{artifact.name}"
        if mac:
            problems = load_sibling("verify-macos-artifact.py").check_checksum(artifact, None)
            if problems:
                raise MetadataError("; ".join(problems))
            if adhoc:
                continue  # The verified preview is useful as a manual download only.
            if record.get("signing", {}).get("team_identifier") != MACOS_TEAM:
                raise MetadataError("the macOS artifact is signed by a different team than the updater pins")
            artifacts.append(macos_update_artifact(artifact, record, url))
            continue
        verifier = load_sibling("verify-desktop-artifact.py")
        verifier.verify(artifact, allow_legacy_layout=allow_legacy_layout)
        entry = record["artifact"]
        artifacts.append({
            "target": target, "app_id": record["product"], "url": url,
            "size": entry["size"], "sha256": entry["sha256"], "archive": archive,
            "root": verifier.ROOT.rstrip("/"), "entries": entry["entries"],
            "installed_size": entry["installed_size"],
        })
    first = records[0]
    identity = lambda record: (record["build"], record["git"]["commit"], record["cargo_lock_sha256"])
    if any(identity(record) != identity(first) for record in records[1:]):
        raise MetadataError("verified packages are from different builds, commits or lockfiles")
    return manifest_document(first, artifacts, "stable", expires_days, now)


def write_manifest(args) -> None:
    now = datetime.now(timezone.utc)
    if args.packages is not None:
        if args.tag is None or args.provenance is not None or args.url is not None or args.channel != "stable":
            raise MetadataError("--packages requires --tag and the stable channel")
        manifest = combined_manifest(args.packages, args.tag, now, args.expires_days)
    else:
        if args.provenance is None or args.url is None or args.tag is not None:
            raise MetadataError("--artifact requires --provenance and --url")
        record = json.loads(args.provenance.read_text())
        manifest = release_manifest(args.artifact, record, args.url, args.channel, args.expires_days, now)
    args.out.write_bytes((json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode())


def key_id(public_key: str) -> str:
    """The updater's key ID: the first 8 bytes of the SHA-256 of the raw key."""
    return hashlib.sha256(bytes.fromhex(public_key)).hexdigest()[:16]


def release_envelope(signed, manifest_text: str, trusted_keys: list[str]) -> str:
    """The envelope text from `broscribe --json update sign-manifest` output,
    once it carries `manifest_text` exactly, every signature is reported with
    its signer's public key and at least one signer is a key clients pin."""
    if not trusted_keys:
        raise MetadataError("name at least one public key the updater pins (--trusted-key)")
    if not all(is_hex(key, 64) for key in trusted_keys):
        raise MetadataError("a trusted key is not 64 lowercase hex digits")
    if not isinstance(signed, dict) or set(signed) != {"envelope", "signers"} \
            or not isinstance(signed["envelope"], str):
        raise MetadataError("the signer output is not {envelope, signers}")
    text = signed["envelope"]
    try:
        envelope = json.loads(text)
    except json.JSONDecodeError as error:
        raise MetadataError(f"envelope: {error}") from error
    if not isinstance(envelope, dict) or set(envelope) != {"format", "schema", "manifest", "signatures"}:
        raise MetadataError("the envelope does not have exactly format, schema, manifest and signatures")
    if envelope["format"] != ENVELOPE_FORMAT or type(envelope["schema"]) is not int or envelope["schema"] != 1:
        raise MetadataError("not a schema 1 BroScribe update envelope")
    if envelope["manifest"] != manifest_text:
        raise MetadataError("the envelope does not carry this manifest exactly")
    signatures = envelope["signatures"]
    if not isinstance(signatures, list) or not signatures:
        raise MetadataError("the envelope is unsigned")
    for signature in signatures:
        if not isinstance(signature, dict) or set(signature) != {"key_id", "ed25519"} \
                or not is_hex(signature["key_id"], 16) or not is_hex(signature["ed25519"], 128):
            raise MetadataError("an envelope signature is not {key_id: 16 hex, ed25519: 128 hex}")
    signers = signed["signers"]
    if not isinstance(signers, list):
        raise MetadataError("the signer output lists no signers")
    for signer in signers:
        if not isinstance(signer, dict) or set(signer) != {"label", "key_id", "public_key"} \
                or not is_hex(signer["public_key"], 64) or signer["key_id"] != key_id(signer["public_key"]):
            raise MetadataError("a reported signer is not {label, key_id, public_key} with a matching key ID")
    if [s["key_id"] for s in signatures] != [s["key_id"] for s in signers]:
        raise MetadataError("the envelope's signatures and the reported signers differ")
    if not any(signer["public_key"] in trusted_keys for signer in signers):
        raise MetadataError("no signature is by a key the updater pins; installations would refuse the release")
    return text


def write_envelope(args) -> None:
    signed = json.loads(args.signed.read_bytes().decode("utf-8"))
    if isinstance(signed, dict) and set(signed) == {"error"} and isinstance(signed["error"], dict):
        raise MetadataError(f"signing failed: {signed['error'].get('code')}: {signed['error'].get('message')}")
    text = release_envelope(signed, args.manifest.read_bytes().decode("utf-8"), args.trusted_key)
    args.out.write_bytes(text.encode("utf-8"))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)

    icon = sub.add_parser("icon")
    icon.add_argument("--out", type=Path, required=True)

    info = sub.add_parser("build-info")
    info.add_argument("--out", type=Path, required=True)
    for flag in ("version", "build", "target", "git-commit", "git-dirty", "rustc", "cargo",
                 "sherpa-archive", "sherpa-sha256", "profile", "copyright"):
        info.add_argument(f"--{flag}", required=True)
    info.add_argument("--cargo-lock", type=Path, required=True)

    log = sub.add_parser("notary-log")
    log.add_argument("--in", dest="input", type=Path, required=True)
    log.add_argument("--out", type=Path, required=True)
    log.add_argument("--redact", action="append", default=[])

    prov = sub.add_parser("provenance")
    prov.add_argument("--artifact", type=Path, required=True)
    prov.add_argument("--app", type=Path, required=True)
    prov.add_argument("--signing-json", type=Path, required=True)
    prov.add_argument("--submission-id", required=True)
    prov.add_argument("--notary-status", required=True)
    prov.add_argument("--dist-dir", type=Path)

    manifest = sub.add_parser("manifest")
    source = manifest.add_mutually_exclusive_group(required=True)
    source.add_argument("--artifact", type=Path)
    source.add_argument("--packages", type=Path)
    manifest.add_argument("--tag")
    manifest.add_argument("--provenance", type=Path)
    manifest.add_argument("--url")
    manifest.add_argument("--channel", default="stable")
    manifest.add_argument("--expires-days", type=int, default=90)
    manifest.add_argument("--out", type=Path, required=True)

    envelope = sub.add_parser("envelope")
    envelope.add_argument("--manifest", type=Path, required=True)
    envelope.add_argument("--signed", type=Path, required=True)
    envelope.add_argument("--trusted-key", action="append", default=[])
    envelope.add_argument("--out", type=Path, required=True)

    args = parser.parse_args(argv)
    try:
        if args.command == "icon":
            write_iconset(args.out)
        elif args.command == "build-info":
            args.out.write_text(json.dumps(build_info(args), indent=2, sort_keys=True) + "\n")
        elif args.command == "notary-log":
            try:
                raw = json.loads(args.input.read_text())
            except json.JSONDecodeError:
                raw = {"statusSummary": "notarization log was not JSON; see notarytool output"}
            args.out.write_text(json.dumps(sanitize_notary_log(raw, args.redact), indent=2, sort_keys=True) + "\n")
            args.out.chmod(0o600)
        elif args.command == "provenance":
            args.dist_dir = args.dist_dir or args.artifact.parent
            write_provenance(args)
        elif args.command == "manifest":
            write_manifest(args)
        else:
            write_envelope(args)
    except (MetadataError, OSError, ValueError, EOFError, KeyError, AttributeError, TypeError,
            plistlib.InvalidFileException, zipfile.BadZipFile, tarfile.TarError, struct.error) as error:
        print(f"release-metadata {args.command}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
