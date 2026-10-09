export const dynamic = 'force-dynamic';

export async function GET() {
  return new Response('a'.repeat(8 * 1024 * 1024), {
    headers: { 'content-type': 'text/plain' },
  });
}
