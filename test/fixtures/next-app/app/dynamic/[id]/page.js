export const dynamic = 'force-dynamic';

export default async function Dynamic({ params }) {
  const { id } = await params;
  return <p id="param">{JSON.stringify(id)}</p>;
}
