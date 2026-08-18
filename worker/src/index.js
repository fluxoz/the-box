/**
 * Personalized image downloads.
 *
 * The generic image lives in R2 exactly once. This Worker streams it and swaps
 * the claim file's bytes in flight, so a user downloads one file, flashes it,
 * and the Box knows who owns it before it ever boots. No per-user artifact is
 * ever built or stored.
 *
 * The image is packed as three concatenated gzip members (see
 * scripts/personalizable-image.py). The middle member holds the claim file as a
 * single STORED deflate block, so those bytes sit literally in the compressed
 * file. Patching is therefore a byte-for-byte overwrite at a known offset —
 * no recompression, O(1) memory.
 *
 * The payload and its CRC32 are adjacent, so the whole patch is ONE contiguous
 * span: [payload_offset, payload_offset + payload_length + 4).
 *
 * The patch is length-preserving, which is what lets Content-Length stay
 * constant and Range requests work.
 *
 * See docs/claim-flow-spec.md.
 */

const MAX_ORDERS_BYTES = 4096; // half the claim file; the rest is padding
const HEX64 = /^[0-9a-f]{64}$/;
const HOSTNAME = /^[a-z0-9][a-z0-9-]{0,62}$/;

/** Only these keys reach the Box. Anything else is dropped, not trusted. */
function validateOrders(input) {
  if (typeof input !== "object" || input === null || Array.isArray(input)) {
    throw new Error("orders must be a JSON object");
  }
  const out = {};

  // The one required field: without it the Box has no owner, which is the
  // whole point of this endpoint.
  const hash = input.enrollment_code_hash;
  if (typeof hash !== "string" || !HEX64.test(hash)) {
    throw new Error("enrollment_code_hash must be a 64-character hex digest");
  }
  out.enrollment_code_hash = hash;

  const hostname = input.hostname ?? "auto";
  if (hostname !== "auto" && !HOSTNAME.test(hostname)) {
    throw new Error("hostname must be a lowercase DNS label, or \"auto\"");
  }
  out.hostname = hostname;

  if (input.ssh_authorized_keys !== undefined) {
    const keys = input.ssh_authorized_keys;
    if (!Array.isArray(keys) || keys.length > 8) {
      throw new Error("ssh_authorized_keys must be an array of at most 8 keys");
    }
    for (const k of keys) {
      // These land in authorized_keys. A newline would let one entry become
      // several, so the shape is checked rather than assumed.
      if (typeof k !== "string" || k.length > 1024 || /[\r\n\0]/.test(k)) {
        throw new Error("each SSH key must be a single line under 1024 chars");
      }
      if (!/^(ssh-|ecdsa-|sk-)/.test(k.trim())) {
        throw new Error("each SSH key must look like an OpenSSH public key");
      }
    }
    out.ssh_authorized_keys = keys.map((k) => k.trim());
  }

  if (input.wifi !== undefined) {
    const w = input.wifi;
    if (typeof w !== "object" || w === null || typeof w.ssid !== "string") {
      throw new Error("wifi must be an object with an ssid");
    }
    if (/[\r\n\0]/.test(w.ssid) || (w.password && /[\r\n\0]/.test(w.password))) {
      throw new Error("wifi fields must be single-line");
    }
    out.wifi = w.password ? { ssid: w.ssid, password: w.password } : { ssid: w.ssid };
  }

  return out;
}

/** The exact bytes that replace the patch span: claim file + its new CRC32. */
function buildPatch(orders, payloadLength) {
  const body = new TextEncoder().encode(JSON.stringify(orders) + "\n");
  if (body.length > payloadLength) throw new Error("orders too large");
  // NUL padding to the fixed length: the file's size never changes, so the
  // filesystem never has to be touched. The Box reads to the first NUL.
  const payload = new Uint8Array(payloadLength);
  payload.set(body, 0);

  const patch = new Uint8Array(payloadLength + 4);
  patch.set(payload, 0);
  new DataView(patch.buffer).setUint32(payloadLength, crc32(payload), true);
  return patch;
}

let CRC_TABLE = null;
function crc32(bytes) {
  if (!CRC_TABLE) {
    CRC_TABLE = new Uint32Array(256);
    for (let i = 0; i < 256; i++) {
      let c = i;
      for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      CRC_TABLE[i] = c >>> 0;
    }
  }
  let crc = 0xffffffff;
  for (let i = 0; i < bytes.length; i++) {
    crc = CRC_TABLE[(crc ^ bytes[i]) & 0xff] ^ (crc >>> 8);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

/**
 * Overwrite [patchStart, patchStart+patch.length) as bytes stream past.
 * `streamStart` is the absolute offset of the first byte we will emit, so this
 * works for a Range request as well as a whole-file download.
 */
function patchingStream(patch, patchStart, streamStart) {
  let pos = streamStart;
  return new TransformStream({
    transform(chunk, controller) {
      const start = pos;
      const end = pos + chunk.length;
      pos = end;
      const from = Math.max(start, patchStart);
      const to = Math.min(end, patchStart + patch.length);
      if (from < to) {
        // Copy before mutating: the chunk may be backed by a shared buffer.
        chunk = new Uint8Array(chunk);
        chunk.set(patch.subarray(from - patchStart, to - patchStart), from - start);
      }
      controller.enqueue(chunk);
    },
  });
}

function parseRange(header, total) {
  const m = /^bytes=(\d*)-(\d*)$/.exec(header ?? "");
  if (!m) return null;
  let [, s, e] = m;
  if (s === "" && e === "") return null;
  let start = s === "" ? total - Number(e) : Number(s);
  let end = s === "" || e === "" ? total - 1 : Number(e);
  if (!Number.isFinite(start) || !Number.isFinite(end)) return null;
  start = Math.max(0, start);
  end = Math.min(total - 1, end);
  if (start > end) return null;
  return { start, end };
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const match = /^\/image\/([a-z0-9-]{1,32})$/.exec(url.pathname);
    if (!match) return new Response("not found", { status: 404 });
    if (request.method !== "POST") {
      return new Response("use POST with orders", { status: 405 });
    }
    const board = match[1];

    let orders;
    try {
      // Two shapes. A JSON body is what an agent or script sends. A form post
      // is what the Configurator sends, because a native form download streams
      // straight to disk with a real progress bar and resumability, instead of
      // buffering gigabytes into a Blob in the tab.
      const ctype = request.headers.get("content-type") ?? "";
      let raw;
      if (ctype.includes("application/x-www-form-urlencoded")) {
        raw = (await request.formData()).get("orders") ?? "";
      } else {
        raw = await request.text();
      }
      if (raw.length > MAX_ORDERS_BYTES) throw new Error("orders too large");
      orders = validateOrders(JSON.parse(raw));
    } catch (e) {
      // Never echo the body back: it carries the code hash and public keys.
      return new Response(JSON.stringify({ error: String(e.message || e) }), {
        status: 400,
        headers: { "content-type": "application/json" },
      });
    }

    const manifestObj = await env.IMAGES.get(`${board}.img.gz.manifest.json`);
    if (!manifestObj) return new Response("unknown board", { status: 404 });
    const manifest = await manifestObj.json();

    const patch = buildPatch(orders, manifest.payload_length);
    const patchStart = manifest.payload_offset;
    const total = manifest.total_length;

    const range = parseRange(request.headers.get("range"), total);
    const obj = await env.IMAGES.get(
      `${board}.img.gz`,
      range ? { range: { offset: range.start, length: range.end - range.start + 1 } } : undefined
    );
    if (!obj) return new Response("image not found", { status: 404 });

    const streamStart = range ? range.start : 0;
    const body = obj.body.pipeThrough(patchingStream(patch, patchStart, streamStart));

    const headers = {
      "content-type": "application/gzip",
      "content-disposition": `attachment; filename="${board}.img.gz"`,
      "cache-control": "no-store",
      "accept-ranges": "bytes",
      "x-content-type-options": "nosniff",
    };
    if (range) {
      headers["content-length"] = String(range.end - range.start + 1);
      headers["content-range"] = `bytes ${range.start}-${range.end}/${total}`;
      return new Response(body, { status: 206, headers });
    }
    // Length-preserving patch, so this is the generic artifact's own length.
    headers["content-length"] = String(total);
    return new Response(body, { status: 200, headers });
  },
};
