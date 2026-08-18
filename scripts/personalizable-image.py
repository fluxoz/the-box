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

import json
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


def gz_member(data: bytes) -> bytes:
    co = zlib.compressobj(9, zlib.DEFLATED, -zlib.MAX_WBITS)
    body = co.compress(data) + co.flush()
    return GZ_HEADER + body + struct.pack(
        "<II", zlib.crc32(data) & 0xFFFFFFFF, len(data) & 0xFFFFFFFF
    )


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
    work.write_bytes(src.read_bytes())
    place_placeholder(work)

    raw = work.read_bytes()
    at = raw.find(MAGIC)
    if at < 0:
        print("error: placeholder magic not found after mcopy", file=sys.stderr)
        return 1
    if raw.find(MAGIC, at + 1) >= 0:
        print("error: placeholder magic is not unique", file=sys.stderr)
        return 1

    # Scan for the magic rather than trusting a fixed offset: it moves on every
    # rebuild, and a stale constant would corrupt images silently.
    prefix, placeholder, suffix = raw[:at], raw[at : at + CLAIM_LEN], raw[at + CLAIM_LEN :]
    m1, m2, m3 = gz_member(prefix), stored_member(placeholder), gz_member(suffix)
    blob = m1 + m2 + m3
    out_gz.write_bytes(blob)
    work.unlink()

    payload_offset = len(m1) + 15  # 10-byte header + 5-byte stored block header
    manifest = {
        "claim_file": CLAIM_NAME,
        "payload_offset": payload_offset,
        "payload_length": CLAIM_LEN,
        "crc_offset": payload_offset + CLAIM_LEN,
        "total_length": len(blob),
        "uncompressed_length": len(raw),
        "sha256_generic": __import__("hashlib").sha256(blob).hexdigest(),
    }
    out_manifest.write_text(json.dumps(manifest, indent=2) + "\n")

    # Prove it before shipping it: the artifact must decompress to exactly the
    # image we started from.
    import gzip as _gzip

    assert _gzip.decompress(blob) == raw, "round-trip failed"
    print(f"ok: {out_gz.name} ({len(blob)} bytes), payload at {payload_offset}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
