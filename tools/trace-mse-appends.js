// Opt-in diagnostics for the short live fixture. Copy bytes before appendBuffer
// can detach them, and record completion ranges independently for each buffer.
window.installAppendTrace = () => {
  const trace = window.appendTrace = {buffers: [], operations: [], bytes: 0, truncated: false};
  const ranges = b => Array.from({length: b.buffered.length}, (_, i) => [b.buffered.start(i), b.buffered.end(i)]);
  const add = MediaSource.prototype.addSourceBuffer;
  MediaSource.prototype.addSourceBuffer = function(mime) {
    const b = add.call(this, mime), id = trace.buffers.length;
    trace.buffers.push({id, mime});
    let pending;
    for (const event of ['updateend', 'error', 'abort']) b.addEventListener(event, () => {
      trace.operations.push({event, id, operation: pending, wall: performance.now(), ranges: ranges(b)});
    });
    for (const method of ['appendBuffer', 'remove', 'abort']) {
      const original = b[method];
      b[method] = function(...args) {
        const row = {event: method, id, wall: performance.now(), before: ranges(b),
          offset: b.timestampOffset, start: b.appendWindowStart, end: Number.isFinite(b.appendWindowEnd) ? b.appendWindowEnd : null};
        if (method === 'appendBuffer') {
          const value = args[0];
          const bytes = ArrayBuffer.isView(value) ? new Uint8Array(value.buffer, value.byteOffset, value.byteLength) : new Uint8Array(value);
          trace.bytes += bytes.length;
          if (trace.bytes > 32 * 1024 * 1024) trace.truncated = true;
          else {
            let text = '';
            for (let i = 0; i < bytes.length; i += 8192) text += String.fromCharCode(...bytes.subarray(i, i + 8192));
            row.data = btoa(text);
          }
        } else row.args = args;
        pending = trace.operations.length;
        trace.operations.push(row);
        try { return original.apply(this, args); }
        catch (e) { row.error = String(e); throw e; }
      };
    }
    return b;
  };
};
