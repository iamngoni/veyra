# Veyra Console

Operations console for the Veyra trading service (TanStack Start + Tailwind).

## Run

```sh
npm install
npm run dev          # development on :3000, proxying /api to 127.0.0.1:8080
npm run build        # production build into dist/
./scripts/serve.sh   # serve the build (supervised: cc.antonlabs.veyra.console)
```

`VEYRA_API_TARGET` points the proxy at another service host (default
`http://127.0.0.1:8080`). The console renders what the loopback control
surface exposes: status, account and positions, the streaming activity feed
(`/events`), command lifecycle, and the market window. It is loopback-only by
design; exposing it beyond this machine requires authentication first.
