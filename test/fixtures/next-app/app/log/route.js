export const dynamic = 'force-dynamic';

export async function GET() {
  console.log('fixture console.log from a route handler');
  console.info('fixture console.info');
  process.stdout.write('fixture raw stdout write\n');
  return new Response('logged', { headers: { 'content-type': 'text/plain' } });
}
