# Docker deployment with host-native MT4

This profile runs the observable server-side stack in Docker while MetaTrader 4
and `VeyraProbe` remain native macOS applications. It preserves the EA wire
contract and the two independent live-trading controls.

The operations console is private at:

```text
https://veyra-console.antonlabs.cc
```

That DNS-only hostname resolves to the Mac mini's Tailscale address and is not
routed through Cloudflare Tunnel. The public broker endpoint remains separate:

```text
https://veyra.antonlabs.cc/ea/poll
```

## Runtime boundaries

| Component | Runtime | Exposure |
| --- | --- | --- |
| MT4 + `VeyraProbe` | native macOS | outbound HTTPS to the EA hostname |
| Veyra | Docker | Compose network only; ports 7801 and 8080 are not published |
| PostgreSQL 17 | Docker | Compose network only; named persistent volume |
| Console | Docker/TanStack preview | Traefik through the Tailscale-addressed DNS name only |
| EA `cloudflared` | optional Compose profile | outbound-only; started only during cutover |
| Traefik | existing media-server stack | private console TLS and routing |

The service's normal EA listener remains loopback-only. Docker requires an
explicit `VEYRA_EA_ALLOW_NON_LOOPBACK=true` opt-in, and `compose.yaml` uses it
only on an un-published project network. Startup still rejects accidental
non-loopback binding without that opt-in.

## Prepare secrets and state

Copy the existing `.env` over an encrypted SSH connection and keep it mode
`0600`. Add a random `VEYRA_POSTGRES_PASSWORD` to that untracked file. Keep
these settings false during migration; Compose reads them from `.env` and
defaults each missing value to false:

```dotenv
VEYRA_ENV=production
VEYRA_TRADING_ENABLED=false
VEYRA_EA_ALLOW_LIVE=false
VEYRA_AUTOPILOT_ENABLED=false
```

A restored database can carry live settings saved from the console, and those
win over `.env`. After the service starts, check `GET /config` for
`overridden: true` on `VEYRA_TRADING_ENABLED` and `VEYRA_AUTOPILOT_ENABLED`,
and clear any override with `POST /config {"VEYRA_TRADING_ENABLED": null}`.

Do not start the tunnel profile during initial verification. The old machine
continues to own the named tunnel until final cutover.

Model fallback identifiers are read from `VEYRA_MODEL_FALLBACKS` at startup.
They must be available to the OpenRouter account's allowed-provider policy and
support the structured-response path; a syntactically valid model id is not a
runtime proof. The console's live settings overlay takes precedence over `.env`
when an operator has edited the model section, so clear that override or apply
the new chain through the console before recreating the container.

## Build and restore

Start only the fresh database, restore the verified custom archive, then start
the service and console:

```sh
docker compose config --quiet
docker compose build
./scripts/docker-restore.sh \
  "$HOME/Library/Application Support/veyra/backups/veyra-YYYYMMDD-HHMMSS.dump"
docker compose up --detach veyra console
```

`docker-restore.sh` refuses to merge into a database that already has public
tables. If a retry needs a clean database, stop the stack and move the named
volume aside or explicitly remove only `veyra-postgres-data` after confirming
that the verified dump is available.

## Verify the private stack

```sh
docker compose ps
docker compose logs --tail=100 veyra console postgres
docker compose exec -T veyra \
  curl -fsS http://127.0.0.1:8080/health
curl -fsS https://veyra-console.antonlabs.cc/api/status
```

Expected pre-cutover state:

- all three containers are healthy;
- the restored audit and runtime rows are visible in the console;
- `trading_enabled` is false;
- the broker reports disconnected until the tunnel and host MT4 move;
- ports 7801, 8080, and 5432 have no host publication.

## Backups

Create a verified host-side dump from the containerized database with:

```sh
./scripts/docker-backup.sh
```

The default archive directory remains
`~/Library/Application Support/veyra/backups`. The script writes a partial
archive, verifies its catalog, atomically renames it, and keeps fourteen local
generations by default. When `VEYRA_BACKUP_R2_BUCKET` and the existing
`VEYRA_WRANGLER_BIN` credentials are configured, it also uploads the verified
archive off-machine; an upload failure leaves the local recovery point intact.

Install the hybrid host agents after the stack is healthy:

```sh
./scripts/install-docker-launchd.sh
```

They start native MT4 at login and run the verified Docker backup once when
loaded and daily at 03:30. Docker's `unless-stopped` policy supervises the
container services; these agents do not start duplicate host copies of Veyra,
the console, PostgreSQL, or `cloudflared`. The installer refuses to continue if
any incompatible full-host Veyra agent is already loaded. Remove the two hybrid
agents with `./scripts/install-docker-launchd.sh --uninstall`.

The terminal LaunchAgent runs `scripts/watch-terminal.sh`: it checks the real
Wine `terminal.exe` process every fifteen seconds and reopens MetaTrader 4 if
the process exits. The launcher itself remains under launchd `KeepAlive`.

## Final tunnel cutover

The local cloudflared directory must contain `veyra-config.yml` plus the named
tunnel credential JSON. Its ingress origin is `http://veyra:7801`, not
localhost, because cloudflared and Veyra are separate containers.

1. Leave both live-trading switches and autopilot false.
2. Disarm and stop the old service and MT4 EA.
3. Create and copy one final verified database dump, then restore that dump into
   a fresh Docker PostgreSQL volume.
4. Stop the old `cloudflared` replica.
5. Start the new connector with `docker compose --profile tunnel up --detach`.
6. Launch host-native MT4 and confirm successful polls, `/ready`, `/status`, and
   console activity.
7. Arm only after explicit approval and after confirming the old machine cannot
   trade the account. Recompile the host EA with `VEYRA_EA_ALLOW_LIVE=true`
   (or turn on `InAllowLiveOrders` in the EA inputs) and restart MT4; the
   container never reads that variable. Then set `VEYRA_TRADING_ENABLED=true`
   and `VEYRA_AUTOPILOT_ENABLED=true` in `.env` and recreate the Veyra
   container, or arm them from the console. Check `GET /config` for
   overrides, since a console-saved value wins over `.env`.

Rollback is the reverse: keep the new switches false, stop the Docker tunnel,
stop the new MT4 terminal, and restart the old stack from its retained database.
