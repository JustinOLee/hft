# Kalshi API notes

What this project relies on, from docs.kalshi.com (read 2026-11-20). When the
code and these notes disagree with the live API, the live API wins: update
both.

## Endpoints

| | REST | Websocket |
|---|---|---|
| demo (fake money) | `https://external-api.demo.kalshi.co/trade-api/v2` | `wss://external-api-ws.demo.kalshi.co/trade-api/ws/v2` |
| prod | `https://external-api.kalshi.com/trade-api/v2` | `wss://external-api-ws.kalshi.com/trade-api/ws/v2` |

Demo and prod have separate accounts and separate API keys.
Low-latency hosts for the `fill` and `user_orders` channels only:
`wss://user-ws.kalshi.com/trade-api/ws/v2` (and `user-ws.demo.kalshi.co`).

## Authentication

Three headers on every authenticated request, and on the websocket upgrade:

- `KALSHI-ACCESS-KEY`: key ID
- `KALSHI-ACCESS-TIMESTAMP`: Unix time in **milliseconds**, must be within 30 s of Kalshi's clock
- `KALSHI-ACCESS-SIGNATURE`: base64 signature of `<timestamp><METHOD><path>`

The signed path includes the `/trade-api/v2` prefix and excludes the query
string. For the websocket it is `GET` + `/trade-api/ws/v2`.

Key types: **Ed25519** (default on the website; supported here; sign the raw
bytes) or RSA-2048 (RSA-PSS, SHA-256, MGF1-SHA256, salt length 32; not
supported here yet).

The websocket requires auth **even for public market data**. A 401 body
explains itself: `{"code":"authentication_error","details":"..."}`, where
`NOT_FOUND` means an unknown key ID and `INCORRECT_API_KEY_SIGNATURE` means a bad
signature.

Public REST market data (`GET /markets`, `/markets/{ticker}`,
`/markets/{ticker}/orderbook`) works without auth.

## Numbers

All prices are dollar strings with up to 4 decimals (`"0.5600"`). All counts
are strings with 2 decimals (`"300.00"`). Never parse them as floats; see
`src/fixed.rs`. Valid prices per market come from `price_ranges`
(`[{"start","end","step"}]`). The old integer-cent fields are gone.

## Websocket protocol

Commands: `{"id": <n>, "cmd": "subscribe" | "unsubscribe" | "update_subscription" | "list_subscriptions", "params": {...}}`.
`id` is ours, unique per connection. There is **no** order placement over the
websocket: orders are REST only.

- Responses: `subscribed` (`msg.channel`, `msg.sid`), `ok`, `unsubscribed`, `error` (`msg.code`, `msg.msg`).
- One subscription per channel per connection. Subscribing again adds markets.
- Keepalive: Kalshi sends a Ping (body `heartbeat`) after 10 s without data, and we must answer with a Pong (tungstenite does this).
- Close 1008 "delivery exceeded 15 seconds" means we read too slowly.
- Subscriptions don't survive reconnects. Ignore unknown message types and fields.

### `orderbook_delta` channel (needs auth)

Subscribe: `{"channels":["orderbook_delta"],"market_tickers":[...]}`.
Optional `update_interval_ms` 100 or 1000 conflates updates.

1. `orderbook_snapshot`: `msg.yes_dollars_fp` / `msg.no_dollars_fp` = `[[price, count], ...]`. An empty side is omitted.
2. `orderbook_delta`: `msg.price_dollars`, `msg.delta_fp` (signed), `msg.side` (`yes`/`no`), and `msg.client_order_id` if our order caused it.

- `seq` increments by 1 per message **per subscription (`sid`)**, across all markets on it.
- On a gap, the books are invalid. Resync with `update_subscription` + `action: "get_snapshot"`, or resubscribe.
- The book lists **bids only**: YES bids and NO bids. Best YES ask = $1 − best NO bid.
- `clock` is the exchange's clock. It can skip and even go backwards during maintenance.

### Other channels

- `trade` (public, has `seq`): `yes_price_dollars`, `count_fp`, `taker_side`, `ts_ms`.
- `ticker` (public): about 1/s snapshot of `yes_bid_dollars`, `yes_ask_dollars`, volume.
- `fill`, `user_orders` (private): our fills and order status updates.

## Orders (REST)

- Create: `POST /trade-api/v2/portfolio/events/orders`, with `ticker`, `side` (`bid` = buy YES, `ask` = sell YES), `count` (`"10.00"`), `price` (`"0.5600"`), `time_in_force` (`good_till_canceled` | `immediate_or_cancel` | `fill_or_kill`), `self_trade_prevention_type` (`taker_at_cross` | `maker`), and optionally `client_order_id`, `post_only`.
  - A 201 means accepted, not necessarily resting. Check `remaining_count`.
- Cancel: `DELETE /trade-api/v2/portfolio/events/orders/{order_id}`.
- The legacy `POST /portfolio/orders` returns 410.

## Rate limits (Basic tier)

- Writes: 100 tokens/s with no burst. An order costs 10 tokens and a cancel costs 2.
- Reads: 200 tokens/s, with bursts up to 600.
- 429 means back off exponentially (there is no Retry-After header).
- Up to 200 websocket connections.

## Fees (unverified, check Kalshi's own schedule)

- Taker: `0.07 × contracts × price × (1 − price)`, rounded up.
- Maker: reportedly $0 on most markets.
