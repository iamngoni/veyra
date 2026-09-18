#!/usr/bin/env python3
"""Watches the supervised Veyra stack and pushes alerts to a webhook.

Run by launchd every two minutes. It reads the loopback control surface only,
compares against the previous run's state (kept in
`~/Library/Application Support/veyra/alert-state.json`), and posts one message
per run listing any findings:

- readiness transition (ready <-> degraded)
- terminal link, EA arming, and trading switch transitions
- autopilot failures repeated three ticks in a row
- audited events since the last cursor: service restarts, reconciliation
  drift, executed opens, and closed positions (enriched with the realized
  fill from `/performance`, so a close reports what it actually banked
  rather than the reconciler's last floating snapshot)

`VEYRA_ALERT_WEBHOOK` (sourced from `.env`) receives the findings. Slack and
Discord webhooks get the shared JSON shape (`text` + `content`); an ntfy
topic URL gets the text body directly with an `X-Title` and warning priority,
so the push reads as a notification rather than a JSON blob. With no webhook
configured, findings are printed for the supervised log instead.

Exit status is zero even when alerts fail: a notification problem must never
make launchd treat the probe itself as broken.
"""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

BASE = os.environ.get("VEYRA_ALERT_BASE", "http://127.0.0.1:8080")
STATE_PATH = Path(
    os.environ.get(
        "VEYRA_ALERT_STATE",
        str(Path.home() / "Library" / "Application Support" / "veyra" / "alert-state.json"),
    )
)
ENV_PATH = Path(os.environ.get("VEYRA_ENV_FILE", str(Path(__file__).resolve().parent.parent / ".env")))
UNAVAILABLE_ALERT_EVERY = 3
TIMEOUT = 5.0


def load_env() -> dict:
    values = {}
    if ENV_PATH.exists():
        for line in ENV_PATH.read_text().splitlines():
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, value = line.split("=", 1)
            values[key.strip()] = value.strip()
    return values


def http_json(path: str):
    request = urllib.request.Request(f"{BASE}{path}", headers={"accept": "application/json"})
    with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
        return json.loads(response.read())


def load_state() -> dict:
    if not STATE_PATH.exists():
        return {}
    try:
        return json.loads(STATE_PATH.read_text())
    except (OSError, ValueError):
        return {}


def save_state(state: dict) -> None:
    STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
    temporary = STATE_PATH.with_suffix(".tmp")
    temporary.write_text(json.dumps(state, indent=1, sort_keys=True))
    temporary.replace(STATE_PATH)


def post_webhook(url: str, text: str, severity: str) -> None:
    host = (urllib.parse.urlparse(url).hostname or "").lower()
    if "ntfy" in host:
        # ntfy publishes the raw body as the message; a JSON payload would
        # arrive as an unreadable blob, so send text with a title and let
        # warnings ring louder than routine findings.
        headers = {"content-type": "text/plain; charset=utf-8", "X-Title": "Veyra"}
        if severity == "warn":
            headers["X-Priority"] = "4"
            headers["X-Tags"] = "warning"
        request = urllib.request.Request(url, data=text.encode("utf-8"), headers=headers)
    else:
        body = json.dumps(
            {"text": text, "content": text, "source": "veyra", "severity": severity}
        ).encode()
        request = urllib.request.Request(
            url, data=body, headers={"content-type": "application/json"}
        )
    with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
        response.read()


def describe_event(event: dict) -> str | None:
    kind = event.get("kind")
    payload = event.get("payload") or {}
    if kind == "service_started":
        return f"Veyra service started (version {payload.get('version', '?')})"
    if kind == "reconciliation_drift":
        return (
            "Reconciliation drift: "
            f"unknown tickets {payload.get('unknownTickets')} "
            f"truncated={payload.get('positionsTruncated')}"
        )
    if kind == "command_completed" and payload.get("kind") == "open_order":
        result = payload.get("result") or {}
        if result.get("executed"):
            return f"Position opened: ticket {result.get('ticket')} (retcode {result.get('retcode')})"
        return None
    if kind == "proposal_evaluated":
        if payload.get("outcome") == "unavailable":
            return "proposal_unavailable"  # marker; aggregated as a streak
        return None
    return None


def close_message(close: dict, trade: dict | None) -> str:
    """One closed-position line: realized fill when the history answered,
    the reconciler's floating snapshot when it did not."""
    ticket = close.get("ticket")
    symbol = close.get("symbol")
    kind = close.get("kind")
    lots = close.get("lots")
    if trade is None:
        profit = float(close.get("profit", 0.0) or 0.0)
        return (
            f"Position closed: ticket {ticket} {symbol} {kind} {lots} lots, "
            f"P/L {profit:+.2f} (floating; realized fill unavailable)"
        )
    net = (
        float(trade.get("profit", 0.0) or 0.0)
        + float(trade.get("swap", 0.0) or 0.0)
        + float(trade.get("commission", 0.0) or 0.0)
    )
    result = "win" if net > 0 else ("loss" if net < 0 else "flat")
    return (
        f"Trade closed — {result}: {symbol} {kind} {lots} lots "
        f"{trade.get('openPrice')} → {trade.get('closePrice')}, net {net:+.2f}"
    )


def realized_performance() -> tuple[dict, dict | None]:
    """Closed fills by ticket plus the window report, from the venue
    history; empty on failure."""
    try:
        payload = http_json("/performance?days=7")
    except (OSError, ValueError):
        return {}, None
    trades = payload.get("trades")
    if not isinstance(trades, list):
        return {}, None
    fills = {
        trade.get("ticket"): trade
        for trade in trades
        if isinstance(trade, dict) and trade.get("ticket") is not None
    }
    report = payload.get("report")
    return fills, report if isinstance(report, dict) else None


def record_line(report: dict | None) -> str | None:
    """Running closed-trade record for the alert footer, when available."""
    if not report:
        return None
    trades = int(report.get("trades") or 0)
    if trades <= 0:
        return None
    wins = int(report.get("wins") or 0)
    losses = int(report.get("losses") or 0)
    net = float(report.get("net_profit") or 0.0)
    win_rate = float(report.get("win_rate_percent") or 0.0)
    return (
        f"Record ({trades} closed): {wins}W/{losses}L, "
        f"win rate {win_rate:.0f}%, net {net:+.2f}"
    )


def main() -> int:
    env = load_env()
    webhook = env.get("VEYRA_ALERT_WEBHOOK", "").strip()
    state = load_state()
    findings: list[str] = []
    severity = "info"
    errors: list[str] = []

    try:
        readiness = http_json("/ready")
        status = http_json("/status")
    except (OSError, ValueError) as error:
        # Covers connection failures and socket timeouts on every supported
        # Python (3.9's socket.timeout is not TimeoutError).
        message = f"Veyra stack unreachable: {error}"
        if state.get("readiness") != "unreachable":
            findings.append(message)
            severity = "warn"
        state["readiness"] = "unreachable"
        push(findings, webhook, severity)
        save_state(state)
        return 0

    if state.get("readiness") is not None and readiness.get("status") != state.get("readiness"):
        findings.append(
            f"Veyra readiness: {readiness.get('status')} "
            f"(broker {readiness.get('broker')}, audit {readiness.get('audit')})"
        )
        if readiness.get("status") != "ready":
            severity = "warn"
    state["readiness"] = readiness.get("status")

    controls = {
        "broker_connected": status.get("broker_connected"),
        "ea_live_orders": status.get("ea_live_orders"),
        "trading_enabled": status.get("trading_enabled"),
        "autopilot": (status.get("autopilot") or {}).get("enabled"),
    }
    labels = {
        "broker_connected": "terminal link",
        "ea_live_orders": "EA arming",
        "trading_enabled": "trading switch",
        "autopilot": "autopilot",
    }
    if state.get("controls"):
        for key, value in controls.items():
            if state["controls"].get(key) != value:
                state_word = "on" if value else "off"
                findings.append(f"Veyra {labels[key]} is now {state_word}")
                if not value and key != "trading_enabled":
                    severity = "warn"
    state["controls"] = controls

    # A configured off-machine backup that silently stops running is the
    # classic failure; the local marker is refreshed on every upload.
    if env.get("VEYRA_BACKUP_R2_BUCKET", "").strip():
        marker = (
            Path.home()
            / "Library"
            / "Application Support"
            / "veyra"
            / "backups"
            / ".last-remote-upload"
        )
        if marker.exists():
            age_hours = (time.time() - marker.stat().st_mtime) / 3600.0
            if age_hours > 26.0:
                findings.append(
                    f"Off-machine backup is stale: last upload {age_hours:.0f}h ago"
                )
                severity = "warn"

    closes: list[dict] = []
    try:
        events = http_json(f"/events?after={state.get('cursor', 0)}&wait_ms=0")
        for event in events.get("events", []):
            if event.get("kind") == "position_closed":
                payload = event.get("payload")
                if isinstance(payload, dict):
                    closes.append(payload)
                continue
            described = describe_event(event)
            if described == "proposal_unavailable":
                streak = int(state.get("unavailable_streak", 0)) + 1
                state["unavailable_streak"] = streak
                if streak % UNAVAILABLE_ALERT_EVERY == 0:
                    findings.append(
                        f"Autopilot: {streak} consecutive ticks could not decide "
                        "(model or judgement unavailable)"
                    )
                    severity = "warn"
            elif described is not None:
                findings.append(described)
        state["cursor"] = events.get("next", state.get("cursor", 0))
        if any(
            describe_event(event) not in (None, "proposal_unavailable")
            for event in events.get("events", [])
        ):
            state["unavailable_streak"] = 0
    except (OSError, ValueError) as error:
        errors.append(f"event feed failed: {error}")

    if closes:
        fills, report = realized_performance()
        findings.extend(close_message(close, fills.get(close.get("ticket"))) for close in closes)
        summary = record_line(report)
        if summary:
            findings.append(summary)

    if findings:
        push(findings, webhook, severity)
    if errors:
        print("; ".join(errors), file=sys.stderr)
    save_state(state)
    return 0


def push(findings: list, webhook: str, severity: str) -> None:
    text = "\n".join(findings)
    print(f"[{severity}] {text}")
    if not webhook:
        return
    try:
        post_webhook(webhook, text, severity)
    except (OSError, ValueError) as error:
        print(f"alert delivery failed: {error}", file=sys.stderr)


if __name__ == "__main__":
    sys.exit(main())
