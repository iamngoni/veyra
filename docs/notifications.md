# Notifications

Veyra can message you when something needs your attention, or just to keep
you informed. It sends to every channel you switch on: email, Telegram,
Discord, Slack, ntfy, Pushover, and any webhook.

Everything is set up in the console's **Notifications** tab. You need the
operator token (`VEYRA_CONSOLE_ADMIN_TOKEN` in `.env`) to save, because the
channel settings include secrets. Secrets are encrypted before they are
stored and are never shown again; the console only shows the last four
characters so you can tell which one is saved.

Notifications need console secret storage (`VEYRA_CONSOLE_SECRET_KEY` and
`VEYRA_CONSOLE_ADMIN_TOKEN`) and the database. Without them the panel is shown
disabled.

## What you get notified about

Every event can be switched off on its own. All are on by default.

| Event | When |
| --- | --- |
| Breaker tripped | The daily loss or peak drawdown limit starts blocking new entries, and again when it clears. |
| Trading halted | The kill switch is engaged or execution is switched off, and again when trading is re-armed. |
| Broker link | The terminal stops reporting for about a minute, and again when it is back. |
| Reconciliation drift | The broker has positions Veyra does not manage. The same drift is repeated at most every 6 hours. |
| Order failed | The terminal fails or rejects an open, close, or modify. |
| Model trouble | The autopilot fails to get a decision 3 times in a row, and again when it recovers. |
| Service down | The watchdog cannot reach Veyra, or Veyra has lost its database (see below). |
| Trade opened | A position was filled, with the stop, the target, and the model's reasoning. |
| Trade closed | A position closed, with its result and why (agent, harvest, or at the broker). |
| Daily summary | Once a day at the hour you pick: trades closed, wins, losses, net result, balance. |

Conditions are reported when they **change**, not on every check, so a
problem that persists produces one message, not one a minute.

## How delivery works

- Sending never slows trading down. A notification is put on a queue, and a
  background worker delivers it to every enabled channel at the same time.
- Each delivery gets up to 3 attempts when the failure is temporary (timeout,
  rate limit, server error). Wrong credentials or a bad address fail at once.
- If the queue ever fills up (256 waiting), new notifications are dropped
  and counted rather than held.
- The panel shows how many were delivered, failed, or dropped, and the
  last few deliveries with any error.
- **Send test** on a channel sends a test message right away using its saved
  settings, so save before testing.

## Setting up each channel

### Telegram

1. In Telegram, open **@BotFather**, send `/newbot`, and follow the prompts.
   Copy the **bot token** it gives you.
2. Send your new bot any message (or add it to a group or channel).
3. Find the **chat id**: open
   `https://api.telegram.org/bot<TOKEN>/getUpdates` in a browser and read
   `message.chat.id`. Group and channel ids are negative, e.g. `-100…`. For a
   public channel where the bot is an admin you can use `@channelname`.
4. Paste both into the Telegram card, switch it on, save, and send a test.

### Discord

1. **Server Settings → Integrations → Webhooks → New Webhook.**
2. Pick the channel, then **Copy Webhook URL**
   (`https://discord.com/api/webhooks/…`).
3. Paste it into the Discord card.

### Slack

1. Go to **api.slack.com/apps → Create New App → From scratch.**
2. Open **Incoming Webhooks**, switch it on, then **Add New Webhook to
   Workspace** and pick a channel.
3. Copy the URL (`https://hooks.slack.com/services/…`) into the Slack card.

### ntfy (free phone notifications)

1. Install the ntfy app (iOS or Android), or use `https://ntfy.sh` in a
   browser.
2. Pick a hard-to-guess topic name, e.g. `veyra-7f3k2q`. Anyone who knows the
   topic can read it, so treat it like a password.
3. Subscribe to it in the app and paste the topic into the ntfy card.
4. Self-hosting ntfy? Set **Server**. The access token is only for protected
   topics.

Critical notifications arrive at ntfy's highest priority.

### Pushover

1. Create an account at pushover.net and install the app (a one-time
   purchase after a 30-day trial).
2. Your **User Key** is on the dashboard.
3. **Create an Application** to get an **API token**.
4. Paste both into the Pushover card. Critical notifications are sent at high
   priority.

### Email

Any SMTP server works. For Gmail:

1. Turn on 2-Step Verification for the account.
2. Create an app password at `myaccount.google.com/apppasswords`.
3. Fill in: host `smtp.gmail.com`, port `587`, security `starttls`, username
   your Gmail address, password the app password, from your address, and to
   one or more addresses separated by commas.

Port 465 uses `tls` instead of `starttls`. Username and password are set
together or not at all.

### Webhook

Any HTTPS endpoint that accepts a JSON `POST`:

```json
{
  "service": "veyra",
  "event": "trade_closed",
  "severity": "info",
  "title": "Closed GBPUSD short · −5.27",
  "body": "Closed at broker (stop, target or manual) · 0.01 lots · Ticket 10655087",
  "at": 1790341680
}
```

`severity` is `info`, `warning`, or `critical`. If you set a bearer token it
is sent as `Authorization: Bearer <token>`.

## The watchdog

The service sends everything above itself, which means it cannot tell you
if it has stopped. The watchdog is a separate process that checks Veyra's
`/ready` endpoint every minute. It sends **Service down** after 3 failed
checks in a row (the service not answering, or its database unavailable),
and a recovery message when things are back. It uses the same channels and
reads your settings from the database every 5 minutes.

- **Docker:** it runs as the `watchdog` service in `compose.yaml`, from the
  same image as Veyra. `docker compose up -d watchdog` starts it.
- **Native (launchd):** `scripts/install-launchd.sh` installs it as
  `cc.antonlabs.veyra.alerts`, running `scripts/run-service.sh watchdog`.

Optional settings, in `.env`:

| Setting | Default | Meaning |
| --- | --- | --- |
| `VEYRA_WATCHDOG_URL` | `http://veyra:8080` (Docker), `http://127.0.0.1:8080` (launchd) | Where to find the service. |
| `VEYRA_WATCHDOG_INTERVAL_SECS` | `60` | Seconds between checks (10 to 3600). |
| `VEYRA_WATCHDOG_FAILURES` | `3` | Failed checks in a row before alerting (1 to 60). |

The watchdog only runs on the same machine as Veyra, so it cannot report a
machine that is powered off or offline. For that, use an outside uptime
checker against your public endpoint.

## Replaced: `VEYRA_ALERT_WEBHOOK`

The old `scripts/alert-probe.py` and its `VEYRA_ALERT_WEBHOOK` setting are
gone. To keep the same destination, add it as a channel: an ntfy topic URL
`https://ntfy.sh/<topic>` becomes the ntfy channel with that topic, a Slack
or Discord webhook URL goes into that card. The old value in `.env` is
ignored and can be deleted.
