import { Suspense } from 'react';

export const dynamic = 'force-dynamic';

async function Delayed({ ms, label }) {
  await new Promise((resolve) => setTimeout(resolve, ms));
  return <p className="chunk">{label}</p>;
}

export default function Stream() {
  return (
    <main>
      <p>shell</p>
      <Suspense fallback={<p>loading one</p>}>
        <Delayed ms={200} label="one" />
      </Suspense>
      <Suspense fallback={<p>loading two</p>}>
        <Delayed ms={400} label="two" />
      </Suspense>
    </main>
  );
}
