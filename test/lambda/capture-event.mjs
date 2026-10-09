// Phase 5, TEMPORARY: a throwaway Lambda function (not MeshScale code) that returns and logs
// the raw event it received, so real Function URL events can be captured and turned into
// fixtures with redact-event.cjs (HEAD responses have no body, so their event is only in the
// logs). Deploy it on its own, behind an AWS_IAM Function URL, and delete it afterwards.
// Never deploy it with real data or secrets nearby.
export const handler = async (event) => {
  console.log(JSON.stringify(event));
  return {
    statusCode: 200,
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(event, null, 2),
  };
};
