# Decision models and Jev

Veyra's decision engine talks to one model provider. The recommended setup is
**OpenRouter**, which exposes hundreds of models behind one key, including Jev.

## Using any OpenRouter model

```dotenv
VEYRA_MODEL_PROVIDER=openrouter
VEYRA_MODEL_API_KEY=sk-or-v1-...        # your OpenRouter key, never committed
VEYRA_MODEL_FAST=openai/gpt-4.1-mini
VEYRA_MODEL_BALANCED=typesafe/jev-router
VEYRA_MODEL_REASONING=typesafe/jev-router
```

Each tier takes any OpenRouter model id (no whitespace). The service does not
keep an allow-list of its own. Two things can still stop a model:

1. **The model must accept a forced tool call** (`tool_choice`), because Veyra
   makes the model answer through a response schema. For reasoning models that
   reject this, set `VEYRA_MODEL_COMPEL_STRUCTURED=false`.
2. **Your OpenRouter account's provider policy must allow it.** A model that no
   allowed provider serves returns HTTP 404 ("No allowed providers are
   available"); change the allowed providers in OpenRouter or pick another model.

Models can also be changed from the dashboard's settings without a restart; a
value saved there overrides `.env`.

### Checked on 2026-10-05

A forced tool call (the request Veyra makes) was sent to each model through the
production OpenRouter key:

| Model | Result |
| --- | --- |
| `typesafe/jev-router` | works (served by `openai/gpt-6-luna` at the time) |
| `openai/gpt-4o-mini` | works |
| `openai/gpt-4.1-mini` | works |
| `anthropic/claude-haiku-4.5` | works |
| `google/gemini-2.5-flash` | works |
| `deepseek/deepseek-chat` | blocked: no allowed provider on that account |

This proves the model accepts the call, not that its trading judgement is good.
Evaluate a model on dry runs before arming live orders.

## Jev

`typesafe/jev-router` is the only Jev model on OpenRouter. It routes each
request to an underlying model, so OpenRouter lists its price as variable.
**Use it like any other model id; no TypeSafe key is required.**

`VEYRA_JEV_API_KEY` configures something different: an optional *judge* service
that adds calibrated judgements to the autopilot's inputs. When it is not
configured the autopilot decides from the candles alone and nothing is blocked.
`VEYRA_RISK_ALLOW_TRADING_WITHOUT_JEV` only matters when a judge **is**
configured and then fails.

## Cost control

Leave `VEYRA_AUTOPILOT_INTERVAL_SECS` long (600 is a sensible default) and set
`VEYRA_MODEL_MAX_CALLS_PER_HOUR` / `VEYRA_MODEL_MAX_CALLS_PER_DAY`. One
autopilot cycle can make several model calls, because the model may call
read-only tools before answering, so size the caps from the daily call count
shown in `/status` (`model_budget`) after a day of running.
