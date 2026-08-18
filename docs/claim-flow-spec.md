# Spec: one-step claim, for both install routes

Status: **built.** Both routes now mint the credential at the moment the user
creates the install medium, and no install path can produce a Box nobody owns.

Shipped: `box-core::pairing` (the shared 80-bit code format),
`orders::ensure_enrollment` wired into both installer wizards, persisted claim
state and the seeded-box refusal in `boxd::auth`, the boot-partition claim file
read at first boot, the installer's refusal of ownerless orders,
`scripts/personalizable-image.py`, the `pi{3,4,5}-image-personalizable` flake
outputs, `worker/`, the Configurator's VEC-05 image download and clickable claim
link, `/pair?c=` prefill, the claim receipt in the journal, and the landing page
and Pi guide rewritten around both flows.

Verified end to end: image build → placeholder → three-member gzip → patched
download → decompress → **mounted FAT filesystem carrying the orders**, with the
generic artifact's published SHA-256 still matching. A NixOS VM test
(`nix/tests/unattended-claim.nix`) boots a Box with orders on its boot partition
and proves the rest: first boot adopts them, the network cannot seize it, the
owner's code redeems, and a replay is refused.

## To switch on in production

The code is in; these are deployment steps, not development:

1. Build and upload per model, after each release:
   `nix build .#packages.aarch64-linux.pi5-image-personalizable`, then put
   `thebox-pi5.img.gz` and its `.manifest.json` in the `box-images` R2 bucket.
2. `cd worker && wrangler deploy`, and route `thebox.build/image/*` to it.
3. Publish the `.img.gz.sha256` alongside the release so the generic artifact
   stays independently checkable.

Until step 2, the Configurator's flash path posts to an endpoint that is not
there yet. The curl path is unaffected and works today.

## The goal

A stranger gets a Box onto their network in one step, and the Box knows who
owns it before it ever boots. Two routes, identical semantics:

| Route | What the user does | How the credential arrives |
|---|---|---|
| Spare PC (x86) | pastes one `curl` command | base64 orders in `BOX_ORDERS_B64` (already built) |
| Pi / SD card | downloads one image, flashes it | orders baked into the image at download |

Both mint the same thing at the same moment: **a pairing code, held by the
person who created the install medium, burned on first use, never expiring.**

## Why this shape

The credential has to exist before first boot, because after first boot there
is no channel. A headless Pi has no screen, and the network is not a channel we
can trust: `nix/module.nix` publishes `_thebox._tcp` over mDNS the moment boxd
starts, and `platform.nix` binds `0.0.0.0:2693`, so an unclaimed Box announces
itself to the whole segment. A script beats a human to `POST /pair/claim` every
time. That path is open today for any blank-flashed Box, because
`effective_orders` never adds an `enrollment_code_hash`.

Trust-wise, serving a personalized image is **the same trust level as the curl
one-liner**: in both cases the user trusts `thebox.build` over TLS to hand down
correct orders. No new exposure.

## Image format

### The placeholder

Every image ships with a real file on the FAT boot partition:

```
/box-claim.txt      exactly 8192 bytes
```

At build time it contains a 64-byte magic constant followed by NUL padding. It
is a real file in a real filesystem with a real directory entry, so
personalizing it never allocates clusters, moves data, or changes any FAT
metadata — only the bytes inside it change, and the length never does.

8192 bytes is sized for headroom: orders carry a code hash, hostname, and one
or more SSH public keys (an RSA-4096 key is ~730 bytes).

Content after personalization is the orders JSON, then `\n`, then NUL padding to
8192. The first-boot parser reads to the first NUL.

### Multi-member gzip

The served artifact is `.img.gz` built as **three concatenated gzip members**.
RFC 1952 defines a gzip file as a series of members, and decoders concatenate
their output — this is what `cat a.gz b.gz > c.gz` relies on.

```
member 1   deflate(image bytes before the placeholder)
member 2   the 8192 placeholder bytes, as a single STORED deflate block
member 3   deflate(image bytes after the placeholder)
```

Because member 2 is stored (BTYPE=00), those 8192 bytes appear **verbatim** in
the compressed file. Patching is a byte-for-byte overwrite at a known offset.

Member 2 has a fixed 8215-byte layout:

| Offset | Bytes | Contents |
|---|---|---|
| 0 | 10 | gzip header: `1f 8b 08 00`, MTIME `00 00 00 00`, XFL `00`, OS `ff` |
| 10 | 1 | `0x01` — BFINAL=1, BTYPE=00, padded to byte boundary |
| 11 | 2 | LEN = `00 20` (8192, little-endian) |
| 13 | 2 | NLEN = `ff df` (one's complement) |
| 15 | 8192 | **the payload — patch here** |
| 8207 | 4 | CRC32 of the payload, little-endian |
| 8211 | 4 | ISIZE = `00 20 00 00` (8192) |

MTIME 0 and OS 0xff keep the build reproducible.

Patching therefore touches **two fields**: 8192 payload bytes, and the 4-byte
CRC32 at offset 8207. ISIZE never changes because the length is fixed. Member 1
and member 3 are never touched, so their CRCs stay valid. No CRC combination
arithmetic is needed — we recompute a CRC32 over 8 KB, which is free.

The CRC field comes *after* the payload, so a single forward pass can compute
the new CRC while emitting the new payload and write it when it reaches the
trailer. This streams with O(1) memory.

**A length-preserving patch means `Content-Length` is a constant** — identical
to the generic artifact — so progress bars and resumable downloads work.

### Build manifest

The build emits `thebox-pi5.img.gz.manifest.json` beside the artifact:

```json
{
  "payload_offset": 1234567890,
  "payload_length": 8192,
  "crc_offset":     1234576082,
  "total_length":   2345678901,
  "sha256_generic": "…"
}
```

`payload_offset` is `len(member1) + 15`. Offsets are absolute in the `.gz`.

## Build-side changes (flake.nix)

1. Place `box-claim.txt` (magic + NUL padding, 8192 bytes) on the boot
   partition of every image — Pi and x86 alike.
2. After the raw `.img` is produced, locate the magic by scanning (deterministic
   and unique; do **not** hardcode an offset, it moves on every rebuild).
3. Split the raw image at that region and emit the three members. Members 1 and
   3 are ordinary `gzip -n`; member 2 is synthesized directly by the layout
   above.
4. Emit the manifest and the `sha256` of the finished `.gz`, computed in the
   same derivation that produced it — matching how `install.sh`'s netboot hashes
   are already stamped.
5. Keep publishing the existing `.img.xz` for people who flash manually.

## Serving (Cloudflare Worker + R2)

The generic `.img.gz` and its manifest live in R2, stored once. A Worker
streams and patches in flight; **no per-user image is ever built or stored.**

- `POST /image/:board` with the orders JSON. Responds
  `Content-Disposition: attachment` and streams the patched image.
- The Worker reads the manifest, streams the object from R2 through a
  `TransformStream` that swaps the payload region and rewrites the CRC.
- `Content-Length` is `total_length`, unchanged.
- Support `Range` requests for resumability: offsets are known, so a range that
  overlaps the payload or CRC is patched the same way.

R2 has no egress fees, which is what makes serving multi-gigabyte artifacts per
user viable.

Worker-side rules:
- Cap the orders body (8 KB minus framing) and validate against a strict schema
  before embedding. This lands in a file the Box parses at first boot.
- Reject control characters and anything that is not valid UTF-8 JSON.
- Never log the orders body.
- Rate limit per IP.

## Configurator changes

Unchanged in spirit — it already mints the code, hashes it, and keeps the code
client-side. What changes:

1. It POSTs the orders to the Worker and hands back **one image file**.
2. The recovery kit gains a **clickable claim link**
   (`http://box.local:2693/pair?c=<code>`), so the happy path involves no typing.
3. The SSH key field becomes **optional**, behind an "advanced access" toggle.
   Requiring it costs conversions from exactly the non-technical users this flow
   is for; the console and MCP are the intended interface.
4. Only the **hash** ever goes into the image. The code stays in the browser and
   the kit, so a stray `.img` or a lost SD card is never a credential.

## Daemon changes (boxd)

These are independently valuable — item 1 closes the LAN-seizure hole on its own
and should land first.

1. **`effective_orders` always carries an `enrollment_code_hash`.** At first
   boot, read `/boot/box-claim.txt`; if it holds orders, import the hash via the
   existing `import_code` path (`expires_at: i64::MAX`, burned on first use).
2. **`is_claimable` becomes an explicit persisted fact.** Today it is inferred
   from an empty auth store (`sessions.is_empty() && codes.is_empty() &&
   keys.is_empty()`), so revoking every device silently reopens first-run claim
   to the LAN. Persist `claimed_at` and read that instead.
3. **No network claim without a seeded code.** If no code was seeded, the Box is
   not claimable over the network at all; `/pair` says to claim from the console
   (`boxd auth mint` already exists and requires physical or SSH access).
4. **Raise pairing-code entropy.** `mint_code` is `random_hex(5)` — 40 bits —
   and `hash()` is a plain SHA-256 whose output ships inside the orders. Orders
   land in shell history, process tables and support pastes, and 2^40 SHA-256
   candidates fall to a commodity GPU in under a minute, so **publishing the
   hash publishes the code.** Move to ≥80 bits. Encode as grouped base32
   (`XXXX-XXXX-XXXX-XXXX`) so the rare typed case stays humane; the clickable
   link makes length free on the happy path.
5. **Claim receipt.** Journal who claimed the Box and when, and surface it on
   first sign-in. Any first-use trust model needs the owner to be able to notice
   if someone else got there first.

## Verified

The format above was built and round-tripped before this spec was written, not
assumed. A three-member file with an 8192-byte stored middle member:

- decompresses identically under **`gunzip`/`zcat`** (zlib CLI), **Node's
  `zlib.gunzipSync`** (what Etcher-class tools use), **Python's `gzip`**, and
  **WHATWG `DecompressionStream("gzip")`** (what the browser exposes);
- member 2 measures exactly **8215 bytes**, with `LEN/NLEN` = `0020 ffdf` and
  block header byte `0x01`, matching the layout table;
- after patching payload + CRC only, the file still decompresses, total
  compressed length is unchanged, decompressed image length is unchanged, and
  members 1 and 3 come back byte-identical.

## To verify before building

- **Raspberry Pi Imager and balenaEtcher specifically.** Both are zlib-derived
  and Node's implementation passed, so this is expected to work, but these are
  the two tools a real user will actually run and they deserve a live test.
- Uncompressed image size, to size the R2 objects and confirm the bandwidth
  picture.
- Whether Pi Imager's custom OS repository mechanism can list The Box, which
  would be a better distribution story than any binary we could ship.

## Explicitly rejected

- **Server-side per-user image generation.** Same UX, but builds and stores a
  unique artifact per user. The streaming patch gets the same result with no
  storage and negligible CPU.
- **A browser-based flashing tool.** Not possible: WebUSB blocks the Mass
  Storage interface class in Blink, and the File System Access API has no path
  to a raw block device. Browser firmware flashers work because they speak
  bootloader protocols (Web Serial to ESP32/STM32 ROM bootloaders, WebUSB to
  DFU-class devices) — none of which is mass storage.
- **A native flashing app.** Deferred, not dismissed. Tauri would fit the stack
  and could flash, personalize and verify in one step, but it costs Apple
  notarization, a Windows signing certificate, auto-update and cross-platform QA
  — and asks a stranger to trust a new binary at the exact moment the pitch is
  "one command, with hashes you can verify." Revisit if the Pi lane grows.
