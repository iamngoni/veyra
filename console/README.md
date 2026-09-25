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
surface exposes, across seven pages: Overview, Activity, Trades (closed trades,
paginated), Risk, Trace (audit trail), Diagnostics (autopilot, model route,
metrics, agent log), and Settings (live `/config` overlay), plus a read-only
assistant. It has no authentication of its own: keep it on loopback or a
private network such as Tailscale. `VEYRA_CONSOLE_ALLOWED_HOSTS` adds host
names the dev and preview servers accept.
