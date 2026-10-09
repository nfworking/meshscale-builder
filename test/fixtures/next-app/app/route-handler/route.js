import { createHash } from 'node:crypto';

export const dynamic = 'force-dynamic';

export async function GET(request) {
  const url = new URL(request.url);
  return Response.json({ method: 'GET', a: url.searchParams.getAll('a') });
}

export async function POST(request) {
  const body = Buffer.from(await request.arrayBuffer());
  return Response.json({
    length: body.length,
    sha256: createHash('sha256').update(body).digest('hex'),
  });
}
