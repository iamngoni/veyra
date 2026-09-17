# ADR 0002 — EA control channel transport

## Status

Accepted 2026-09-17; proven live end-to-end (heartbeat plus bidirectional
ping/pong) with MT4 build 1476 under Wine.

## Context

Findings that shaped this decision, each verified on the live terminal:

- MQL4 has no socket API at all (compiler `error 168` for every socket
  function; MetaQuotes documents sockets for MQL5 only).
- MQL4 `WebRequest` selects the server port from the scheme — 80 for `http://`,
  443 for `https://` — and rejects explicit ports with
  `5200 ERR_WEBREQUEST_INVALID_ADDRESS`.
- The WebRequest allowlist matches the **full request URL**, not a host prefix.
- The MetaTrader macOS app runs the Windows terminal under Wine. Wine resolved
  the tunnel hostname to IPv6 only and does not fall back to IPv4, so requests
  failed instantly despite the host having working IPv4 connectivity.
- MQL4's `StringToCharArray` byte/count semantics produced malformed request
  bodies; the EA now builds its request bytes explicitly.

## Decision

- Transport is MQL4 `WebRequest` over **HTTPS (port 443)** to the service's EA
  endpoint, reached through a Cloudflare Tunnel
  (`veyra.antonlabs.cc` → `127.0.0.1:7801`); the listener itself stays
  loopback-only.
- The terminal's allowlist contains the **exact endpoint URL**, and automated
  trading is enabled for the terminal (the probe never trades).
- Wine needs a hosts pin: `/etc/hosts` maps the tunnel hostname to Cloudflare's
  IPv4 edges because Wine has no IPv4 fallback.
- Authentication is a shared token in the JSON body, compared in constant time.
- Protocol: one JSON object per POST with `hello` / `hb` / `pong` kinds. The
  service requests a `pong` until one arrives, proving the return path before
  the channel is trusted with more.

## Consequences

- The endpoint is publicly reachable through Cloudflare. It is token-gated and
  currently heartbeat-only; before order commands exist it must be hardened
  (for example a Cloudflare rule requiring a cookie value that MQL4 can send)
  or execution traffic must move to a loopback-only path.
- The channel is poll-based (1 s heartbeat). That is appropriate for
  decision-frequency trading and leaves headroom.
- These platform quirks (ports, allowlist matching, Wine IPv4) are documented
  operational requirements; none of them leak into the `BrokerLink` contract,
  so a future bridge or direct-API implementation is unaffected.
