# The plan (Kalshi version)

This is `idea.txt` rewritten for Kalshi, in plain English. It describes how
to build a small, fast trading program from scratch, one step at a time.

This is a **learning project**, not a way to make money. Expect it to take a
couple of months. Each step builds on the last, so do them in order.

New to the words? See the "Words to know" table at the end of
[docs/PRIMER.md](docs/PRIMER.md).

---

## Ground rules (read these first)

1. **Use small money.** Keep only a few dollars in the account. A mistake
   should cost cents, not hundreds of dollars.
2. **Keep secrets secret.** The private key lives in `secrets/`, which git
   ignores. This repo is public. Never paste the key anywhere.
3. **Write everything down.** Every message in and out goes into the log file.
4. **The state machine never touches the outside world.** No clock, no
   network, no random numbers. It only reads events.
5. **Always be able to replay.** If replaying a log gives a different result
   than the live run, stop and fix it before doing anything else.
6. **Respect the exchange.** Kalshi limits how many requests you can send
   (about 10 orders per second on the basic tier). Stay well under that.

---

## Part 1: Watching the market

### Step 1. Connect and log everything ✅

**Goal:** see exactly what Kalshi sends us.

**Build:** a program that connects to Kalshi's websocket and writes every
event to a file, with the time it happened. Events include: connection
opened, connection closed, request sent, message received, message sent.

**Rules:**
- Write incoming messages the moment they arrive.
- Write outgoing messages *before* you send them.
- Run everything on one thread. Don't use `async` (Rust's built-in way of
  juggling many tasks); we'll build our own version later.
- Only use libraries for pure computation (encryption, message framing,
  JSON). Open the sockets yourself.

**Done when:** you can open the log and read every message from a live session.

### Step 2. Track the order book ✅

**Goal:** turn the stream of messages into "what does the market look like
right now?"

**Build:**
- A **state machine**: an object with one function, `update(event)`, that
  keeps track of connections and the order book.
- Run the network code on one thread. On a second thread, read the log file
  as it grows, and call `update` for every new line.
- Make `update` print the time and the mid price.

**Done when:** you run it and see prices printing as the market moves.

### Step 3. Replay ✅

**Goal:** prove the state machine is **deterministic**: same input, same
output, every time.

**Build:** a "replay" mode that reads an old log file and runs the state
machine, with no network at all.

**Done when:** replaying a session prints *exactly* what the live run printed.
If it doesn't, find what's different (usually something reading the real
clock instead of the event's time) and fix it.

---

## Part 2: Trading

### Step 4. Let the state machine act

**Goal:** the state machine can decide to do things, not just watch.

**Build:**
- `update` returns a list of **actions**: place an order, cancel an order, or
  ask for a fresh order book. Usually the list is empty.
- Send actions to the network thread through a queue.
- The network thread writes each action to the log **before** doing it.
- In replay mode, print the actions but don't send them.
- Kalshi takes orders over its REST API (normal HTTP requests), not over the
  websocket, so the network thread needs a second connection for that.

**Done when:** actions flow from the state machine to the network thread and
into the log, and replay shows the same actions.

### Step 5. Place one order and cancel it

**Goal:** the first real round trip with real money.

**Build:** make the state machine, once it knows the order book:
1. place one small buy order, far below the market so it won't fill
   (for example: buy 1 YES contract at 1¢, which costs at most $0.01)
2. wait until Kalshi confirms the order is resting in the book
3. cancel it

**Check:**
- First run it in replay mode on an old log. Does it print the actions you
  expect?
- Then run it live.
- Then replay the live session. The actions from the replay must match the
  actions in the log **exactly**. If not, fix the state machine and try
  again.

**Done when:** live and replay agree, and Kalshi's website shows the order
was placed and canceled.

### Step 6. Stop and look at what you have

Congratulations: this is a basic trading system.

```
messages in  →  state machine  →  messages out
```

Everything unpredictable (network, clock) sits at the edges. The middle is
predictable, so you can always replay it to find bugs.

---

## Part 3: Making it solid

### Step 7. A proper network loop

**Goal:** replace the quick hacks in the network code with a clean design.

**Learn:** how a program can wait on many connections at once. On Linux this
is called **epoll**; on Mac it's **kqueue**. The `mio` library wraps both.
The design built on top is called the **reactor pattern**: one loop that
asks the operating system "which connections have data?", handles them, and
goes back to waiting.

**Build:**
- Rewrite the network thread as one loop that checks, without ever getting
  stuck waiting on just one: every connection, the queue of actions, and a
  timer.
- Add a **timer event every second**, so the state machine can act even when
  the market is quiet (for example, to cancel an order that's been sitting
  too long).
- Log everything the loop does as an event.
- Reconnect automatically when a connection drops.

**Done when:** the program runs for hours, survives disconnects, and replay
still matches.

### Step 8. Measure waiting time

**Goal:** find out how long messages sit around before we read them.

**Build:** ask the operating system to stamp each network packet with the
exact time it arrived (**kernel timestamps**). Store that next to our own
timestamp. The difference is how long the message waited for us.

This delay doesn't matter yet. We're just learning to measure.

### Step 9. A real queue

**Goal:** replace "write to a file, read the file" with a proper queue.

**Build:** wrap the file reading and writing in a simple queue interface
(`push` and `pop`), for both directions: network → state machine and
state machine → network. Leave a note to later build your own fast version,
like the open-source Chronicle Queue.

This delay doesn't matter yet either.

---

## Part 4: A real strategy

### Step 10. Market making

**Goal:** earn the spread by always offering to both buy and sell.

**Build:**
- Pick one active market.
- Keep one buy order (bid) below the mid and one sell order (ask) above the
  mid, a set distance apart (the spread).
- Set a **maximum inventory**: the most contracts you're willing to hold.
- When you're holding contracts, **lean** your prices to get rid of them.
  Let `x` = (contracts you hold) ÷ (max inventory), between 0 and 1:
  - On the side that just traded with you, move your price **away** from the
    mid by `spread × x ÷ (1 − x)`.
  - On the other side, move your price **closer** to the mid by `spread × x`.

Kalshi prices move in steps (usually 1¢), so round to the market's price
grid (`price_ranges`).

**Done when:** it quotes both sides, gets some fills, and keeps its inventory
under control.

### Step 11. Split the state machine into three

By now the state machine will be getting messy. The fix is *more* state
machines, each with one job:

1. **Tracker:** reads events and keeps the facts: order books, your orders,
   your balance and position.
2. **Strategy:** looks at the facts and says what orders it *wants*: "a bid
   at 19¢ for 5 contracts, and an ask at 22¢ for 5." Leave a TODO to add
   nicer helpers for describing wanted orders later.
3. **Executor:** compares the wanted orders with the real ones and works out
   what to do: place, cancel, or nothing. "Real orders" includes **pending**
   ones: sent but not yet confirmed.

**Done when:** replay gives *identical* output before and after the split.
If not, something changed by accident.

### Step 12. A second exchange

**Goal:** trade the same thing in two places.

**Build:**
- Add **Polymarket US** as a second exchange, if its API allows it. If not,
  use two related Kalshi markets instead. For example, in a tennis match
  "Player A wins" and "Player B wins" must add up to $1.
- Run the step 10 market maker on the second exchange.
- Then the real trade: pick a question listed on both, with decent volume.
  - Put a resting order on exchange A at a price where you could
    immediately do the opposite trade on exchange B, plus a small profit.
  - Every time the order on A gets filled, immediately send an order on B
    that trades right away (**IOC**: immediate-or-cancel) to cancel out your
    position. This is called **hedging**.
- Keep the lean from step 10, but only count your position on exchange A.
- Use a tiny profit margin at first so it fills often, and fix bugs.

**Done when:** it fills, hedges correctly, tracks its position on both
exchanges, and replay still matches.

### Step 13. Be meticulous

Go through everything carefully. Questions to answer:

- Are you calculating **fees** correctly? Check against Kalshi's fee schedule.
- Do you update your position the **moment** you learn about a fill?
- Read through log files line by line. Read the exchange docs again.
- Are you subscribing to everything useful? Kalshi has a faster connection
  just for your own orders and fills (`user-ws.kalshi.com`).
- Which arrives first: news of your fill, or the order book change? Does it
  vary?
- What triggers your hedge, and is there a faster signal?
- If you open the same feed on 5 connections, do the messages arrive at the
  same time?
- Sort your fills into good and bad ones. What did the market look like just
  before each kind?
- Build a simple page (an LLM can help) that steps through a log file one
  line at a time and shows the order book and your orders.
- Are you canceling and re-placing orders for no reason? That loses your
  place in line.
- Are you placing orders where nobody else is, far from real activity?
- Is someone changing their order every time you change yours?
- Can it run for **24 hours**, survive disconnects and exchange problems?
  Treat everything that goes wrong as a chance to improve it.

**Aim for:** tens to hundreds of fills per hour.

---

## Part 5: Thinking before trading

### Step 14. A simulator

**Goal:** answer "what if?" questions without risking money.

**Build:** code that takes a real log, plus your state machine, and
pretends: "if my orders had been in the book at that moment, would they have
filled?"

- Use it to answer questions like: "what if I hedged with a resting order
  instead of an IOC order?"
- Then actually try that live, and compare reality with the simulation.
- Adjust the simulator until it roughly matches reality. It doesn't need to
  be perfect. You could spend a month on the simulator alone; make it just
  good enough for the questions you have now.
- Use **seeded** random numbers for things like network delay: the same seed
  gives the same "random" numbers. Same seed + same log + same strategy
  = same result, every time.
- You should be able to answer: "how much better would I do if my
  connection were twice as fast?"
- Make it a habit to compare real trading with the simulator, and dig into
  anything that doesn't match.

### Step 15. Predicting prices

**Goal:** guess where the price is heading in the next few seconds.

**Learn:** read the paper at https://arxiv.org/pdf/2108.09750 for ideas on
**signals**: clues in the data that hint at the next price move. If it's too
dense, read the relevant chapters of *The Elements of Statistical Learning*.

**Build:**
- Record 24 hours of data and save a copy. That's your research data.
- Build features from it (order book imbalance, recent trades, related
  markets) and try to predict how the mid price will change over the next
  few seconds.
- You probably won't be able to make money from the prediction on its own.
  Instead, use it to improve your fills in the step 12 trade.

Prediction markets have their own clues: related markets (both sides of a
match, neighboring gas-price levels), and real-world data like live scores.

---

## How big should this be?

About **5,000 to 15,000 lines** of code in total. The exchange-specific code
can grow large, but the core state machines should stay small.

## After that

By this point you'll see a million ways to improve the system, and you won't
have time for all of them. Especially if you've written the important parts
yourself and dug into the areas you understand least. Pick what interests
you and make your own plan.
