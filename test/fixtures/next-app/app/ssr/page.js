import { cookies, headers } from 'next/headers';

export const dynamic = 'force-dynamic';

export default async function Ssr() {
  const headerList = await headers();
  const cookieStore = await cookies();
  const echoed = {
    host: headerList.get('host'),
    'x-test': headerList.get('x-test'),
    cookie: headerList.get('cookie'),
    cookies: cookieStore.getAll().map(({ name, value }) => [name, value]),
  };
  return <pre id="echo">{JSON.stringify(echoed)}</pre>;
}
