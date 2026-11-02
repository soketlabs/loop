'use strict';

function safeState(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  let serialized;
  try { serialized = JSON.stringify(value); } catch { return null; }
  if (serialized.length > 8192) return null;
  function valid(item, depth = 0) {
    if (depth > 5) return false;
    if (item === null || typeof item === 'boolean') return true;
    if (typeof item === 'number') return Number.isFinite(item);
    if (typeof item === 'string') return item.length <= 1024;
    if (typeof item !== 'object') return false;
    return Object.entries(item).length <= 64 && Object.entries(item).every(([key, entry]) =>
      !['__proto__', 'prototype', 'constructor'].includes(key) && valid(entry, depth + 1));
  }
  if (!valid(value)) return null;
  const result = {};
  if (typeof value.selected === 'string' || value.selected === null) result.selected = value.selected;
  if (value.controls && typeof value.controls === 'object' && !Array.isArray(value.controls)) result.controls = value.controls;
  return result;
}

class LineDecoder {
  constructor(onValue, onError, max = 1024 * 1024) {
    this.buffer = ''; this.onValue = onValue; this.onError = onError; this.max = max;
  }
  push(chunk) {
    this.buffer += chunk;
    let newline;
    while ((newline = this.buffer.indexOf('\n')) !== -1) {
      const line = this.buffer.slice(0, newline);
      this.buffer = this.buffer.slice(newline + 1);
      if (line.length > this.max) { this.onError(new Error('Sidecar event exceeds size limit')); continue; }
      try { this.onValue(JSON.parse(line)); } catch { this.onError(new Error('Invalid JSON from sidecar')); }
    }
    if (this.buffer.length > this.max) { this.buffer = ''; this.onError(new Error('Sidecar event exceeds size limit')); }
  }
}

module.exports = { safeState, LineDecoder };
