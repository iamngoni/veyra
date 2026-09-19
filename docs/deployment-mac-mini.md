# Veyra deployment on the home Mac mini

This is the cutover runbook for moving the current Veyra instance to the
always-on Mac mini.

The canonical broker/EA endpoint is:

```text
https://veyra.antonlabs.cc/ea/poll
```

`veyra.antonlabs.cc` is the Cloudflare Tunnel hostname. It is not the console
hostname and it must not be changed to an unrelated Anton Labs domain. The
console and diagnostic API remain private and should be reached over Tailscale
or locally on the Mac mini.

## Recommended layout

Keep MT4 on the macOS host. It owns the broker session, the account login, the
attached `VeyraProbe` expert, and the WebRequest permission. Veyra can run
either directly under launchd (the current, best-supported layout) or in Docker.

| Component | Mac mini location | Network boundary |
| --- | --- | --- |
| MT4 + `VeyraProbe` | native host application | calls the HTTPS tunnel URL |
| Rust Veyra service | launchd or Docker | EA listener on port 7801; API on 8080 |
| Console | launchd or Docker | private, port 3000 |
| PostgreSQL | Homebrew or Docker volume | private; never public |
| `cloudflared` | launchd or Docker | `veyra.antonlabs.cc` → EA listener only |

For first cutover, use launchd for all components. It matches the already
proven setup and makes MT4, the tunnel, backups, and the service restart as one
Mac login-managed stack. Move only the service/console/database into Docker
after the host cutover has been verified.

## 1. Create a current-state backup on the old Mac

Run this from the current checkout:

```sh
cd ~/Developer/Projects/veyra
./scripts/backup-postgres.sh
```

The script creates and verifies a custom PostgreSQL archive under:

```text
~/Library/Application Support/veyra/backups/veyra-YYYYMMDD-HHMMSS.dump
```

Choose the newest archive and copy it to the Mac mini over SSH or an encrypted
external drive. Do not send it through chat or email.

The database contains the audit trail and durable runtime state, including
JeV/model usage counters, equity baselines, stop basis, and the console-edited
risk policy. It does not contain the MT4 terminal session or provider secrets.

Also transfer these items securely, separately from Git:

- `.env` (mode `0600`), after checking that it contains no stale broker
  password;
- `~/.cloudflared/veyra-config.yml` and the tunnel credentials JSON;
- the MT4 terminal installation/profile and the compiled `VeyraProbe.ex4`, or
  the EA source if MetaEditor will compile it on the target;
- any host-specific Tailscale setup.

The EA token, Jev key, OpenRouter key, and webhook token are secrets. Rotate
them after migration if the transfer path was not fully trusted.

## 2. Prepare the Mac mini

On the Mac mini:

```sh
brew install postgresql@17 cloudflared node
brew services start postgresql@17
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
mkdir -p ~/Developer/Projects
git clone git@github.com:iamngoni/veyra.git ~/Developer/Projects/veyra
cd ~/Developer/Projects/veyra
npm --version
cargo --version
```

Install MT4 on the host, launch it once, sign in, attach `VeyraProbe` to the
intended chart, and add this exact URL to MT4's allowed WebRequest URLs:

```text
https://veyra.antonlabs.cc
```

The host does not need to expose ports 7801 or 8080 to the LAN. The tunnel is
the only public-facing path, and it carries only `/ea/poll`.

## 3. Restore PostgreSQL state

Create the empty database and restore the newest verified dump:

```sh
createdb veyra
/opt/homebrew/opt/postgresql@17/bin/pg_restore \
  --dbname=veyra --no-owner \
  ~/Library/Application\ Support/veyra/backups/veyra-YYYYMMDD-HHMMSS.dump
```

Set the target `.env` database URL to a local connection, for example:

```dotenv
VEYRA_DATABASE_URL=postgres://localhost/veyra
```

If the database already exists, stop Veyra first and restore into a newly
created empty database rather than merging archives into live tables. The
service runs migrations at startup, so a fresh database is also valid; the
restore is what preserves the current audit and runtime state.

## 4. Configure the tunnel

Copy the existing tunnel config and credentials to the same paths on the Mac
mini, or create them once with the Cloudflare account:

```sh
mkdir -p ~/.cloudflared
cloudflared tunnel login
cloudflared tunnel list
cloudflared tunnel route dns veyra veyra.antonlabs.cc
```

The ingress must remain equivalent to:

```yaml
tunnel: veyra
credentials-file: /Users/<mac-mini-user>/.cloudflared/<tunnel-id>.json
ingress:
  - hostname: veyra.antonlabs.cc
    service: http://127.0.0.1:7801
  - service: http_status:404
```

Only one machine may run this named tunnel during cutover.

## 5. Install and verify the host stack

Copy `.env` into the checkout and protect it:

```sh
cp /secure/location/veyra.env .env
chmod 600 .env
```

Before starting live execution, use safe switches:

```dotenv
VEYRA_TRADING_ENABLED=false
VEYRA_EA_ALLOW_LIVE=false
```

Build and run the checks:

```sh
cargo build --release
(cd console && npm install && npm run build)
./scripts/preflight.sh
./scripts/install-launchd.sh
curl -fsS http://127.0.0.1:8080/ready
curl -fsS http://127.0.0.1:8080/status
```

Confirm that readiness reports the EA/broker link, the console shows restored
audit/runtime state, and the MT4 journal shows successful polls. Check the
console locally at `http://127.0.0.1:3000/`; for remote access use the Mac
mini's Tailscale address or a Tailscale Serve configuration. Do not put port
8080 or 3000 into the Cloudflare EA tunnel.

After the safe path is proven, live execution requires both deliberate switches:

```dotenv
VEYRA_TRADING_ENABLED=true
VEYRA_EA_ALLOW_LIVE=true
```

Recompile/reinstall the EA if its live-order input is compile-time injected,
then restart the service and terminal. Never run both old and new machines
armed against the same account.

## Docker: yes, with MT4 on the host

This is technically sound, but it is a hybrid deployment. The Dockerized
service cannot access a host MT4 process through `127.0.0.1`; use the public
Cloudflare hostname from MT4 and keep the tunnel and service on the same Docker
network. In that layout:

- Veyra binds to `0.0.0.0` inside its container;
- PostgreSQL uses a named persistent volume;
- `cloudflared` routes `veyra.antonlabs.cc` to `http://veyra:7801`;
- the console proxies `/api` to `http://veyra:8080` inside the Compose network;
- only the console is optionally published to the Mac loopback interface;
- MT4 remains native on macOS and continues calling
  `https://veyra.antonlabs.cc/ea/poll`.

Do not reuse the host launchd agents for the same ports when Docker Compose is
running. Also keep backups outside the database container and test restoring a
dump into a fresh PostgreSQL volume before relying on Docker for recovery.

The current repository's launchd path is the deployment reference. Docker
packaging should be introduced as a separate, verified deployment profile; it
must not change the EA contract or expose the diagnostic API publicly.

## Cutover and rollback

1. On the old Mac, disarm the EA and stop its Veyra launch agents.
2. Confirm the old tunnel is stopped.
3. Start the Mac mini stack with live switches false.
4. Verify `/ready`, `/status`, the console, MT4 polls, and restored audit state.
5. Arm the Mac mini only after those checks pass.
6. If anything is wrong, disarm and stop the Mac mini stack, then restart the
   old machine. The database archive and R2 copy remain recovery points.

Useful logs on the host deployment:

```text
~/Library/Logs/veyra/service.out.log
~/Library/Logs/veyra/service.err.log
~/Library/Logs/veyra/tunnel.out.log
~/Library/Logs/veyra/backup.out.log
```

