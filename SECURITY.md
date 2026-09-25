# Security policy

## Supported surface

Veyra places real orders on a live brokerage account when both execution
controls are armed: the service's `VEYRA_TRADING_ENABLED` setting and the EA's
`InAllowLiveOrders` input. Treat every network listener as sensitive
infrastructure:

- The control surface (`127.0.0.1:8080`) can arm trading and must never be
  exposed beyond loopback.
- The EA channel is token-authenticated and reaches the terminal only through
  the tunnel.
- The console has no authentication of its own; keep it on loopback or a
  private network such as Tailscale.

## Reporting

Open a private issue through the repository owner contact rather than filing a public issue if you believe you found a security problem.

## Data rules

- Never send or commit account numbers, passwords, API keys, or order history.
- Treat all LLM output as untrusted input until validated by deterministic code.
- The risk gate must remain outside model control.
