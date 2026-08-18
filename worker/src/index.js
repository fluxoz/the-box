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

// The claim file is 8192 bytes and the orders get a trailing newline, so this
// is the real ceiling rather than an arbitrary half. The old 4096 made the
// 8-key SSH allowance below unreachable and rejected setups that would have
// fit comfortably.
const MAX_ORDERS_BYTES = 8191;
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
  if (typeof hostname !== "string") throw new Error("hostname must be a string");
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

  // A flashed Pi is headless: if its Wi-Fi and tunnel token do not ride in the
  // image, there is no second chance to supply them. The Configurator collects
  // both, so dropping them here silently produced a Box that never joins the
  // network the user configured.
  if (input.cloudflare_tunnel_token !== undefined) {
    const t = input.cloudflare_tunnel_token;
    if (typeof t !== "string" || t.length > 2048 || /[\r\n\0]/.test(t)) {
      throw new Error("cloudflare_tunnel_token must be a single line under 2048 chars");
    }
    out.cloudflare_tunnel_token = t.trim();
  }

  if (input.wifi !== undefined) {
    const w = input.wifi;
    if (typeof w !== "object" || w === null || typeof w.ssid !== "string") {
      throw new Error("wifi must be an object with an ssid");
    }
    if (w.password !== undefined && typeof w.password !== "string") {
      throw new Error("wifi.password must be a string");
    }
    if (/[\r\n\0]/.test(w.ssid) || (w.password && /[\r\n\0]/.test(w.password))) {
      throw new Error("wifi fields must be single-line");
    }
    out.wifi = w.password ? { ssid: w.ssid, password: w.password } : { ssid: w.ssid };
  }

  // The installer-image fields. A Pi image ignores all of these (the flashed
  // card IS the OS), but thebox-x86 boots an installer that wipes an internal
  // disk — and it refuses orders that do not carry explicit consent, so
  // dropping erase_disk here turned a personalized x86 stick into one that
  // boots and then declines to install.
  if (input.erase_disk === true) out.erase_disk = true;

  if (input.disk !== undefined) {
    const d = input.disk;
    if (typeof d !== "string" || d.length > 256 || /[\r\n\0]/.test(d)) {
      throw new Error("disk must be a single-line string");
    }
    out.disk = d.trim();
  }

  if (input.storage !== undefined) {
    const s = input.storage;
    if (typeof s !== "object" || s === null || Array.isArray(s)) {
      throw new Error("storage must be an object");
    }
    if (!["single", "mirror", "pool", "ask"].includes(s.layout)) {
      throw new Error("storage.layout must be single, mirror, pool, or ask");
    }
    const storage = { layout: s.layout };
    if (s.devices !== undefined) {
      if (
        !Array.isArray(s.devices) ||
        s.devices.length > 16 ||
        s.devices.some((p) => typeof p !== "string" || p.length > 256 || /[\r\n\0]/.test(p))
      ) {
        throw new Error("storage.devices must be at most 16 single-line paths");
      }
      storage.devices = s.devices;
    }
    out.storage = storage;
  }

  // The decide-on-box PIN: proves the person at the wizard is the one who
  // built this image.
  if (input.setup_pin !== undefined) {
    if (typeof input.setup_pin !== "string" || !/^[0-9]{4,8}$/.test(input.setup_pin)) {
      throw new Error("setup_pin must be 4-8 digits");
    }
    out.setup_pin = input.setup_pin;
  }

  if (input.min_disk_gb !== undefined) {
    const n = input.min_disk_gb;
    if (!Number.isInteger(n) || n < 1 || n > 100000) {
      throw new Error("min_disk_gb must be an integer between 1 and 100000");
    }
    out.min_disk_gb = n;
  }

  if (input.force === true) out.force = true;

  if (input.finish !== undefined) {
    if (!["reboot", "poweroff", "none"].includes(input.finish)) {
      throw new Error("finish must be reboot, poweroff, or none");
    }
    out.finish = input.finish;
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
 * Emit [start, end] of the personalized artifact, without a JS transform.
 *
 * The first version piped the whole R2 body through a TransformStream that
 * inspected every chunk. That works locally and dies in production: a 1.96 GB
 * image burned ~2 s of CPU and Workers killed the request mid-stream, so the
 * client got HTTP 200 and a silently TRUNCATED image. A truncated OS image is
 * the worst failure available here, because it looks like a successful download.
 *
 * So nothing touches the bulk bytes in JS any more. The artifact is spliced
 * from three pieces — the R2 range before the claim file, the patch itself, and
 * the R2 range after it — and each R2 body is piped straight into a
 * FixedLengthStream, an identity stream implemented in the runtime. CPU stays
 * flat regardless of image size.
 *
 * FixedLengthStream also restores Content-Length: a response whose body is a
 * plain JS stream has no known length, so the runtime frames it chunked and
 * discards any content-length header we set. Declaring the length here is what
 * puts a real progress bar on a multi-gigabyte download.
 */
function segmentsFor(start, end, patch, patchStart) {
  const patchEnd = patchStart + patch.length; // exclusive
  const segments = [];
  const clip = (a, b) => [Math.max(a, start), Math.min(b, end + 1)];

  const [h0, h1] = clip(0, patchStart);
  if (h0 < h1) segments.push({ offset: h0, length: h1 - h0 });

  const [p0, p1] = clip(patchStart, patchEnd);
  if (p0 < p1) segments.push({ bytes: patch.subarray(p0 - patchStart, p1 - patchStart) });

  const [t0, t1] = clip(patchEnd, Infinity);
  if (t0 < t1 && Number.isFinite(t1)) segments.push({ offset: t0, length: t1 - t0 });

  return segments;
}

async function pump(bucket, key, segments, writable) {
  try {
    for (const seg of segments) {
      if (seg.bytes) {
        const writer = writable.getWriter();
        await writer.write(seg.bytes);
        writer.releaseLock();
        continue;
      }
      const part = await bucket.get(key, {
        range: { offset: seg.offset, length: seg.length },
      });
      if (!part) throw new Error(`missing range ${seg.offset}+${seg.length}`);
      // preventClose: the next segment still has to go down the same pipe.
      await part.body.pipeTo(writable, { preventClose: true });
    }
    await writable.close();
  } catch (e) {
    // Abort rather than close: a half-written body must look broken to the
    // client, not like a complete (short) image.
    await writable.abort(e).catch(() => {});
  }
}

const UNSATISFIABLE = Symbol("unsatisfiable-range");

function parseRange(header, total) {
  const m = /^bytes=(\d*)-(\d*)$/.exec(header ?? "");
  if (!m) return null;
  let [, s, e] = m;
  if (s === "" && e === "") return null;
  let start = s === "" ? total - Number(e) : Number(s);
  let end = s === "" || e === "" ? total - 1 : Number(e);
  if (!Number.isFinite(start) || !Number.isFinite(end)) return null;
  if (start >= total) return UNSATISFIABLE;
  start = Math.max(0, start);
  end = Math.min(total - 1, end);
  if (start > end) return UNSATISFIABLE;
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
    let manifest;
    let patch;
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
      // Measure UTF-8 bytes, not UTF-16 units: the claim file is sized in
      // bytes, so a body of multi-byte characters can pass a length check and
      // still overflow the payload.
      if (new TextEncoder().encode(raw).length > MAX_ORDERS_BYTES) {
        throw new Error("orders too large");
      }
      let parsed;
      try {
        parsed = JSON.parse(raw);
      } catch {
        // Not the parser's message: V8 quotes the offending input, which would
        // put part of the caller's orders — code hash, keys — into a response
        // and into any log that records it.
        throw new Error("orders are not valid JSON");
      }
      orders = validateOrders(parsed);

      const manifestObj = await env.IMAGES.get(`${board}.img.gz.manifest.json`);
      if (!manifestObj) return new Response("unknown board", { status: 404 });
      manifest = await manifestObj.json();
      // Inside the try on purpose: this throws when the orders do not fit, and
      // that is the caller's mistake (400), not a Worker crash (500).
      patch = buildPatch(orders, manifest.payload_length);
    } catch (e) {
      // Never echo the body back: it carries the code hash and public keys.
      return new Response(JSON.stringify({ error: String(e.message || e) }), {
        status: 400,
        headers: { "content-type": "application/json" },
      });
    }

    const patchStart = manifest.payload_offset;
    const total = manifest.total_length;

    // The manifest carries the offsets used to splice this exact artifact. If
    // an upload half-succeeded and the two disagree, the patch would land in
    // the wrong place and produce an image that flashes and then fails
    // mysteriously. Refuse instead.
    const head = await env.IMAGES.head(`${board}.img.gz`);
    if (!head) return new Response("image not found", { status: 404 });
    if (head.size !== total) {
      return new Response(
        JSON.stringify({ error: "image and manifest disagree; this release is not servable" }),
        { status: 503, headers: { "content-type": "application/json" } }
      );
    }

    const range = parseRange(request.headers.get("range"), total);
    // A Range that cannot be satisfied is an error, not an invitation to send
    // the whole 2 GB. A resuming client asking past the end would otherwise
    // restart the entire download without being told why.
    if (range === UNSATISFIABLE) {
      return new Response("range not satisfiable", {
        status: 416,
        headers: { "content-range": `bytes */${total}` },
      });
    }
    const start = range ? range.start : 0;
    const end = range ? range.end : total - 1;
    const length = end - start + 1;

    // Declared length => the runtime frames the response with a real
    // Content-Length, and the bytes are spliced without JS touching them.
    const { readable, writable } = new FixedLengthStream(length);
    const segments = segmentsFor(start, end, patch, patchStart);
    // Deliberately not awaited: the body streams while this fills it.
    pump(env.IMAGES, `${board}.img.gz`, segments, writable);

    const headers = {
      "content-type": "application/gzip",
      "content-disposition": `attachment; filename="${board}.img.gz"`,
      "cache-control": "no-store",
      "accept-ranges": "bytes",
      "x-content-type-options": "nosniff",
    };
    if (range) {
      headers["content-range"] = `bytes ${start}-${end}/${total}`;
      return new Response(readable, { status: 206, headers });
    }
    return new Response(readable, { status: 200, headers });
  },
};
