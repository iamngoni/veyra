# Getting started with Veyra

This guide takes you from an empty computer to a running Veyra, one stage at
a time. You can stop after any stage and still have something that works.

It assumes a Mac. The service and the console also run on Linux and Windows,
but the helper scripts for MetaTrader and for 24/7 running are Mac-only.

> **Read this first.** Veyra can place real trades on a real brokerage
> account. Everything that can move money starts switched off, and this
> guide keeps it off until the final stage. Use a **demo account** until
> you trust your setup, and **never run two copies of Veyra against the same
> account**: they would both trade.

## Contents

1. [What the pieces are](#1-what-the-pieces-are)
2. [Install the tools](#2-install-the-tools)
3. [Get the code](#3-get-the-code)
4. [Stage A: first run on your computer](#4-stage-a-first-run-on-your-computer)
5. [Stage B: keep a history (database)](#5-stage-b-keep-a-history-database)
6. [Stage C: connect MetaTrader 4](#6-stage-c-connect-metatrader-4)
7. [Stage D: the AI model and extras](#7-stage-d-the-ai-model-and-extras)
8. [Stage E: keep it running 24/7](#8-stage-e-keep-it-running-247)
9. [Stage F: turn on real trading](#9-stage-f-turn-on-real-trading)
10. [Everyday commands](#10-everyday-commands)
11. [Updating to a newer version](#11-updating-to-a-newer-version)
12. [For developers: checking your changes](#12-for-developers-checking-your-changes)
13. [Troubleshooting](#13-troubleshooting)

---

## 1. What the pieces are

| Piece | What it does | Needed for |
| --- | --- | --- |
| **The service** | The main program. It runs in the background, decides what to do, and checks every trade against fixed safety limits before anything is sent. | Everything |
| **The console** | A web page you open in your browser to watch the account, positions, activity, and settings. | Watching and controlling |
| **The database** (PostgreSQL) | Where Veyra keeps its history and your settings changes, so nothing is lost when it restarts. | Anything beyond a quick look |
| **MetaTrader 4** (MT4) | The trading app that is logged in to your broker. Veyra never sees your broker password; MT4 keeps it. | Seeing a real account |
| **The Veyra add-on for MT4** (`VeyraProbe`) | MT4 calls add-ons "Expert Advisors" (EAs). This one checks in with the service about once a second, asks "anything for me to do?", does it, and reports back. | Seeing a real account |
| **The tunnel** (Cloudflare Tunnel) | A secure public web address that forwards to your computer. MT4 add-ons can only reach normal `https://` addresses, so the tunnel is how the add-on reaches the service. It only carries the add-on's check-ins. | Seeing a real account |
| **The AI model** (through OpenRouter) | Suggests trades. Its suggestions still have to pass the safety limits. | Automatic trading |
| **Jev** (optional) | A second-opinion service that rates market conditions for the AI. | Optional |

Everything except the tunnel stays private to your computer. The console
and the service are never put on the internet.

---

## 2. Install the tools

Open the **Terminal** app (press `⌘ Space`, type "Terminal", press Enter).
You'll paste commands into it. Press Enter after each one.

**Apple's developer tools** (git, compilers):

```sh
xcode-select --install
```

**Homebrew**, a tool that installs other tools. Skip this if `brew --version`
already prints a version:

```sh
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
```

When it finishes, it may print two or three "Next steps" commands. Run them,
then close and reopen Terminal.

**Rust**, the language the service is written in. Accept the default choices
when asked:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Close and reopen Terminal afterwards. You don't need to pick a Rust version:
the project names the exact one it needs, and it downloads automatically the
first time you build.

**Node.js**, which runs the console (version 20 or newer; 22 is what the
project is tested with):

```sh
brew install node
```

Check that everything is there. Each command should print a version number:

```sh
git --version
cargo --version
node --version
```

You'll install PostgreSQL, the tunnel tool, and MetaTrader later, in the
stages that need them.

---

## 3. Get the code

Pick a place for the project and download it. This puts it in
`~/Developer/Projects/veyra` (`~` means your home folder):

```sh
mkdir -p ~/Developer/Projects
cd ~/Developer/Projects
git clone https://github.com/iamngoni/veyra.git
cd veyra
```

If the repository is private, you need to be given access first. If you use
SSH keys with GitHub, `git clone git@github.com:iamngoni/veyra.git` works too.

Every command from here on is run **inside the `veyra` folder** unless the
guide says otherwise. If you open a new Terminal window, go back there first:

```sh
cd ~/Developer/Projects/veyra
```

---

## 4. Stage A: first run on your computer

Goal: the service and console running, with nothing connected yet.

### 4.1 Create your settings file

Veyra reads its settings from a file called `.env` in the project folder.
It is a plain text file with one setting per line, written `NAME=value`.
Lines starting with `#` are notes. The project ships a template,
`.env.example`, with every setting explained.

Make your own copy and make it readable only by you:

```sh
cp .env.example .env
chmod 600 .env
```

`.env` will hold passwords and keys. Git already ignores it, so it won't be
uploaded by accident. Never paste it into a chat or an email.

As it comes, the file has everything optional switched off: no MetaTrader,
no AI model, no database, and trading off. The service starts with it
unchanged, so there's nothing to fill in yet. Each later stage tells you
which lines to fill in.

To open the file later, use TextEdit:

```sh
open -e .env
```

(Files starting with a dot are hidden in Finder; this command opens it
anyway. Any plain-text editor works.)

### 4.2 Start the service

```sh
set -a; source .env; set +a
cargo run -p veyra-service
```

The first line loads your settings into this Terminal window; the service
does not read `.env` on its own. The second line builds and starts the
service. The **first build takes several minutes** while it downloads and
compiles everything. Later starts take seconds.

When it's running you'll see lines of text that start with
`{"timestamp":…`. Those are its logs. Leave this window open. To stop the
service, click into the window and press `Ctrl C`.

### 4.3 Check it's alive

Open a **new Terminal tab** (`⌘ T`), go to the project folder, and run:

```sh
curl http://127.0.0.1:8080/health
```

You should see `{"status":"ok","service":"veyra","version":"0.1.0"}`. You can
also open these addresses in your browser:

| Address | What it tells you |
| --- | --- |
| <http://127.0.0.1:8080/health> | Is the program running at all? |
| <http://127.0.0.1:8080/ready> | Can it do its job? Lists the broker link and the database. |
| <http://127.0.0.1:8080/status> | The full picture: which connections are set up, the safety limits, the trading switches. No passwords are shown. |

At this stage `/ready` says `"status":"ready"`, with `"broker":"unconfigured"`
(MetaTrader isn't set up yet; that's Stage C) and `"audit":"disabled"` (no
database yet; that's Stage B).

`127.0.0.1` means "this computer". Nobody else can reach these addresses.

### 4.4 Start the console

In another new Terminal tab:

```sh
cd ~/Developer/Projects/veyra/console
npm install
npm run dev
```

`npm install` downloads the console's building blocks (only needed the first
time, and after updates). `npm run dev` starts it. Open
<http://localhost:3000> in your browser.

The console talks to the service for you, so **the service must be running
first**. Account and position panels stay empty until MT4 is connected.

To stop the console, press `Ctrl C` in its tab.

**Stage A done.** You have the service and console running on your computer.

---

## 5. Stage B: keep a history (database)

Without a database, Veyra forgets everything when it stops: the activity
history, settings you changed in the console, and its running counters. It's
fine to skip for a quick look, but set it up before connecting a real
account.

**Install PostgreSQL 17 and start it** (it will also start by itself when
the Mac boots):

```sh
brew install postgresql@17
brew services start postgresql@17
```

**Create an empty database called `veyra`:**

```sh
/opt/homebrew/opt/postgresql@17/bin/createdb veyra
```

**Tell Veyra where it is.** In `.env`, set:

```dotenv
VEYRA_DATABASE_URL=postgres://localhost/veyra
```

**Restart the service**: press `Ctrl C` in its tab, then run the two start
commands from [4.2](#42-start-the-service) again. Veyra creates its tables
automatically the first time. <http://127.0.0.1:8080/ready> should now show
`"audit":"ok"`.

If `VEYRA_DATABASE_URL` is set but the database isn't running, the service
refuses to start. This is deliberate: it will not trade without keeping a
record.

> **Using Docker instead?** If you already run Docker Desktop or OrbStack,
> this starts PostgreSQL in a container (pick your own password):
>
> ```sh
> docker run -d --name veyra-db --restart unless-stopped -e POSTGRES_DB=veyra -e POSTGRES_USER=veyra -e POSTGRES_PASSWORD=choose-a-password -p 127.0.0.1:5432:5432 -v veyra-db-data:/var/lib/postgresql/data postgres:17-alpine
> ```
>
> and the setting becomes
> `VEYRA_DATABASE_URL=postgres://veyra:choose-a-password@127.0.0.1:5432/veyra`.
> The daily-backup job in Stage E expects the Homebrew install, so prefer
> Homebrew if you plan to run 24/7 on this Mac.

---

## 6. Stage C: connect MetaTrader 4

Goal: MT4 logged in to your broker, with the Veyra add-on checking in with
the service.

You'll need:

- a broker account that supports MetaTrader 4 (start with a **demo** account);
- a free [Cloudflare](https://dash.cloudflare.com/sign-up) account, and a
  domain name managed by Cloudflare. The tunnel gets an address on it, such
  as `ea.yourdomain.com`. Replace `ea.yourdomain.com` with your own address
  everywhere below.

### 6.1 Install MetaTrader 4

Install MetaTrader 4 for Mac (your broker usually provides a download) so it
ends up in `/Applications/MetaTrader 4.app`. Open it once and log in to your
account.

### 6.2 Switch on Veyra's MetaTrader link

The add-on proves who it is with a password that you make up and that both
sides know. Run this in Terminal; it prints a long random string:

```sh
openssl rand -hex 24
```

In `.env`, set these two lines, pasting your string after `VEYRA_EA_TOKEN=`:

```dotenv
VEYRA_BROKER_PROVIDER=ea
VEYRA_EA_TOKEN=paste-your-string-here
```

Restart the service (`Ctrl C` in its tab, then the two commands from
[4.2](#42-start-the-service)). It now also listens for the add-on on port
7801, on this computer only. <http://127.0.0.1:8080/ready> says
`"status":"degraded"` with `"broker":"stale"` until MT4 checks in. That's
expected; it changes at the end of this stage.

### 6.3 Set up the tunnel

Install the tunnel tool and log in to Cloudflare (a browser window opens;
choose your domain):

```sh
brew install cloudflared
cloudflared tunnel login
```

Create a tunnel named `veyra`. It prints a **tunnel ID**, a long string like
`6f2a…-…`; you'll need it in a moment:

```sh
cloudflared tunnel create veyra
```

Give the tunnel its public address:

```sh
cloudflared tunnel route dns veyra ea.yourdomain.com
```

Create the tunnel's settings file:

```sh
open -e ~/.cloudflared/veyra-config.yml
```

If TextEdit says the file doesn't exist, create it with
`touch ~/.cloudflared/veyra-config.yml` and run the command above again.
Paste this in, replacing the three placeholders (your Mac username is what
`whoami` prints in Terminal):

```yaml
tunnel: veyra
credentials-file: /Users/YOUR-MAC-USERNAME/.cloudflared/YOUR-TUNNEL-ID.json
ingress:
  - hostname: ea.yourdomain.com
    service: http://127.0.0.1:7801
  - service: http_status:404
```

This forwards `ea.yourdomain.com` to the add-on door on your computer (port
7801) and nothing else. Keep the file name and the tunnel name `veyra`: the
24/7 setup in Stage E looks for exactly these.

Start the tunnel in its own Terminal tab and leave it running:

```sh
cloudflared tunnel --config ~/.cloudflared/veyra-config.yml run veyra
```

**Check the tunnel reaches Veyra** (with the service running):

```sh
curl -s -o /dev/null -w '%{http_code}\n' -X POST https://ea.yourdomain.com/ea/poll
```

`400` is the answer you want. It means the request reached Veyra, which
rejected it because it was empty. A number in the `500`s means the tunnel
isn't running or can't reach the service.

### 6.4 Work around a MetaTrader-on-Mac quirk

MetaTrader for Mac runs inside a compatibility layer (Wine) that can't fall
back from the newer IPv6 kind of internet address to the older IPv4 kind.
Without this step, its requests fail instantly. The fix is to tell the Mac
which IPv4 address to use for your tunnel.

Look up the address:

```sh
dig +short A ea.yourdomain.com
```

Copy the first line it prints (four numbers separated by dots). Open the
system's address list:

```sh
sudo nano /etc/hosts
```

Enter your Mac password when asked (nothing appears as you type). Use the
arrow keys to go to the end of the file and add a line with the address
and your tunnel name, for example:

```text
104.21.10.20 ea.yourdomain.com
```

Save with `Ctrl O`, then `Enter`, and exit with `Ctrl X`.

### 6.5 Build the add-on and install it into MT4

In `.env`, set the address the add-on should call:

```dotenv
VEYRA_EA_URL=https://ea.yourdomain.com/ea/poll
```

Leave `VEYRA_EA_ALLOW_LIVE=false` as it is, so the add-on can't place real
orders yet. Stage F covers turning it on.

Then build it. This fills your address, password, and live-orders choice into
the add-on and installs it into MT4:

```sh
set -a; source .env; set +a
./scripts/compile_ea.sh
```

It should end with `EA compiled disarmed` and the path of the installed file.
If it says `MT4 installation not found`, MT4 is installed somewhere other than
the usual place, or hasn't been opened once yet.

> **MT4 on Windows, or elsewhere?** The script only knows the Mac layout. Do
> it by hand instead: open `ea/VeyraProbe.mq4` in a text editor, replace
> `__VEYRA_URL__` with your tunnel address, `__VEYRA_TOKEN__` with your
> `VEYRA_EA_TOKEN`, and `__VEYRA_ALLOW_LIVE__` with `false`. Save the result
> into MT4's `MQL4/Experts` folder (in MT4: File → Open Data Folder), open it
> in MetaEditor, and press Compile. Don't commit that edited copy: it contains
> your password.

### 6.6 Allow the add-on in MT4 and attach it

In MetaTrader 4:

1. **Tools → Options → Expert Advisors.** Tick **Allow automated trading** and
   **Allow WebRequest for listed URL**. Add your tunnel address to the list:
   `https://ea.yourdomain.com` (adding the full `…/ea/poll` address as well
   does no harm). Click OK.
2. Open a chart for the instrument you want (for example EURUSD).
3. Open the **Navigator** panel (View → Navigator), expand **Expert
   Advisors**, and drag **VeyraProbe** onto the chart. Click OK in the window
   that appears.
4. Make sure the **AutoTrading** button in the toolbar is on (green).

A small face in the chart's top-right corner means the add-on is running. Its
messages appear in the **Experts** tab of the Terminal panel at the bottom
(View → Terminal).

**Whenever you rebuild the add-on, restart MT4** (or remove it from the
chart and drag it on again). MT4 keeps using the old copy otherwise.

### 6.7 Check the connection

Within a few seconds:

- <http://127.0.0.1:8080/ready> shows `"status":"ready"` and
  `"broker":"connected"`;
- the console shows your account.

Ask the add-on for a fresh account report:

```sh
curl -X POST http://127.0.0.1:8080/commands/account_snapshot
```

**Stage C done.** Veyra can see your account. It still cannot trade.

---

## 7. Stage D: the AI model and extras

All of these go in `.env`. Restart the service after changing `.env`.

### The AI model (needed for automatic trading)

Veyra asks an AI model for trade ideas through
[OpenRouter](https://openrouter.ai), a service that gives one account access
to many AI models. Create an account, add some credit, and create an API key.

Veyra uses three models: a quick one, a middle one, and a slow careful one.
Choose each on OpenRouter's model list. **Each model must support "tool
calling"** (OpenRouter shows this on the model's page), because Veyra
requires answers in a fixed format. Models are written as `vendor/model`.

```dotenv
VEYRA_MODEL_PROVIDER=openrouter
VEYRA_MODEL_API_KEY=your-openrouter-key
VEYRA_MODEL_FAST=vendor/quick-model
VEYRA_MODEL_BALANCED=vendor/middle-model
VEYRA_MODEL_REASONING=vendor/careful-model
```

If you pick a "thinking" model and every decision fails with an error about
tool choice, add `VEYRA_MODEL_COMPEL_STRUCTURED=false`: those models refuse
to be forced into the fixed format, but still follow it when offered.

OpenRouter isn't the only option. OpenAI, Anthropic, Groq, DeepSeek, xAI,
Mistral, Kimi, Z.AI, a local Ollama, or a ChatGPT or Claude subscription
connected in the console all work too. The comments above
`VEYRA_MODEL_PROVIDER` in `.env.example` list them.

Put a ceiling on spending, so a mistake can't run up a bill:

```dotenv
VEYRA_MODEL_MAX_CALLS_PER_HOUR=60
VEYRA_MODEL_MAX_CALLS_PER_DAY=500
```

`VEYRA_MODEL_FALLBACKS` lists up to four backup models to try when the main
one fails, separated by commas. It's optional; `.env.example` shows an
example.

To prove the model answers correctly (this makes one real, paid request):

```sh
set -a; source .env; set +a
cargo test --test model_live -- --ignored --nocapture
```

If you set only some of the model lines, the service refuses to start and
names the one that's missing. That's on purpose: half a setup is treated as
a mistake, not as "off".

### Jev (optional)

Fill in your key. The other Jev lines can stay empty; they then use the
standard values.

```dotenv
VEYRA_JEV_API_KEY=your-jev-key
```

Once Jev is set up, the autopilot pauses whenever Jev is unavailable instead
of trading without it. Set `VEYRA_RISK_ALLOW_TRADING_WITHOUT_JEV=true` if
you'd rather it carried on.

### Market prices

Lets the console show price candles and gives the autopilot its market data,
read through MT4:

```dotenv
VEYRA_MARKET_PROVIDER=ea
```

### News calendar

Makes Veyra aware of scheduled economic news, and blocks new trades for an
instrument from 30 minutes before to 30 minutes after a high-impact event that
affects it. It needs no account or key:

```dotenv
VEYRA_CALENDAR_PROVIDER=forexfactory
```

`VEYRA_RISK_CALENDAR_BLACKOUT_MINUTES` changes the 30 minutes. If the
calendar can't be reached, Veyra refuses new trades rather than trading
without knowing what news is coming.

### Safety limits

These are checked for every trade, and the AI can't override them. The most
important ones:

| Setting | Meaning |
| --- | --- |
| `VEYRA_RISK_SYMBOLS` | Which instruments may be traded, for example `EURUSD,GBPUSD`. **Empty means nothing can be traded.** |
| `VEYRA_RISK_MAX_VOLUME_PER_ORDER` | Largest size of one trade, in lots. `0.01` is the smallest normal size. |
| `VEYRA_RISK_MAX_TOTAL_LOTS` | Largest total size across all open trades. |
| `VEYRA_RISK_MAX_OPEN_ORDERS` | Most trades open at once. |
| `VEYRA_RISK_SESSION_HOURS_UTC` | Only trade between these hours (UTC), for example `7-21`. |
| `VEYRA_RISK_MAX_DAILY_LOSS_PERCENT` | Stop opening trades for the rest of the day once the account is down this much since the day began (default 10%). |
| `VEYRA_RISK_KILL_SWITCH` | `true` blocks every trade, immediately. |

Start small: `0.01` lots and one open trade. `.env.example` explains every
other setting, including the per-trade risk cap, the drawdown limit, and what
happens to open trades over the weekend. Most limits can also be changed
while running, in the console's **Risk** tab.

### Notifications (recommended)

Veyra can message you when something important happens: trades opened or
closed, a loss limit reached, the connection dropping, repeated failures. It
can send to email, Telegram, Discord, Slack, ntfy (free phone notifications),
Pushover, or any webhook, and to several at once.

Notifications include passwords for those services, so Veyra stores them
encrypted. That needs the database (Stage B) and two values in `.env`: an
encryption key, and an operator password you type into the console to save
changes. Generate each with the command next to it and paste it in:

```sh
openssl rand -base64 32
```

```sh
openssl rand -hex 32
```

```dotenv
VEYRA_CONSOLE_SECRET_KEY=paste-the-first-output-here
VEYRA_CONSOLE_ADMIN_TOKEN=paste-the-second-output-here
```

Keep the encryption key the same from then on: if it changes, Veyra can no
longer read what it saved. Restart the service.

Then open the console's **Notifications** tab, pick a channel, follow its
"How to set up" steps, save, and press **Send test**.
[docs/notifications.md](docs/notifications.md) has the same guides and the
full list of events.

A small watchdog, started as part of the 24/7 setup (Stage E), also tells you
if Veyra itself stops responding.

### The autopilot

The autopilot is the loop that trades by itself: every few minutes it gathers
prices, news, and (optionally) Jev's view, asks the AI for one decision,
checks it against the safety limits, and passes it on. It needs the broker,
market prices, and the AI model set up.

```dotenv
VEYRA_AUTOPILOT_ENABLED=true
```

With the trading switch still off (see Stage F), the autopilot thinks and
records its decisions but can't place a trade. That's a good way to watch
what it would do before trusting it.

---

## 8. Stage E: keep it running 24/7

So far everything runs in Terminal tabs and stops when you close them or
restart the Mac. For unattended running, use the Mac setup below.

> **Only one copy per account.** If Veyra already runs on another machine
> against the same account, stop it there first:
> `./scripts/install-launchd.sh --uninstall` in that machine's copy.

### The Mac setup (recommended)

This uses macOS's built-in background-job system (launchd). It keeps the
service, tunnel, and console running, restarts them if they crash, opens MT4
when you log in, runs a watchdog that tells you if Veyra stops responding,
trims logs hourly, and backs the database up daily.

It expects everything from Stages B and C to be in place, set up the way
this guide did it: PostgreSQL 17 from Homebrew, `cloudflared` from Homebrew,
the tunnel named `veyra` with its settings in `~/.cloudflared/veyra-config.yml`,
and MT4 in `/Applications`.

**1. Close the manual copies.** Press `Ctrl C` in the service, console, and
tunnel tabs. (The installer also stops any it finds.)

**2. Build the fast, final versions** of the service and the console:

```sh
cargo build --release
```

```sh
(cd console && npm install && npm run build)
```

**3. Check the Mac has everything.** This only looks and reports:

```sh
./scripts/preflight.sh
```

Fix anything marked `[fail]` until it ends with `PREFLIGHT_OK`. Lines marked
`[note]` are information only.

**4. Install and start the background jobs:**

```sh
./scripts/install-launchd.sh
```

**5. Check it's up**, as before:

- <http://127.0.0.1:8080/ready> shows `ready` and `connected`;
- the console is at <http://127.0.0.1:3000>.

The console is only reachable from this Mac. To reach it from elsewhere, use a
private network such as [Tailscale](https://tailscale.com); never put it on
the public internet.

**Where things are kept:**

| What | Where |
| --- | --- |
| Logs | `~/Library/Logs/veyra/` (for example `service.out.log`, `tunnel.out.log`) |
| Database backups | `~/Library/Application Support/veyra/backups/` |

For a backup right now: `./scripts/backup-postgres.sh`. To also copy backups
off the machine, see `VEYRA_BACKUP_R2_BUCKET` in `.env.example`.

**After changing `.env` or the code**, rebuild and restart the service:

```sh
cargo build --release && launchctl kickstart -k gui/$(id -u)/cc.antonlabs.veyra.service
```

**To remove the background jobs** and stop everything:

```sh
./scripts/install-launchd.sh --uninstall
```

### Docker

The repository also has a Docker setup (`compose.yaml`) that runs the
service, console, database, and tunnel in containers, while MT4 stays a
normal Mac app. It's tailored to one particular home server: it plugs into
an existing web gateway (Traefik) on a Docker network called
`media-server_default`, and has that server's addresses and paths as
defaults. Use it only if you're comfortable adapting `compose.yaml`. Follow
[`docs/deployment-docker.md`](docs/deployment-docker.md), and don't run it
alongside the Mac setup above.

Moving an existing install to a new machine is covered in
[`docs/deployment-mac-mini.md`](docs/deployment-mac-mini.md).

---

## 9. Stage F: turn on real trading

Real orders need **two separate switches**, both on. This is deliberate: one
mistake can't move money on its own.

| Switch | Where | Effect when off |
| --- | --- | --- |
| **Service switch** | The **Execution** switch in the console's **Settings** tab, or `VEYRA_TRADING_ENABLED` in `.env` | The service refuses to send any order. |
| **Add-on switch** | `VEYRA_EA_ALLOW_LIVE` when building the add-on, or the add-on's `InAllowLiveOrders` setting in MT4 (right-click the chart → Expert Advisors → Properties → Inputs) | MT4 checks each order and reports what *would* have happened, without placing it. |

On top of that, the instrument must be listed in `VEYRA_RISK_SYMBOLS` and
pass every other safety limit.

**Before switching on, check:**

- [ ] You've watched the autopilot's decisions with trading off, and they make
      sense.
- [ ] You're on a demo account, or you accept the risk on a real one.
- [ ] Sizes are small (`0.01` lots, one open trade).
- [ ] Notifications reach you.
- [ ] Only one copy of Veyra can reach this account.
- [ ] You know how to stop it (below).

**To switch on:**

1. Set `VEYRA_EA_ALLOW_LIVE=true`, rebuild the add-on
   ([6.5](#65-build-the-add-on-and-install-it-into-mt4)), and restart MT4. In
   the add-on's window in MT4, tick **Allow live trading** on the Common tab.
2. Turn on the service switch: the **Execution** switch in the console's
   **Settings** tab. Or set `VEYRA_TRADING_ENABLED=true` in `.env` and
   restart the service, or change it while it runs with:

   ```sh
   curl -X POST http://127.0.0.1:8080/config -H 'Content-Type: application/json' -d '{"VEYRA_TRADING_ENABLED": true}'
   ```

   Send the same command with `false` to turn it off again.

**Changes made while running win over `.env`.** A setting changed through the
console or the command above is saved (with a database set up) and used on
every later start, even if `.env` says otherwise. Change it back the same
way. <http://127.0.0.1:8080/config> lists every such setting; `"overridden":
true` marks the ones that no longer follow `.env`.

**Profit protection is on by default.** While the autopilot runs with
trading on, `VEYRA_AUTOPILOT_PROFIT_HARVEST` moves stop-losses on open
trades and closes a trade that is giving back its profit. It can be turned
off while running, under **Profit harvesting** in the console's **Settings**
tab.

### Emergency stop

Any one of these stops new trades. Fastest first:

1. Turn on the **kill switch** in the console's **Risk** tab, or run:

   ```sh
   curl -X POST http://127.0.0.1:8080/risk/policy -H 'Content-Type: application/json' -d '{"killSwitch": true}'
   ```

2. Turn off the **Execution** switch in the console's **Settings** tab.
3. In MT4, remove VeyraProbe from the chart, or turn off AutoTrading.
4. Stop everything: `./scripts/install-launchd.sh --uninstall`.

Stopping Veyra does not close trades that are already open. Close them in
MT4 if you need to.

---

## 10. Everyday commands

| Task | Command |
| --- | --- |
| Start the service by hand | `set -a; source .env; set +a` then `cargo run -p veyra-service` |
| Start the console by hand | `cd console && npm run dev` |
| Is it alive? | `curl http://127.0.0.1:8080/health` |
| Is it ready? | `curl http://127.0.0.1:8080/ready` |
| Full status | `curl http://127.0.0.1:8080/status` |
| Refresh the account from MT4 | `curl -X POST http://127.0.0.1:8080/commands/account_snapshot` |
| Stop all new trades (kill switch) | `curl -X POST http://127.0.0.1:8080/risk/policy -H 'Content-Type: application/json' -d '{"killSwitch": true}'` |
| Settings changed while running | `curl http://127.0.0.1:8080/config` |
| Watch the service log (24/7 setup) | `tail -f ~/Library/Logs/veyra/service.out.log` |
| Restart the service (24/7 setup) | `launchctl kickstart -k gui/$(id -u)/cc.antonlabs.veyra.service` |
| Back up the database now | `./scripts/backup-postgres.sh` |

---

## 11. Updating to a newer version

```sh
git pull
```

```sh
cargo build --release
```

```sh
(cd console && npm install && npm run build)
```

```sh
./scripts/install-launchd.sh
```

Re-running the installer restarts every background job with the new
versions. If `ea/VeyraProbe.mq4` changed, rebuild the add-on
([6.5](#65-build-the-add-on-and-install-it-into-mt4)) and restart MT4.
Database changes apply automatically when the service starts.

---

## 12. For developers: checking your changes

Every change must pass these before it's committed (see
[`AGENTS.md`](AGENTS.md) and [`CONTRIBUTING.md`](CONTRIBUTING.md)). The
coverage check needs one extra tool, installed once:

```sh
rustup component add llvm-tools-preview
```

```sh
cargo install cargo-llvm-cov
```

Then run everything, service and console, in one go:

```sh
./scripts/check.sh
```

It ends with `CHECK_OK` when everything passes. To start the real program,
call each status address, and check it shuts down cleanly:

```sh
./scripts/smoke.sh
```

Tests that talk to real outside services (the AI model, Jev, MT4, the
database) are skipped by default. Each runs on request, with `.env` loaded:

```sh
cargo test --test model_live -- --ignored --nocapture
```

The others work the same way: `jev_live`, `ea_channel_live` (MT4 must be
connected), and `store_live` (the database must be running).

How the system is designed is covered in
[`docs/architecture.md`](docs/architecture.md), and the reasoning behind key
choices in [`docs/decisions/`](docs/decisions/).

---

## 13. Troubleshooting

### The service stops straight away

It prints one line starting with `Error:`. That line names the problem.

| Message contains | What it means | Fix |
| --- | --- | --- |
| `"VEYRA_BIND_HOST"` | The settings weren't loaded in this Terminal window. | Run `set -a; source .env; set +a` first, from the project folder. |
| `"VEYRA_EA_TOKEN"` | The MT4 add-on password is empty, or has invalid characters. | Fill it in ([6.2](#62-switch-on-veyras-metatrader-link)). It must be 16–128 letters, digits, `-` or `_`. |
| `"VEYRA_BROKER_PROVIDER"` | Some `VEYRA_EA_…` lines are filled in but the provider line is empty. | Set `VEYRA_BROKER_PROVIDER=ea`, or empty the other lines. |
| `"VEYRA_MODEL_API_KEY"` | Some AI model lines are filled in but not the key. | Add the key, or empty every `VEYRA_MODEL_…` line. |
| `"VEYRA_JEV_API_KEY"` | Some Jev lines are filled in but not the key. | Add the key, or empty every `VEYRA_JEV_…` line. |
| `console credential storage requires VEYRA_DATABASE_URL` | The encryption key and operator password are set, but there's no database. | Set up the database (Stage B), or empty both lines. |
| `VEYRA_CONSOLE_SECRET_KEY and VEYRA_CONSOLE_ADMIN_TOKEN must both be configured` | Only one of the two is set, or the password is too short. | Set both, using the commands in Stage D's Notifications section. |
| `InvalidEnvironmentVariable` | A setting has a value Veyra doesn't accept. The message names it and says what's allowed. | Correct that line. `.env.example` explains each one. |
| `Storage` … `connect failed` | The database isn't running, or `VEYRA_DATABASE_URL` is wrong. | `brew services start postgresql@17`, and check the URL. |
| `Address already in use` | Another program, or another copy of Veyra, is using the port. | Stop the other copy. If the 24/7 setup is installed, it's already running. |

### `/ready` says "degraded"

Look at the other words on that line:

- `"broker":"stale"`: MT4 isn't checking in. Check that MT4 is open, the
  add-on is on the chart with its face showing, AutoTrading is on, and the
  tunnel is running.
- `"audit":"unavailable"`: the database stopped answering. Check that
  PostgreSQL is running, then the service log.

### MT4's Experts tab shows errors

- **`VeyraProbe webrequest error=4060`**: MT4 isn't allowed to contact the
  address. Add your tunnel address in Tools → Options → Expert Advisors
  ([6.6](#66-allow-the-add-on-in-mt4-and-attach-it)).
- **Other `webrequest error=` numbers, straight away** on a Mac: add the
  IPv4 line to `/etc/hosts` ([6.4](#64-work-around-a-metatrader-on-mac-quirk)).
- **No errors, but the service never shows MT4 as connected**: the add-on was
  probably built with a different `VEYRA_EA_TOKEN` than the service is using,
  and the service is turning it away. Rebuild the add-on and restart MT4.
- **Your change didn't take effect**: MT4 is still using the old add-on.
  Restart MT4.

### The console is blank or shows errors

- The service must be running before the console can show anything. Check
  <http://127.0.0.1:8080/health>.
- If `npm run dev` says port 3000 is in use, start it on another port:
  `npx vite dev --port 3001`, then open <http://localhost:3001>.

### Where to look for more detail

- Service started by hand: its own Terminal tab.
- 24/7 setup: `~/Library/Logs/veyra/`, one file per part
  (`service.err.log` and `service.out.log`, `tunnel.out.log`, and so on).
- MT4: the Experts and Journal tabs in its Terminal panel.
