# Check how your gateway handles HTTP 503

vacancyd answers an admission refusal with HTTP `503 Service Unavailable`, an OpenAI-shaped JSON error whose `type` is `no_vacancy`, a specific reason in `code`, and `Retry-After`. That is the proxy contract. It does not guarantee that a gateway exposes the same status or applies a cooldown.

## A known adapter failure mode

An Ollama-path LiteLLM inspection on September 1, 2026 found an exception mapper that checked error-message substrings without consulting the response status. vacancyd's refusal could therefore become `APIConnectionError` instead of a recognized 503. That inspection does not establish the behavior of a current LiteLLM release or another adapter. The current [router documentation](https://docs.litellm.ai/docs/routing) describes the routing controls; test the installed version before relying on them.

## Verify the path you deploy

1. Use a synthetic request and a dedicated upstream. Configure vacancyd to refuse new lending, and confirm the request receives HTTP 503 without reaching the upstream.
2. Send the same request through the gateway with the adapter and fallback configuration you intend to use.
3. Record the gateway version, error classification, selected fallback and whether the configured cooldown takes effect. Check the upstream separately to establish that it received no rejected request.
4. Repeat with an admitted request so a connection or authentication failure cannot masquerade as a successful refusal test.

Fallback and cooldown are separate outcomes. A gateway can choose another backend while still classifying the refusal in a way that bypasses its cooldown policy.

Keep vacancyd's refusal body stable. Changing it to match a foreign library's error-text heuristic would couple admission to that library's implementation. If the installed adapter misclassifies a valid HTTP response, retain a minimal synthetic reproduction for its maintainer rather than inventing a latency or compatibility promise.
