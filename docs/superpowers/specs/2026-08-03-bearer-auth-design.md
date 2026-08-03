# Design: Bearer API Key Authentication for `oxllm`

## Overview
This design introduces optional Bearer token authentication for the public-facing API endpoints of the `oxllm` proxy. This allows users to secure their proxy instance to prevent unauthorized usage, while maintaining backward compatibility by keeping the authentication optional.

## Requirements
- Authentication must be optional, configured via `config.toml`.
- If configured, endpoints `/v1/chat/completions`, `/v1/embeddings`, and `/v1/models` must require a valid `Authorization: Bearer <key>` header.
- Admin endpoints (`/status`, `/health`, `/reload`, `/admin/*`) continue to be protected by IP-based localhost-only restriction, independent of the Bearer token.
- Support hot-reload of the API key configuration.
- API key must support environment variable expansion (e.g., `${OXLLM_API_KEY}`).

## Architecture
### Configuration Changes
Update `oxllm-core` to include `api_key: Option<String>` in `ServerConfig`.

### Middleware Implementation
Implement a new Axum middleware function `require_bearer_auth` in `oxllm/src/main.rs`.
- The middleware will extract the `AppState` to retrieve the current effective `api_key`.
- If `api_key` is `None`, the middleware will proceed without authorization.
- If `api_key` is `Some(key)`, it will extract the `Authorization` header and compare the token.
- On authorization failure, it will return a JSON `401 Unauthorized` response.

### Integration
- Apply `require_bearer_auth` as a layer only to the public routes.
- The `AppState` will be refreshed during hot-reloads using the same mechanism as the upstream provider configurations.

## Error Handling
- Use a structured JSON error response for `401 Unauthorized` errors, consistent with other error responses in the proxy.

## Security Considerations
- Use constant-time comparison for token validation to prevent timing attacks.
- Keep the API key out of logs.
