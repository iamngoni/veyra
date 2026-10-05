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

Jev runs on OpenRouter in two different ways. They are not interchangeable:

1. **As a chat model** (`typesafe/jev-router`): an ordinary OpenRouter model id
   for the decision tiers. OpenRouter routes each request to an underlying LLM,
   so its price varies.
2. **As the judge** (`typesafe/jev-1.13`, alias `~typesafe/jev-latest`): a
   *decision model*. OpenRouter rejects it on `/chat/completions` ("is a
   decisions model") and serves it from `POST https://openrouter.ai/api/v1/systemone`
   with the same request and response schema as TypeSafe's own System One API.
   This is what Veyra's judge client speaks, so **no TypeSafe account is
   needed**: the judge uses your OpenRouter key.

### Judge through OpenRouter

```dotenv
VEYRA_JEV_API_KEY=sk-or-v1-...                 # the OpenRouter key
VEYRA_JEV_BASE_URL=https://openrouter.ai/api   # Veyra appends /v1/systemone
VEYRA_JEV_MODEL=jev-1.13                       # pinned; jev-latest also works
```

Verified on 2026-10-05 with the autopilot's three questions (direction, trend,
momentum): HTTP 200, answers in the contract's shape, about $0.00002 per
judgement. `jev-1.13`, `jev-latest` and `~typesafe/jev-latest` all resolve to
`typesafe/jev-1.13-20260917`. The config validator accepts bare names only, so
set `jev-1.13`, not `typesafe/jev-1.13`.

With a judge configured, a judge failure pauses the autopilot tick
(`VEYRA_RISK_ALLOW_TRADING_WITHOUT_JEV` empty = fail closed). Set it to `true`
only if you would rather trade on the model alone during a judge outage.

### Without a judge

Leave the three `VEYRA_JEV_*` variables empty. The autopilot then decides from
the candles alone; its prompt does not mention judgements, so the model is not
tempted to ask for a tool that cannot answer.

## Cost control

Leave `VEYRA_AUTOPILOT_INTERVAL_SECS` long (600 is a sensible default) and set
`VEYRA_MODEL_MAX_CALLS_PER_HOUR` / `VEYRA_MODEL_MAX_CALLS_PER_DAY`. One
autopilot cycle can make several model calls, because the model may call
read-only tools before answering, so size the caps from the daily call count
shown in `/status` (`model_budget`) after a day of running.
