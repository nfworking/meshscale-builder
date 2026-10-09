import { submit } from './actions';

export const dynamic = 'force-dynamic';

export default async function Action({ searchParams }) {
  const { done } = await searchParams;
  return (
    <form action={submit}>
      <p id="done">{done ? `done:${done}` : 'pending'}</p>
      <input name="name" defaultValue="fixture" />
      <button type="submit">Submit</button>
    </form>
  );
}
