# Security policy

## Supported surface

The current service is diagnostic-only and performs no trading. Any exposed network listener should be treated as sensitive infrastructure and authenticated before public deployment.

## Reporting

Open a private issue through the repository owner contact rather than filing a public issue if you believe you found a security problem.

## Data rules

- Never send or commit account numbers, passwords, API keys, or order history.
- Treat all LLM output as untrusted input until validated by deterministic code.
- The risk gate must remain outside model control.
