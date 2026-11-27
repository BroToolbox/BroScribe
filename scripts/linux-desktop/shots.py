"""README screenshot manifest, artifact validation and publish decision.

Standard library only, so the publish job can run it without installing
anything. Pixel hashes come from a small PNG decoder (8-bit RGB or RGBA,
not interlaced) and are the SHA-256 of the decoded RGBA bytes; they ignore
how the file was compressed, so they decide whether the screenshots changed.
The decoder is strict: only IHDR, IDAT and IEND chunks (what the capture
writes), the required size checked from IHDR before anything is inflated,
and a zlib stream that ends exactly where the image data ends.

    shots.py manifest <dir> --commit <sha>     write <dir>/manifest.json
    shots.py validate <dir> --commit <sha>     check a downloaded artifact
    shots.py decide <new manifest> [<published manifest>]
                                               print "publish" or "unchanged"
    shots.py source-commit <published manifest>
                                               print its source commit, if valid
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import struct
import sys
import zlib
from pathlib import Path

IMAGES = ("library.png", "meeting.png", "meeting-dark.png", "recording.png")
MANIFEST = "manifest.json"
EXPECTED_FILES = frozenset(IMAGES + (MANIFEST,))
WIDTH, HEIGHT = 1280, 800
MAX_IMAGE_BYTES = 8 * 1024 * 1024
MAX_MANIFEST_BYTES = 16 * 1024
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
# The only chunks the capture writes. Anything else (text, dates, tRNS
# transparency, palettes, gamma) is refused rather than half-supported.
ALLOWED_CHUNKS = frozenset({b"IHDR", b"IDAT", b"IEND"})
COMMIT = re.compile(r"[0-9a-f]{40}")


class Invalid(ValueError):
    """The artifact or image is not what the capture produces."""


def chunks(data: bytes):
    """(type, body) of every chunk, with CRCs checked, ending at IEND."""
    if not data.startswith(PNG_SIGNATURE):
        raise Invalid("not a PNG file")
    at = len(PNG_SIGNATURE)
    first = True
    while True:
        if at + 8 > len(data):
            raise Invalid("truncated PNG chunk")
        length, kind = struct.unpack(">I4s", data[at:at + 8])
        if kind not in ALLOWED_CHUNKS:
            raise Invalid(f"unexpected {kind.decode('latin-1')!r} chunk")
        if (kind == b"IHDR") != first:
            raise Invalid("IHDR must be the first and only header chunk")
        first = False
        end = at + 8 + length
        if end + 4 > len(data):
            raise Invalid("truncated PNG chunk")
        body = data[at + 8:end]
        (crc,) = struct.unpack(">I", data[end:end + 4])
        if zlib.crc32(kind + body) != crc:
            raise Invalid(f"bad CRC in {kind!r} chunk")
        yield kind, body
        at = end + 4
        if kind == b"IEND":
            if body:
                raise Invalid("IEND chunk must be empty")
            if at != len(data):
                raise Invalid("data after IEND")
            return


def header(data: bytes) -> tuple[int, int, int]:
    """Width, height and colour type, from IHDR alone."""
    kind, body = next(chunks(data))
    if kind != b"IHDR" or len(body) != 13:
        raise Invalid("PNG does not start with IHDR")
    width, height, depth, colour, compression, filtering, interlace = struct.unpack(">IIBBBBB", body)
    if depth != 8 or colour not in (2, 6) or compression or filtering or interlace:
        raise Invalid("only 8-bit RGB or RGBA PNGs without interlacing are expected")
    return width, height, colour


def _paeth(a: int, b: int, c: int) -> int:
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    return b if pb <= pc else c


def rgba(data: bytes, size: tuple[int, int] | None = (WIDTH, HEIGHT)) -> tuple[int, int, bytes]:
    """Decode an 8-bit RGB or RGBA PNG to width, height and RGBA bytes.

    The dimensions are checked against `size` (None: any up to 8192) from
    IHDR, before any image data is inflated.
    """
    width, height, colour = header(data)
    if size is not None and (width, height) != size:
        raise Invalid(f"image is {width}x{height}, not {size[0]}x{size[1]}")
    if not 0 < width <= 8192 or not 0 < height <= 8192:
        raise Invalid("unexpected PNG dimensions")
    channels = 4 if colour == 6 else 3
    stride = width * channels
    expected = height * (stride + 1)
    idat = b"".join(body for kind, body in chunks(data) if kind == b"IDAT")
    inflater = zlib.decompressobj()
    try:
        raw = inflater.decompress(idat, expected + 1)
    except zlib.error as error:
        raise Invalid(f"PNG image data does not inflate: {error}") from None
    if len(raw) != expected or inflater.unconsumed_tail:
        raise Invalid("PNG image data has the wrong size")
    if not inflater.eof or inflater.unused_data:
        raise Invalid("PNG image data does not end with its zlib stream")
    previous = bytearray(stride)
    rows = []
    for y in range(height):
        start = y * (stride + 1)
        kind = raw[start]
        line = bytearray(raw[start + 1:start + 1 + stride])
        if kind == 1:
            for i in range(channels, stride):
                line[i] = (line[i] + line[i - channels]) & 0xFF
        elif kind == 2:
            line = bytearray((x + p) & 0xFF for x, p in zip(line, previous))
        elif kind == 3:
            for i in range(stride):
                left = line[i - channels] if i >= channels else 0
                line[i] = (line[i] + ((left + previous[i]) >> 1)) & 0xFF
        elif kind == 4:
            for i in range(stride):
                if i >= channels:
                    left, corner = line[i - channels], previous[i - channels]
                else:
                    left = corner = 0
                line[i] = (line[i] + _paeth(left, previous[i], corner)) & 0xFF
        elif kind != 0:
            raise Invalid(f"unknown PNG filter {kind}")
        rows.append(bytes(line))
        previous = line
    pixels = b"".join(rows)
    if channels == 3:
        out = bytearray(width * height * 4)
        out[0::4], out[1::4], out[2::4] = pixels[0::3], pixels[1::3], pixels[2::3]
        out[3::4] = b"\xff" * (width * height)
        pixels = bytes(out)
    return width, height, pixels


def describe(data: bytes, size: tuple[int, int] | None = (WIDTH, HEIGHT)) -> dict:
    """The manifest entry of one image, which must be `size` (None: any)."""
    width, height, pixels = rgba(data, size)
    return {
        "sha256": hashlib.sha256(data).hexdigest(),
        "pixels_sha256": hashlib.sha256(pixels).hexdigest(),
        "width": width,
        "height": height,
    }


def build_manifest(folder: Path, commit: str) -> dict:
    if not COMMIT.fullmatch(commit):
        raise Invalid("source commit must be 40 lowercase hex digits")
    images = {}
    for name in IMAGES:
        try:
            images[name] = describe((folder / name).read_bytes())
        except Invalid as error:
            raise Invalid(f"{name}: {error}") from None
    return {"source_commit": commit, "images": images}


def write_manifest(folder: Path, commit: str) -> dict:
    manifest = build_manifest(folder, commit)
    (folder / MANIFEST).write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return manifest


def _read_limited(path: Path, limit: int) -> bytes:
    if path.is_symlink() or not path.is_file():
        raise Invalid(f"{path.name} is not a regular file")
    if path.stat().st_size > limit:
        raise Invalid(f"{path.name} is larger than {limit} bytes")
    return path.read_bytes()


def validate(folder: Path, commit: str) -> dict:
    """Check a downloaded artifact as data and return its manifest."""
    if folder.is_symlink() or not folder.is_dir():
        raise Invalid("artifact folder missing")
    names = {entry.name for entry in folder.iterdir()}
    if names != EXPECTED_FILES:
        raise Invalid(f"unexpected artifact files: {sorted(names ^ EXPECTED_FILES)}")
    try:
        manifest = json.loads(_read_limited(folder / MANIFEST, MAX_MANIFEST_BYTES))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise Invalid(f"manifest is not JSON: {error}") from None
    if not isinstance(manifest, dict) or set(manifest) != {"source_commit", "images"}:
        raise Invalid("manifest must hold exactly source_commit and images")
    if manifest["source_commit"] != commit or not COMMIT.fullmatch(commit):
        raise Invalid("manifest source_commit is not the commit being published")
    if not isinstance(manifest["images"], dict) or set(manifest["images"]) != set(IMAGES):
        raise Invalid("manifest must list exactly the four images")
    for name in IMAGES:
        try:
            entry = describe(_read_limited(folder / name, MAX_IMAGE_BYTES))
        except Invalid as error:
            raise Invalid(f"{name}: {error}") from None
        if manifest["images"][name] != entry:
            raise Invalid(f"{name} does not match its manifest entry")
    return manifest


def pixel_map(manifest: dict | None) -> dict | None:
    """Image name to pixel hash, or None for a missing or unreadable manifest."""
    try:
        images = manifest["images"]
        return {name: images[name]["pixels_sha256"] for name in IMAGES}
    except (KeyError, TypeError):
        return None


def decide(new: dict, published: dict | None) -> str:
    """"unchanged" when every image has the published pixels, else "publish"."""
    current = pixel_map(new)
    if current is None:
        raise Invalid("new manifest has no pixel hashes")
    return "unchanged" if pixel_map(published) == current else "publish"


def read_published(path: Path | None) -> dict | None:
    """The published manifest, or None when it is missing or not a JSON object."""
    if path is None or not path.is_file():
        return None
    try:
        manifest = json.loads(_read_limited(path, MAX_MANIFEST_BYTES))
    except (UnicodeDecodeError, json.JSONDecodeError, Invalid):
        return None
    return manifest if isinstance(manifest, dict) else None


def source_commit(manifest: dict | None) -> str:
    """The manifest's source commit when it is a full SHA, else ""."""
    commit = (manifest or {}).get("source_commit")
    return commit if isinstance(commit, str) and COMMIT.fullmatch(commit) else ""


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("manifest", "validate"):
        command = commands.add_parser(name)
        command.add_argument("folder", type=Path)
        command.add_argument("--commit", required=True)
    command = commands.add_parser("decide")
    command.add_argument("new", type=Path)
    command.add_argument("published", type=Path, nargs="?")
    command = commands.add_parser("source-commit", help="print a published manifest's commit, if any")
    command.add_argument("published", type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == "manifest":
            write_manifest(args.folder, args.commit)
        elif args.command == "validate":
            validate(args.folder, args.commit)
            print("artifact valid")
        elif args.command == "source-commit":
            print(source_commit(read_published(args.published)))
        else:
            print(decide(json.loads(args.new.read_text(encoding="utf-8")), read_published(args.published)))
    except Invalid as error:
        print(f"{args.command}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
