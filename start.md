# Starting Veyra

The backend runs on the Raspberry Pi in Docker. Your Mac only runs MetaTrader 4.
The dashboard runs on Cloudflare.

```text
MT4 + VeyraProbe (Mac) --https--> ea.rusero.co.za ---+
                                                     +--> Cloudflare tunnel --> Pi: Veyra + Postgres
Dashboard veyra.rusero.co.za --> api-veyra.rusero.co.za -+
```

## Day to day

The Pi starts everything by itself after a power cut or reboot (Docker's
`unless-stopped` policy). **You normally do nothing on the Pi.** To use Veyra:

1. Open MetaTrader 4 on the Mac and make sure **AutoTrading** (toolbar) is on.
   VeyraProbe should be on your chart.
2. Check it is connected:

   ```bash
   ssh -i ~/.ssh/id_ed25519_pi ras@100.118.226.126 'cd veyra && docker compose exec -T veyra curl -s http://127.0.0.1:8080/ready'
   ```

   You want `"status":"ready"` and `"broker":"connected"`. `broker: stale` means
   MT4 is not reaching Veyra: check the Experts tab in MT4, and that
   `https://ea.rusero.co.za` is in Tools -> Options -> Expert Advisors -> Allow
   WebRequest for listed URL.
3. Open the dashboard: https://veyra.rusero.co.za (only `elishabere4@gmail.com`
   can sign in).

## Pi commands

Connect first (no password; this key has no passphrase, so keep the file private):

```bash
ssh -i ~/.ssh/id_ed25519_pi ras@100.118.226.126
cd veyra
```

| What | Command |
| --- | --- |
| Are all four running? | `docker compose ps` |
| Service log | `docker compose logs --tail=100 veyra` |
| Tunnel log | `docker compose logs --tail=50 cloudflared` |
| Restart the service | `docker compose restart veyra` |
| Start everything (if stopped) | `docker compose --profile tunnel up --detach --no-build postgres veyra watchdog cloudflared` |
| Stop everything | `docker compose --profile tunnel stop` |

The four containers are `postgres`, `veyra`, `watchdog` and `cloudflared`. The
Pi's own media apps are separate and are never touched by these commands.

## Where things are

| What | Address | Who can reach it |
| --- | --- | --- |
| Dashboard | https://veyra.rusero.co.za | Only `elishabere4@gmail.com` (Cloudflare login) |
| API the dashboard uses | https://api-veyra.rusero.co.za | Only the dashboard (Cloudflare service token) |
| EA channel (MT4 polls here) | https://ea.rusero.co.za/ea/poll | Anyone holding the EA token (`VEYRA_EA_TOKEN`) |
| Veyra config and secrets | `~/veyra/.env` on the Pi (mode 600) | The `ras` user |
| Tunnel config and credentials | `~/.cloudflared/` on the Pi (mode 600) | The `ras` user |

Veyra, Postgres and the API publish **no ports** on the Pi. The only way in is
the Cloudflare tunnel, which the Pi opens outward.

## Safety

Two independent switches must both be on before a real order can leave MT4:

| Switch | State now | Where |
| --- | --- | --- |
| Service: `VEYRA_TRADING_ENABLED` | **on** (since 2026-10-05) | `~/veyra/.env` on the Pi |
| EA: `InAllowLiveOrders` | **off** (compiled disarmed) | the EA's Inputs tab in MT4, on whichever machine runs it |

So right now every order the EA receives is validated and reported as a dry
run. To go live, switch `InAllowLiveOrders` on in MT4 yourself, after you have
seen `"broker":"connected"`. Autopilot (the system that decides trades by
itself) is separate and stays **off** (`VEYRA_AUTOPILOT_ENABLED=false`).

Limits in force (Pi `.env`): EURUSD only, 0.01 lot per order, 0.01 lot in
total, one open order at a time. Turn trading off instantly with the kill
switch (`VEYRA_RISK_KILL_SWITCH=true`, then `docker compose up --detach --no-build veyra`)
or by setting `VEYRA_TRADING_ENABLED=false` the same way.

The live-order path has never been exercised against a real account. Start
with one 0.01 lot trade and watch it.

## Using MT4 on any machine

The EA does not need the Mac or the Pi. On any machine with MT4:

1. Put `VeyraProbe` on a chart, with `InUrl` = `https://ea.rusero.co.za/ea/poll`
   and `InToken` = the value of `VEYRA_EA_TOKEN` from the Pi's `.env`.
2. In MT4: Tools -> Options -> Expert Advisors -> Allow WebRequest for listed
   URL -> add `https://ea.rusero.co.za`, and switch AutoTrading on.
3. Run only **one** terminal on the same account at a time, or two bots will
   act on it.

## Updating the backend and dashboard

The Pi runs `main` and only `main`. Merge to `main` first, then from the Mac:

```bash
scripts/deploy-pi.sh             # backend only
scripts/deploy-pi.sh --console   # backend, then the Cloudflare dashboard
```

The script exports `origin/main` from git, so your checked-out branch and
uncommitted edits never reach the Pi. It leaves the Pi's own `.env` alone,
rebuilds on the Pi (about 10 minutes, capped at 2 CPU jobs so the media apps keep
running), restarts, and records the deployed commit. Check what is live with:

```bash
ssh -i ~/.ssh/id_ed25519_pi ras@100.118.226.126 'cat veyra/.deployed-commit'
```

## If something is wrong

| Symptom | Fix |
| --- | --- |
| `broker: stale` | MT4 is closed, AutoTrading is off, or the EA was removed from the chart. |
| Dashboard loads but has no data | On the Pi: `docker compose ps`. Then check `cloudflared` is running. |
| `veyra.rusero.co.za` will not load on the Mac | Stale DNS: `sudo dscacheutil -flushcache && sudo killall -HUP mDNSResponder` |
| Pi disk nearly full | `df -h /` on the Pi (it had about 19 GB free). The build needs a few GB. |
| Pi unreachable over SSH | It uses the Tailscale address; check Tailscale is up on the Mac. |

## Fallback: run it on the Mac instead

`scripts/start.sh` starts the same stack on the Mac (Docker Postgres, the
service, the tunnel). Only do this if the Pi is down, and **never run both at
once**: two tunnel connectors with the same name split traffic between two
backends. Stop the Pi's `cloudflared` first
(`docker compose --profile tunnel stop cloudflared`), and stop the Mac's before
going back.
