export const dynamic = 'force-dynamic';

export async function GET() {
  return new Response(process.env.MESHSCALE_TEST_VALUE ?? '(unset)', {
    headers: { 'content-type': 'text/plain' },
  });
}
