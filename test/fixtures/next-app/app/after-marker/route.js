import { readMarker } from '../after/marker';

export const dynamic = 'force-dynamic';

export async function GET(request) {
  const id = new URL(request.url).searchParams.get('id') || 'default';
  return Response.json({ marker: readMarker(id) });
}
