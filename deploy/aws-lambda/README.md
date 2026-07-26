# AWS Lambda deployment

Install [Cargo Lambda](https://www.cargo-lambda.info/) and the AWS SAM CLI, then run from the repository root:

```bash
cargo lambda build --release --arm64 --package honestqr-lambda
sam deploy --guided --template-file deploy/aws-lambda/template.yaml
```

The SAM template creates an ARM64 Lambda function and HTTP API. Its 30-second
platform timeout intentionally leaves headroom above the router's request
deadline, allowing the API to return a structured timeout response. Keep that
headroom if you customize either value.

Configure throttling, a custom domain, WAF, and authentication in AWS when
exposing it publicly. The Lambda adapter uses the same router and rendering core
as the standalone server.
