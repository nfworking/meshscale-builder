import { after } from 'next/server';
import { markerPath, writeMarker } from './marker';
import fs from 'node:fs';

export const dynamic = 'force-dynamic';

export async function GET(request) {
  const id = new URL(request.url).searchParams.get('id') || 'default';
  fs.rmSync(markerPath(id), { force: true });
  after(async () => {
    await new Promise((resolve) => setTimeout(resolve, 200));
    writeMarker(id);
  });
  return new Response('scheduled', { headers: { 'content-type': 'text/plain' } });
}
