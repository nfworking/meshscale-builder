export async function getServerSideProps({ query, req }) {
  return { props: { query, xTest: req.headers['x-test'] || null } };
}

export default function PagesSsr(props) {
  return <pre id="props">{JSON.stringify(props)}</pre>;
}
