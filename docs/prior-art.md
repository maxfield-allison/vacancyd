# Related approaches

vacancyd places an admission gate in front of a dedicated inference server. It checks owner policy and sampled resources before forwarding borrowed work, and explains a refusal with HTTP 503, a reason and `Retry-After`. This is a bounded product description, not a claim that no other project can implement the same policy.

## Model lifetime and admission answer different questions

[Ollama's keep-alive controls](https://docs.ollama.com/faq#how-do-i-keep-a-model-loaded-in-memory-or-make-it-unload-immediately) decide how long a model remains loaded after a request. vacancyd can use Ollama's unload API when its reclaim policy permits it. Model lifetime alone does not express vacancyd's owner-presence policy or per-resource admission checks.

## A gateway can choose another backend

[LiteLLM's router](https://docs.litellm.ai/docs/routing) supports routing and fallback. vacancyd supplies a separate admission answer for one lending machine; it does not replace a gateway or guarantee how that gateway classifies a refusal. Verify the actual adapter and configuration, including the behavior described in [503 interoperability](litellm-503-interop.md).

## Refusing new work does not stop accepted work

A rejected request does not begin its upstream model load. Work already accepted follows its configured cancel, drain or finish policy. A resource-scoped upstream can continue lending while the owner is present. Closing a connection and acknowledging an unload are not proof that computation stopped or memory was recovered. See [admission and recovery](admission.md).

## Choose the boundary you need

Use vacancyd when borrowers can be directed through its proxy and the supported adapter routes fit the service. Keep the upstream dedicated to borrowed work: direct requests bypass the gate, and process attribution cannot separate owner and borrower requests inside one server process. Operating-system quotas, workload scheduling and network isolation remain the operator's responsibility.
