# Phase 5 runbook: real Lambda, no edge

This is the manual part of `LAMBDA-PLAN.md` Phase 5. You run the AWS commands; the scripts
write results to files that are reviewed afterwards. Nothing here touches the edge or R2.

## 0. Safety and prerequisites

- Use a **sandbox AWS account** with no production data. Delete everything in step 7.
- Configure the AWS CLI v2 yourself (for example `aws configure sso`). The scripts call the
  `aws` CLI and never read, print or store credentials. Do not paste keys into commands,
  files or chat. For `curl --aws-sigv4`, export temporary credentials into your shell with
  `eval "$(aws configure export-credentials --format env)"`.
- Never create a Function URL with `--auth-type NONE` for anything in this runbook.
- A **Linux build host on the Lambda architecture**, with glibc (Amazon Linux 2023 or
  Ubuntu; not Alpine), Node.js 24, Rust 1.88+, git and network access. An EC2 instance works:
  x86_64 (for example `m7i.large`) for `--arch x86_64`, Graviton (`m7g.large`) for `--arch arm64`.
  The build host must not hold deploy credentials (README "Security").
- `node` on the machine where you run the scripts (the build host is fine).

Set these once per shell (pick your own region):

```bash
export AWS_REGION=eu-west-1
export FN=meshscale-phase5
export ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
```

## 1. Build and package on Linux

```bash
git clone <this repository> builder && cd builder && git checkout refactor/lambada
cargo test builds_fixture_app -- --ignored --nocapture
cargo run -- package lambda target/meshscale-fixture/output --arch x86_64 --node-runtime nodejs24.x
cat target/meshscale-fixture/output/lambda/lambda.json
```

The fixture test builds `test/fixtures/next-app`, then deletes the build workspace and invokes
`lambda-entry.cjs` locally. `package lambda` refuses the output if the Node.js major or the
architecture does not match; use `--arch arm64` on Graviton.

## 2. Create the function, a version and an alias

```bash
aws iam create-role --role-name $FN-exec --assume-role-policy-document file://test/lambda/trust-policy.json
aws iam attach-role-policy --role-name $FN-exec \
  --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
sleep 10   # IAM propagation

aws lambda create-function --function-name $FN --runtime nodejs24.x --architectures x86_64 \
  --handler lambda-entry.handler --role arn:aws:iam::$ACCOUNT:role/$FN-exec \
  --zip-file fileb://target/meshscale-fixture/output/lambda/lambda.zip \
  --memory-size 1024 --timeout 30 --environment "Variables={MESHSCALE_TEST_VALUE=phase5}"
aws lambda wait function-active-v2 --function-name $FN

export VERSION=$(aws lambda publish-version --function-name $FN --query Version --output text)
aws lambda create-alias --function-name $FN --name live --function-version $VERSION
```

If the zip is above 50 MB (`requires_s3_upload: true` in `lambda.json`), upload it to an S3
bucket in the same region and use `--code S3Bucket=...,S3Key=...` instead of `--zip-file`.

## 3. Direct-invoke matrix

```bash
node test/lambda/invoke-matrix.cjs --function $FN --qualifier $VERSION --env-value phase5 \
  --region $AWS_REGION --out test/lambda/results/direct-v$VERSION-1024mb.json
```

It sends Function URL payload 2.0 events for every route in the plan's matrix (section 8),
plus a 6.5 MB request (Lambda must reject it) and 10 concurrent invokes that must each get
their own response back. The first case usually shows `init` (a cold start). Failures are
listed with the reason; the JSON file has the details.

## 4. Cold starts and memory sizes

Each configuration change forces new execution environments (cold starts):

```bash
for mem in 512 1024 1769 2048; do
  aws lambda update-function-configuration --function-name $FN --memory-size $mem > /dev/null
  aws lambda wait function-updated-v2 --function-name $FN
  node test/lambda/invoke-matrix.cjs --function $FN --env-value phase5 --region $AWS_REGION \
    --out test/lambda/results/direct-latest-${mem}mb.json
done
```

These run against `$LATEST`; the published version from step 2 is unchanged.

## 5. Capture real Function URL events

A throwaway function returns (and logs) the raw event it received:

```bash
mkdir -p /tmp/capture && cp test/lambda/capture-event.mjs /tmp/capture/index.mjs
(cd /tmp/capture && zip -q capture.zip index.mjs)
aws lambda create-function --function-name $FN-capture --runtime nodejs24.x --handler index.handler \
  --role arn:aws:iam::$ACCOUNT:role/$FN-exec --zip-file fileb:///tmp/capture/capture.zip
aws lambda wait function-active-v2 --function-name $FN-capture
aws lambda create-function-url-config --function-name $FN-capture --auth-type AWS_IAM
export CAPTURE_URL=$(aws lambda get-function-url-config --function-name $FN-capture --query FunctionUrl --output text)

eval "$(aws configure export-credentials --format env)"
sign() { curl -sS --aws-sigv4 "aws:amz:$AWS_REGION:lambda" --user "$AWS_ACCESS_KEY_ID:$AWS_SECRET_ACCESS_KEY" \
  ${AWS_SESSION_TOKEN:+-H "x-amz-security-token: $AWS_SESSION_TOKEN"} -H "x-forwarded-host: app.example.test" "$@"; }

mkdir -p test/lambda/captured
sign -H "x-multi: one" -H "x-multi: two" -H "Cookie: a=1" -H "Cookie: b=2" \
  "${CAPTURE_URL}products/a%20b?ref=a&ref=b&x=%2F" -o test/lambda/captured/get.json
printf '{"hello":"world","n":1}' > /tmp/capture/body.json
sign -X POST -H "content-type: application/json" \
  -H "x-amz-content-sha256: $(sha256sum /tmp/capture/body.json | cut -d' ' -f1)" \
  --data-binary @/tmp/capture/body.json "${CAPTURE_URL}api/echo" -o test/lambda/captured/post-json.json
head -c 4096 /dev/urandom > /tmp/capture/body.bin
sign -X POST -H "content-type: application/octet-stream" \
  -H "x-amz-content-sha256: $(sha256sum /tmp/capture/body.bin | cut -d' ' -f1)" \
  --data-binary @/tmp/capture/body.bin "${CAPTURE_URL}route-handler" -o test/lambda/captured/post-binary.json
sign -I "${CAPTURE_URL}ssr"            # HEAD has no body: the event is in the logs
aws logs tail /aws/lambda/$FN-capture --since 10m --format short > test/lambda/captured/logs.txt

for name in get post-json post-binary; do
  node test/lambda/redact-event.cjs test/lambda/captured/$name.json test/fixtures/events/captured-$name.json
done
```

`test/lambda/captured/` is git-ignored because it holds unredacted events (account ID, IAM
caller, signing headers). Only the redacted `test/fixtures/events/captured-*.json` files are
meant to be committed, after review. If a signed request returns 403, check that your
principal has `lambda:InvokeFunctionUrl` (and `lambda:InvokeFunction`) on the function.

## 6. Function URL on the alias (AWS_IAM) against the real function

```bash
aws lambda create-function-url-config --function-name $FN --qualifier live --auth-type AWS_IAM
export URL=$(aws lambda get-function-url-config --function-name $FN --qualifier live --query FunctionUrl --output text)

curl -s -o /dev/null -w "unsigned: %{http_code}\n" "${URL}ssr"            # expect 403
sign -D - -o /dev/null "${URL}ssr" | head -1                               # expect 200
sign -H "Cookie: a=1" -H "Cookie: b=2" "${URL}ssr" | grep -o 'id="echo">[^<]*'
sign -D - "${URL}api/echo?a=1&a=2" | grep -i -E "^HTTP|^set-cookie"        # two set-cookie headers
sign -X POST -H "content-type: application/octet-stream" \
  -H "x-amz-content-sha256: $(sha256sum /tmp/capture/body.bin | cut -d' ' -f1)" \
  --data-binary @/tmp/capture/body.bin "${URL}route-handler"; sha256sum /tmp/capture/body.bin
sign -D - -o /dev/null "${URL}big" | grep -i -E "^HTTP|x-meshscale-error"   # expect 502
sign -I "${URL}ssr" | grep -i -E "^HTTP|content-length"
sign -D - -o /dev/null "${URL}redirect" | grep -i -E "^HTTP|^location"
```

Save the terminal output to `test/lambda/results/function-url.txt`.

## 7. Clean up

```bash
aws lambda delete-function-url-config --function-name $FN --qualifier live
aws lambda delete-function --function-name $FN
aws lambda delete-function-url-config --function-name $FN-capture
aws lambda delete-function --function-name $FN-capture
aws iam detach-role-policy --role-name $FN-exec \
  --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
aws iam delete-role --role-name $FN-exec
aws logs delete-log-group --log-group-name /aws/lambda/$FN
aws logs delete-log-group --log-group-name /aws/lambda/$FN-capture
```

## 8. What to hand back

- `test/lambda/results/*.json` and `function-url.txt`
- `test/fixtures/events/captured-*.json` (redacted) and, if useful, the HEAD event from
  `captured/logs.txt` after removing account and caller details
- `target/meshscale-fixture/output/lambda/lambda.json` from the Linux build
- anything that failed, with the command and its output
