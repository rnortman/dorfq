// The playground's simulator, off the page's thread: dorfq-sim compiled to
// WebAssembly. Receives a configuration's text, replies with the report
// document's text, the configuration error, or the panic that trapped it.
// The module's memory may grow during a call, so every view of it is taken
// afresh after one.
"use strict";

let module = null;

async function exports() {
  if (module === null) {
    const { instance } = await WebAssembly.instantiateStreaming(fetch("dorfq_sim_wasm.wasm"), {});
    module = instance.exports;
  }
  return module;
}

// Reads a framed reply — tag byte, little-endian u32 length, UTF-8 — and
// frees it.
function take(wasm, ptr) {
  const view = new DataView(wasm.memory.buffer);
  const tag = view.getUint8(ptr);
  const length = view.getUint32(ptr + 1, true);
  const text = new TextDecoder().decode(new Uint8Array(wasm.memory.buffer, ptr + 5, length));
  wasm.dealloc(ptr, length + 5);
  return { tag, text };
}

self.onmessage = async (event) => {
  let wasm;
  try {
    wasm = await exports();
  } catch (error) {
    self.postMessage({ kind: "unavailable", message: error.message });
    return;
  }
  const bytes = new TextEncoder().encode(event.data);
  const input = wasm.alloc(bytes.length) >>> 0;
  new Uint8Array(wasm.memory.buffer, input, bytes.length).set(bytes);
  let reply;
  try {
    reply = take(wasm, wasm.run(input, bytes.length) >>> 0);
  } catch (error) {
    let message = error.message;
    try {
      message = take(wasm, wasm.panic_message() >>> 0).text || message;
    } catch {
      // The trapped instance could not say why; the trap's own message stands.
    }
    self.postMessage({ kind: "crash", message });
    return;
  }
  wasm.dealloc(input, bytes.length);
  self.postMessage(reply.tag === 0 ? { kind: "document", text: reply.text } : { kind: "error", message: reply.text });
};
