import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

export function markerPath(id) {
  const safe = String(id).replace(/[^a-zA-Z0-9_-]/g, '');
  return path.join(process.env.MESHSCALE_FIXTURE_MARKER_DIR || os.tmpdir(), `meshscale-after-${safe}`);
}

export function readMarker(id) {
  try {
    return fs.readFileSync(markerPath(id), 'utf8');
  } catch {
    return null;
  }
}

export function writeMarker(id) {
  fs.writeFileSync(markerPath(id), 'done');
}
