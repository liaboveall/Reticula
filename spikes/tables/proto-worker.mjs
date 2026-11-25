// Runs the prototype in a worker, on a file in the origin-private file system.
let wasm, handle, memory, reads = 0, readBytes = 0;
const env = {
  file_write: (at, pointer, length) => handle.write(new Uint8Array(memory.buffer, pointer, length), { at }),
  file_read: (at, pointer, length) => { reads++; readBytes += length; handle.read(new Uint8Array(memory.buffer, pointer, length), { at }); },
};
const now = () => performance.now();
/** Copies bytes into the module: the memory may move while it makes room. */
const put = (bytes) => { const pointer = wasm.input(bytes.length); new Uint8Array(memory.buffer, pointer, bytes.length).set(bytes); return bytes.length; };
const text = (value) => put(new TextEncoder().encode(value));
const tasks = {
  async boot() {
    const t = now();
    const { instance } = await WebAssembly.instantiateStreaming(fetch("/proto.wasm"), { env });
    wasm = instance.exports; memory = wasm.memory;
    const root = await navigator.storage.getDirectory();
    try { await root.removeEntry("proto.bin"); } catch {}
    handle = await (await root.getFileHandle("proto.bin", { create: true })).createSyncAccessHandle();
    return { ms: now() - t };
  },
  async import({ file, kinds, slice }) {
    const t = now();
    put(new Uint8Array(kinds));
    wasm.begin(kinds.length);
    let wait = 0;
    // The next piece is asked for while this one is parsed.
    let next = file.slice(0, slice).arrayBuffer();
    for (let at = 0; at < file.size; at += slice) {
      const a = now();
      const buffer = await next;
      wait += now() - a;
      const last = at + slice >= file.size;
      if (!last) next = file.slice(at + slice, at + 2 * slice).arrayBuffer();
      put(new Uint8Array(buffer));
      wasm.feed(buffer.byteLength, last);
    }
    const rows = wasm.finish();
    handle.flush();
    return { ms: now() - t, waited: wait, rows, bytes: wasm.file_bytes(), onDisk: handle.getSize() };
  },
  memory: () => memory.buffer.byteLength,
  dictionary: ({ column }) => wasm.dictionary_size(column),
  aggregate: ({ columns }) => { const t = now(); const values = columns.map((c) => wasm.aggregate(c)); return { ms: now() - t, values }; },
  sort: ({ column, descending, fresh }) => { const t = now(); const passes = wasm.sort(column, descending, fresh); return { ms: now() - t, passes }; },
  filter: ({ column, value, number, limit }) => { const n = text(value); const t = now(); const count = wasm.filter_equal_greater(column, n, number, limit); return { ms: now() - t, count }; },
  contains: ({ column, value }) => { const n = text(value); const t = now(); const count = wasm.filter_contains(column, n); return { ms: now() - t, count }; },
  project: ({ sorted }) => { const t = now(); const count = wasm.project(sorted); return { ms: now() - t, count }; },
  screens: ({ starts, count, ordered }) => { const r = reads; const times = starts.map((s) => { const t = now(); wasm.screen(s, count, ordered); return now() - t; }); return { times, reads: (reads - r) / starts.length }; },
  clear: () => wasm.clear_cache(),
};
onmessage = async ({ data: { id, name, argument } }) => {
  try { postMessage({ id, value: await tasks[name](argument) }); } catch (error) { postMessage({ id, error: String(error) }); }
};
