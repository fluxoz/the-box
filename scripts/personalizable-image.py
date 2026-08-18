#!/usr/bin/env python3
"""Turn a raw Box image into a personalizable one.

Two jobs, in order:

  1. Put `box-claim.txt` on the image's FAT boot partition — a fixed-size file
     holding a magic constant and NUL padding. It is a real file with a real
     directory entry, so writing orders into it later never allocates clusters,
     moves data, or changes any FAT metadata. Only the bytes inside it change,
     and the length never does.

  2. Emit the image as THREE concatenated gzip members, where the middle member
     is that file's bytes as a single *stored* deflate block. Stored means the
     bytes sit literally in the compressed file, so personalizing a download is
     a byte-for-byte overwrite at a known offset plus a 4-byte CRC — no
     recompression, no per-user artifact, and it streams with O(1) memory.

RFC 1952 defines a gzip file as a series of members whose outputs concatenate,
which is what `cat a.gz b.gz > c.gz` relies on. Verified against gunzip/zcat,
Node's zlib, Python's gzip, and the browser's DecompressionStream.

See docs/claim-flow-spec.md.
"""

import gzip
import hashlib
import json
import shutil
import struct
import subprocess
import sys
import zlib
from pathlib import Path

CLAIM_NAME = "box-claim.txt"
CLAIM_LEN = 8192
MAGIC = b"BOXCLAIM-PLACEHOLDER-DO-NOT-EDIT-"
MAGIC = MAGIC + b"." * (64 - len(MAGIC))  # exactly 64 bytes

# gzip member scaffolding. MTIME=0 and OS=0xff keep the build reproducible.
GZ_HEADER = b"\x1f\x8b\x08\x00" + b"\x00" * 4 + b"\x00\xff"
FAT_TYPES = {"c", "b", "e", "ef", "0c", "0b", "0e"}


def fat_partition_offset(img: Path) -> int:
    """Byte offset of the boot partition. Prefers an EFI/FAT partition."""
    out = subprocess.run(
        ["sfdisk", "--json", str(img)], capture_output=True, text=True, check=True
    ).stdout
    table = json.loads(out)["partitiontable"]
    sector = table.get("sectorsize", 512)
    for part in table["partitions"]:
        kind = str(part.get("type", "")).lower()
        is_esp = kind.startswith("c12a7328") or kind.lstrip("0x") in FAT_TYPES
        if is_esp:
            return int(part["start"]) * sector
    # Pi images put the firmware partition first and it is always FAT.
    return int(table["partitions"][0]["start"]) * sector


def place_placeholder(img: Path) -> None:
    """Write the fixed-size placeholder file into the FAT partition."""
    offset = fat_partition_offset(img)
    payload = MAGIC + b"\x00" * (CLAIM_LEN - len(MAGIC))
    tmp = img.parent / "box-claim.seed"
    tmp.write_bytes(payload)
    subprocess.run(
        ["mcopy", "-o", "-i", f"{img}@@{offset}", str(tmp), f"::/{CLAIM_NAME}"],
        check=True,
    )
    tmp.unlink()


CHUNK = 8 << 20


def gz_member_stream(src, start: int, length: int, out) -> int:
    """Compress [start, start+length) of `src` into `out` as one gzip member.

    Streamed on purpose: a real Pi image is ~6 GB, so reading one into memory
    (let alone slicing it into prefix/suffix copies) OOMs any CI runner. Returns
    the number of compressed bytes written, which is what locates the next
    member in the output.
    """
    written = out.write(GZ_HEADER)
    co = zlib.compressobj(9, zlib.DEFLATED, -zlib.MAX_WBITS)
    crc = 0
    src.seek(start)
    left = length
    while left:
        chunk = src.read(min(CHUNK, left))
        if not chunk:
            raise SystemExit("unexpected end of image while compressing")
        left -= len(chunk)
        crc = zlib.crc32(chunk, crc)
        block = co.compress(chunk)
        if block:
            written += out.write(block)
    written += out.write(co.flush())
    written += out.write(
        struct.pack("<II", crc & 0xFFFFFFFF, length & 0xFFFFFFFF)
    )
    return written


def find_magic(path: Path) -> int:
    """Byte offset of the placeholder, scanned rather than assumed.

    The offset moves on every rebuild, so a hardcoded constant would silently
    corrupt images. Chunks overlap by len(MAGIC)-1 so a match spanning a
    boundary is still found.
    """
    found = -1
    overlap = len(MAGIC) - 1
    with path.open("rb") as f:
        base = 0
        tail = b""
        while True:
            chunk = f.read(CHUNK)
            if not chunk:
                break
            buf = tail + chunk
            pos = buf.find(MAGIC)
            while pos >= 0:
                at = base - len(tail) + pos
                if found >= 0 and at != found:
                    raise SystemExit("error: placeholder magic is not unique")
                found = at
                pos = buf.find(MAGIC, pos + 1)
            base += len(chunk)
            tail = buf[-overlap:] if overlap else b""
    return found


def stored_member(payload: bytes) -> bytes:
    """One STORED deflate block: the payload appears verbatim in the output."""
    assert len(payload) == CLAIM_LEN
    assert CLAIM_LEN <= 0xFFFF, "a stored block tops out at 65535 bytes"
    block = (
        b"\x01"  # BFINAL=1, BTYPE=00 (stored), padded to a byte boundary
        + struct.pack("<H", len(payload))
        + struct.pack("<H", len(payload) ^ 0xFFFF)
    )
    return GZ_HEADER + block + payload + struct.pack(
        "<II", zlib.crc32(payload) & 0xFFFFFFFF, len(payload)
    )


def main() -> int:
    if len(sys.argv) != 4:
        print(f"usage: {sys.argv[0]} <in.img> <out.img.gz> <out.manifest.json>")
        return 2
    src, out_gz, out_manifest = (Path(a) for a in sys.argv[1:])

    work = out_gz.parent / (src.name + ".work")
    shutil.copyfile(src, work)
    work.chmod(0o644)
    place_placeholder(work)

    at = find_magic(work)
    if at < 0:
        print("error: placeholder magic not found after mcopy", file=sys.stderr)
        return 1

    total_raw = work.stat().st_size
    with work.open("rb") as f, out_gz.open("wb") as out:
        m1_len = gz_member_stream(f, 0, at, out)
        f.seek(at)
        placeholder = f.read(CLAIM_LEN)
        out.write(stored_member(placeholder))
        gz_member_stream(f, at + CLAIM_LEN, total_raw - at - CLAIM_LEN, out)
    work.unlink()

    payload_offset = m1_len + 15  # 10-byte header + 5-byte stored block header
    digest = hashlib.sha256()
    with out_gz.open("rb") as f:
        for block in iter(lambda: f.read(CHUNK), b""):
            digest.update(block)

    manifest = {
        "claim_file": CLAIM_NAME,
        "payload_offset": payload_offset,
        "payload_length": CLAIM_LEN,
        "crc_offset": payload_offset + CLAIM_LEN,
        "total_length": out_gz.stat().st_size,
        "uncompressed_length": total_raw,
        "sha256_generic": digest.hexdigest(),
    }
    out_manifest.write_text(json.dumps(manifest, indent=2) + "\n")

    # Prove it before shipping it: the artifact must decompress to exactly the
    # image we started from, and the placeholder must be where the manifest
    # says. Streamed, so this stays honest on a 6 GB image.
    with gzip.open(out_gz, "rb") as gz:
        seen = 0
        magic_ok = False
        while True:
            block = gz.read(CHUNK)
            if not block:
                break
            if seen <= at < seen + len(block):
                magic_ok = block[at - seen : at - seen + len(MAGIC)] == MAGIC
            seen += len(block)
    if seen != total_raw:
        print(f"error: round-trip length {seen} != {total_raw}", file=sys.stderr)
        return 1
    if not magic_ok:
        print("error: placeholder not at the recorded offset", file=sys.stderr)
        return 1

    print(f"ok: {out_gz.name} ({manifest['total_length']} bytes), payload at {payload_offset}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
