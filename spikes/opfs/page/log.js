// The framing of an append-only log, as these experiments use it.
//
// A record is a header of two little-endian 32-bit numbers, the length of the
// payload and a checksum, followed by the payload. The checksum covers the
// length as well as the payload. A run of zero bytes, which is what a file can
// end in when its space was allocated and the data never arrived, is therefore
// not a valid empty record.
//
// A reader takes records from the start for as long as each is whole and its
// checksum agrees. Everything from the first record that fails is a tail to
// be discarded: a writer that flushes before it reports a record as stored
// never reported anything in that tail.

export const HEADER = 8;
/** A length above this is taken for damage, not for a record. */
export const MAX_PAYLOAD = 1 << 24;

const TABLE = new Uint32Array(256);
for (let byte = 0; byte < 256; byte++) {
  let value = byte;
  for (let bit = 0; bit < 8; bit++) {
    value = value & 1 ? 0xedb88320 ^ (value >>> 1) : value >>> 1;
  }
  TABLE[byte] = value >>> 0;
}

function update(crc, bytes) {
  for (let index = 0; index < bytes.length; index++) {
    crc = TABLE[(crc ^ bytes[index]) & 0xff] ^ (crc >>> 8);
  }
  return crc;
}

/** CRC-32 of a record's length field and payload; `record` starts at its header. */
function checksum(record, length) {
  const crc = update(update(0xffffffff, record.subarray(0, 4)), record.subarray(HEADER, HEADER + length));
  return (crc ^ 0xffffffff) >>> 0;
}

/** A payload with its header in front. */
export function encode(payload) {
  const record = new Uint8Array(HEADER + payload.length);
  const view = new DataView(record.buffer);
  view.setUint32(0, payload.length, true);
  record.set(payload, HEADER);
  view.setUint32(4, checksum(record, payload.length), true);
  return record;
}

/**
 * Finds the records in the contents of a log file. `end` is where the last
 * whole record ends; `tail` says how many bytes follow it and why the first
 * of them is not a record.
 */
export function scan(bytes) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const records = [];
  let at = 0;
  let reason = null;
  while (at < bytes.length) {
    if (bytes.length - at < HEADER) {
      reason = "incomplete header";
      break;
    }
    const length = view.getUint32(at, true);
    if (length > MAX_PAYLOAD) {
      reason = "length over the limit";
      break;
    }
    if (bytes.length - at - HEADER < length) {
      reason = "incomplete record";
      break;
    }
    if (view.getUint32(at + 4, true) !== checksum(bytes.subarray(at), length)) {
      reason = "checksum";
      break;
    }
    records.push({ at: at + HEADER, length });
    at += HEADER + length;
  }
  return { records, end: at, tail: { bytes: bytes.length - at, reason } };
}

// --- payloads for the experiments -------------------------------------------
//
// A payload starts with its sequence number and continues with bytes that
// follow from that number alone, so a reader can tell not only that a record
// is whole but that it is the record that was written.

/** The payload of record number `sequence`, `size` bytes long (at least 4). */
export function payloadOf(sequence, size) {
  const payload = new Uint8Array(Math.max(size, 4));
  new DataView(payload.buffer).setUint32(0, sequence, true);
  let state = Math.imul(sequence, 0x9e3779b1) >>> 0 || 1;
  for (let index = 4; index < payload.length; index++) {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    state >>>= 0;
    payload[index] = state & 0xff;
  }
  return payload;
}

export function sequenceOf(payload) {
  return new DataView(payload.buffer, payload.byteOffset, payload.byteLength).getUint32(0, true);
}

/** Whether a payload is byte for byte the one its sequence number stands for. */
export function intact(payload) {
  if (payload.length < 4) {
    return false;
  }
  const expected = payloadOf(sequenceOf(payload), payload.length);
  return expected.every((byte, index) => byte === payload[index]);
}
