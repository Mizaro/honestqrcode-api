# Security policy

## Supported versions

Security fixes are provided for the latest release on the `main` branch.

## Reporting a vulnerability

Please use GitHub's **Security → Report a vulnerability** flow for this repository. Do not open a public issue for an unpatched vulnerability.

Include the affected version, reproduction steps, impact, and any suggested mitigation. You can expect an initial acknowledgement within seven days. We will coordinate disclosure after a fix is available.

## Deployment boundary

The application intentionally provides no authentication or TLS termination. Internet-facing operators should place it behind a gateway or reverse proxy that enforces TLS, authentication where required, request-rate limits, and abuse controls. The application does not store QR payloads or log request bodies.

