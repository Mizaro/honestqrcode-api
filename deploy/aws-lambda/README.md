# AWS Lambda deployment

Install [Cargo Lambda](https://www.cargo-lambda.info/) and the AWS SAM CLI, then run from the repository root:

```bash
cargo lambda build --release --arm64 --package honestqr-lambda
sam deploy --guided --template-file deploy/aws-lambda/template.yaml
```

The SAM template creates an ARM64 Lambda function and HTTP API. Configure throttling, a custom domain, WAF, and authentication in AWS when exposing it publicly. The Lambda adapter uses the same router and rendering core as the standalone server.

