export default function handler(req, res) {
  res.setHeader('set-cookie', ['first=1; Path=/; HttpOnly', 'second=2; Path=/']);
  res.status(200).json({
    method: req.method,
    url: req.url,
    query: req.query,
    headers: {
      cookie: req.headers.cookie || null,
      'x-test': req.headers['x-test'] || null,
      'x-multi': req.headers['x-multi'] || null,
      'content-type': req.headers['content-type'] || null,
    },
    body: req.body ?? null,
  });
}
