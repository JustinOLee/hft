# Primer: what we've built so far

This is a step-by-step recap of the project up to the start of step 4. It
explains each piece in plain words. You don't need to read any code to follow
it, but each section says which file to look at if you want to.

---

## The big idea in one picture

```
   Kalshi's servers
         │
         │  messages come in over the internet
         ▼
 ┌───────────────────┐
 │  network thread   │   talks to Kalshi, and writes down EVERYTHING
 └───────────────────┘
         │
         ▼
   logs/<time>.log      a file: one line per thing that happened
         │
         ▼
 ┌───────────────────┐
 │   state machine   │   reads the file, line by line, and keeps track
 └───────────────────┘   of what the market looks like right now
         │
         ▼
   prints prices (later: decides to buy or sell)
```

Three rules hold everything together:

1. **Write everything down.** Every message in and out goes into the log file
   the moment it happens.
2. **The state machine only reads the log.** It never touches the network or
   the clock.
3. **So we can replay.** Feed an old log file back into the state machine and
   you get exactly the same result, every time. That's how you debug a
   trading system: re-run the exact moment something went wrong.

---

## Step 0: Setup

**What we did:**
- Installed Rust, the programming language we're using. `cargo` is its tool
  for building and running code.
- Made a git repository (a history of every change) and put it on GitHub at
  `github.com/JustinOLee/hft`. **The repo is public**, so anyone can see it.
- Told git to **ignore** secret files (`secrets/`, `*.pem`, `.env`) and log
  files (`*.log`), so they can never be uploaded by accident.

**Commands you'll use:**
```
cargo build                 # compile the program
cargo test                  # run the automatic checks
cargo run -- live <TICKER>  # run it for real
git status                  # see what changed
```

---

## Lesson 1: Talking to a server by hand

**File:** git history, commit "Lesson 1"

**What we did:** you wrote a program that opened a connection to
`example.com` and sent this, as plain text:

```
GET / HTTP/1.1
Host: example.com
Connection: close
(blank line)
```

The server answered with a web page.

**What you learned:**
- **TCP** is a pipe for bytes between two computers. You write bytes in one
  end and they come out the other end, in order.
- **HTTP** is just text sent through that pipe. The blank line at the end
  means "my request is done." Without it, both sides wait for each other
  forever, which is what you saw when your program hung.
- **A read returns whatever has arrived so far**, not "one message." You got
  932 bytes in one read, but it could just as easily have come in 10 pieces.
  So you always collect bytes into a buffer and look for where a message ends.
  This idea comes back over and over. It's called **framing**.
- There are three ways HTTP marks where a response body ends:
  `Content-Length: N`, chunked encoding (size, data, size, data, 0), or the
  server hanging up.

---

## Step 1: Connect and log everything

**Files:** `src/ws.rs`, `src/eventlog.rs`

**What it does:** opens a live connection to the exchange and writes every
event to a log file.

**The layers of a connection**, from the bottom up:

| Layer | What it does | Who handles it |
|---|---|---|
| TCP | the byte pipe | Rust's standard library |
| TLS | encrypts the pipe and checks we're talking to the real server | `rustls` library |
| HTTP upgrade | one request that says "let's switch to websocket" | we wrote this by hand |
| WebSocket | splits the pipe into separate messages | `tungstenite` library |

Each layer costs time. It took about 0.6 seconds before the first message
arrived. That's why trading systems open connections once and keep them open.

**The log file** (`logs/<time>.log`) has one line per event, written as JSON:

```
{"ts":1795288777989465000,"conn":1,"kind":"ws_in","data":"..."}
```

- `ts`: when it happened, in nanoseconds (billionths of a second) since 1970
- `conn`: which connection
- `kind`: what happened (`tcp_open`, `http_request`, `ws_in`, `ws_out`, ...)
- `data`: the exact message

**Two rules for writing the log:**
- Incoming messages: write them down the moment they arrive.
- Outgoing messages: write them down *before* sending.

That way, if the program crashes, the log never claims we sent something we
didn't, and never misses something that arrived.

**Libraries rule:** we only use libraries that do pure computation (encryption,
message framing, JSON). We open sockets and decide when to read and write
ourselves. These are called "sans-io" (French for "without input/output")
libraries.

---

## Step 2: A state machine that tracks the market

**Files:** `src/sm.rs`, `src/tail.rs`, `src/event.rs`, `src/main.rs`

**What it does:** two threads (two things running at once):
- the **network thread** talks to the exchange and writes the log
- the **main thread** reads the log as it grows and feeds each line to the
  state machine

They share nothing except the file.

**State machine** sounds fancy but it's simple: a thing that remembers some
facts (connection status, the order book) and updates them one event at a
time, through one function: `update(event)`.

**The golden rule:** `update` may only use the event it's given and what it
already remembers. No reading the clock, no network, no random numbers. When
it needs "the current time," it uses the event's `ts`.

**Reading a growing file** (`tail.rs`): when we reach the end of the file, the
other thread may be halfway through writing a line. So we keep the half-line
and wait for the rest. It's the same framing problem as Lesson 1, using the
end of a line as the boundary.

**Small details that matter:**
- We use `BTreeMap` instead of `HashMap`. A HashMap lists its contents in a
  different order every time the program runs, which would quietly break
  replay.
- If the connection drops, we **throw away the order book**. The real market
  keeps moving and ours wouldn't. A stale book is worse than no book.

---

## Step 3: Replay

**Files:** `src/main.rs`, `scripts/check_replay.sh`

**What it does:** `hft replay <file>` runs the state machine over an old log
file, with no network.

**The test:** run live for 30 seconds, then replay the file it wrote. The
printed output must be **exactly identical**, byte for byte.

```
scripts/check_replay.sh 30 <TICKER>
```

It passed on the first try because steps 1 and 2 followed the rules. To prove
the test catches the mistake it's meant to catch, we made the state machine
read the clock on purpose. Every line came out different. Then we undid it.

**Why this matters:** later, when the program does something strange with
real money, you can replay that exact session, add print statements, and
step through it until you find the bug. Without replay, the bug may never
happen again in front of you.

---

## The switch to Kalshi

We started on Hyperliquid (a crypto exchange). Its rules don't allow people
in the US, and you're in Texas. So we switched to **Kalshi**, a US-regulated
exchange for yes/no questions ("Will player X win this match?"). Everything
above stayed the same. Only the parts that talk to the exchange changed.

### How Kalshi markets work

- Each market is a yes/no question.
- A **YES contract** pays **$1** if the answer is yes, and $0 if not.
- A **NO contract** pays $1 if the answer is no.
- So a price is also a probability: YES at $0.20 means the market thinks
  there's about a 20% chance.

### The order book

The **order book** is the list of everyone's offers to buy and sell.

- **Bid** = an offer to buy. **Ask** = an offer to sell.
- **Best bid** = the highest offer to buy. **Best ask** = the lowest offer to
  sell. The gap between them is the **spread**.
- **Mid** = halfway between the best bid and best ask.

Kalshi only lists bids, on both sides: "buy YES at 20¢" and "buy NO at 79¢".
Buying NO at 79¢ is the same as selling YES at 21¢, because YES + NO always
equals $1. So:

```
best YES ask = $1.00 − best NO bid
```

**File:** `src/book.rs`

### Exact prices (no decimals in the computer)

Computers store normal decimal numbers ("floats") slightly inaccurately:
0.1 + 0.2 comes out as 0.30000000000000004. That's not acceptable for
prices. So we store prices as **whole numbers of tiny units**:

```
$0.5600   →  5600   (units of $0.0001)
300.00 contracts  →  30000   (units of 0.01 contracts)
```

Whole-number math is exact, and it gives the same answer on every computer.

**File:** `src/fixed.rs`

### Snapshot, then changes

Kalshi sends the full order book once (a **snapshot**), then only the
**changes** (**deltas**): "the quantity at 56¢ went down by 3."

Each message has a **sequence number**: 1, 2, 3, 4... If we ever see 5 right
after 3, message 4 got lost, and our copy of the book is now quietly wrong.
When that happens we throw the book away until a fresh snapshot arrives.

### Proving who we are (signing)

Kalshi needs to know it's really you, even just to watch prices. You made a
**key pair** on Kalshi's website:
- Kalshi keeps the **public key**.
- You keep the **private key** (`secrets/kalshi-prod.pem`). **Never share it.**

When we connect, we take the current time plus the request, for example
`1795288777989GET/trade-api/ws/v2`, and **sign** it with the private key. Kalshi
checks the signature with the public key. The time is included so that an old
signature stops working after 30 seconds.

The log file shows `<redacted>` in place of the key ID and signature, so the
log can never leak them.

**Files:** `src/kalshi.rs`, `secrets/kalshi.env` (your settings),
`kalshi.env.example` (the blank template that goes on GitHub)

### Checking our own work

On the first real run, we got 653 book changes but only 3 printed lines. That
looked like a bug. So we rebuilt the books with a completely separate Python
script. It agreed: the best prices just hadn't moved. All the activity was
deeper in the book. **Checking a result with a second, independent method** is
a habit worth keeping.

---

## Step 4: The state machine can act

**Files:** `src/action.rs`, `src/http.rs`, `src/net.rs`, `src/main.rs`, `src/sm.rs`

**What it does:** `update(event)` now returns a list of **actions**:

| Action | Meaning | Sent over |
|---|---|---|
| `place` | put an order in the book | REST (an HTTP request) |
| `cancel` | take an order out | REST |
| `resync` | "send me fresh order books" | websocket |

Usually the list is empty.

**How an action travels:**
```
state machine ──(channel)──► network thread ──► writes "action" to the log
                                             └─► sends it to Kalshi
```
A **channel** is a queue between two threads: one end puts things in, the
other takes them out.

**The state machine learns what happened the same way it learns everything:
from the log.** It sees its own `action` line come back, so it knows the
order is **pending** (asked for, not yet confirmed). Then Kalshi's answer
arrives, and the order becomes accepted, resting, canceled and so on.

**A hand-written HTTP client** (`http.rs`). Kalshi takes orders as normal
HTTP requests. We keep the connection open between requests ("keep-alive")
to save setup time, which means we must find where each response ends using
`Content-Length` or chunked encoding: Lesson 1 again. The tests feed it
responses 3 bytes at a time to prove it doesn't assume a whole message
arrives in one read.

**A stopgap in the network loop.** The network thread used to wait forever
for the next websocket message. Now it waits at most 5 milliseconds, then
checks for actions, then goes back to waiting. Step 7 replaces this with a
proper design.

**Settings are inputs too.** The first line of the log (`start`) now records
the session's settings. Replay reads them from there, so it always uses
exactly what the live run used.

**How we tested it without spending money:**
- `hft balance` sends one signed request ("what's my balance?") and prints
  the answer.
- `hft live --test-resync <TICKER>` makes the state machine ask for fresh
  order books once, right after the first one arrives. That sends a real
  action through the whole path, and replay still matches.

---

## Step 5: The first real order

**Files:** `src/sm.rs` (the `run_place_cancel` function), `src/main.rs`

**What it does:** `hft live --place-cancel <TICKER>` runs a tiny strategy:

1. Wait until we can see the market's order book.
2. Place a **post-only bid for 1 YES contract at 1¢**. "Post-only" means:
   if this order would trade immediately, reject it instead. 1¢ is far
   below the market, so the order just sits in the book. Even in the worst
   case it would cost one cent.
3. When Kalshi says the order is **resting** (sitting in the book), cancel it.
4. When Kalshi says it's **canceled**, stop.

**A trap we avoided: asking twice.** After the state machine decides to
cancel, it takes a moment for its own `cancel` action to come back through
the log. Other events (book changes) can arrive in between. If the strategy
only looked at "what does Kalshi say the order's status is?", it would see
"still resting" and send another cancel, and another. So the strategy keeps
its own note of **what it has asked for** (`Placed`, `Canceling`), which
runs ahead of **what Kalshi has confirmed**. Remembering both is a big theme
of trading systems.

**Another trap: news arriving out of order.** Kalshi tells us about our
order on two different connections: the REST reply ("accepted") and the
websocket feed ("resting"). They can arrive in either order. So an order's
status only ever moves **forward**: once we know it's resting, a late
"accepted" doesn't drag it back.

**Testing it before using real money:** `hft replay --place-cancel <old log>`
runs the strategy on a log recorded *without* it. It printed exactly one
`place` action and then waited (that old log never confirms the order). Good.

**The real run** (Kalshi production, real account):

| Time | What happened |
|---|---|
| 0.00 s | start |
| 0.19 s | websocket open, subscribed to the book and to our own orders |
| 0.24 s | state machine: **place** (buy 1 at $0.01) |
| 0.24 to 0.37 s | open the REST connection (TCP, then TLS) |
| 0.43 s | Kalshi: **201 Created**, here's the order ID |
| 0.44 s | private feed: order is **resting** |
| 0.45 s | state machine: **cancel** |
| 0.49 s | Kalshi: **200 OK**, reduced by 1.00 |
| 0.50 s | private feed: order is **canceled**. Done. |

A whole round trip, place to cancel, took a quarter of a second. Balance
afterwards: unchanged at $20.01.

**Checking it with replay:** `hft replay --check-actions <log>` compares the
actions the state machine produces during replay with the actions the live
run wrote to the log. Both had exactly the same 2 actions (place, cancel),
and the printed output was byte-for-byte identical. `scripts/check_replay.sh`
now does both checks automatically.

**What the program prints** for an order:
```
ACTION {"action":"place", ... "price":100,"count":100,"post_only":true}
ORDER hft1795298319-1 KXAAAGASD-26NOV22-3.6450 Pending -
ORDER hft1795298319-1 ... Accepted 01a2001e-...
ORDER hft1795298319-1 ... Resting  01a2001e-...
ACTION {"action":"cancel", ...}
ORDER hft1795298319-1 ... CancelPending 01a2001e-...
ORDER hft1795298319-1 ... Canceled 01a2001e-...
DONE place_cancel: placed and canceled hft1795298319-1
```
`hft1795298319-1` is **our** order ID (session number + counter). The long
one is **Kalshi's**. We pick our own so we can recognize the order in every
message without waiting for Kalshi to tell us its ID.

**Privacy note:** the log contains your Kalshi user ID (Kalshi includes it in
order messages). Logs are never committed to git, so it stays on your
computer.

---

## Step 6: Stop and look at what we have

This is a basic trading system:

```
packets in  →  state machine  →  packets out
```

- Everything unpredictable (the network, the clock) is at the **edges**: the
  network thread.
- The middle (the state machine) is **predictable**. Same log in, same
  output and same actions out, every time.
- So whenever something goes wrong, we can replay the exact session and find
  out why.

**What's still rough** (the next steps fix these):

| Problem | Why it matters | Fixed in |
|---|---|---|
| The network loop polls every 5 ms, and blocks during REST requests | wastes time, adds delay | step 7 |
| No timer: the state machine only runs when a message arrives | can't act in a quiet market | step 7 |
| If the connection drops, the program just sits there | can't run for long | step 7 |
| We don't know how long messages wait before we read them | can't measure speed | step 8 |
| Threads talk through a file on disk | slow, a hack | step 9 |

---

## Step 7: A proper network loop (the "reactor")

**Files:** `src/reactor.rs` (new, replaces the old network code), `src/http.rs`

**The problem:** the old network thread used three hacks:
- it waited up to 5 ms on the websocket, then checked for actions, over and over
- while an order request was out, it couldn't read the websocket at all
- if a connection dropped, it just stopped

**The fix: an event loop.** One loop, one thread, that never waits on any
single thing:

```
loop:
    ask the operating system "which connections have something for me?"
        (sleep until one does, OR the next timer tick, OR the state
         machine wakes us up because it has an action)
    for each ready connection: read everything, write what we can
    send any actions the state machine queued
    every second: write a "timer" event
    reconnect anything that dropped
```

This design is called the **reactor pattern**. "Ask the operating system" is
a feature called **kqueue** on Mac and **epoll** on Linux. The `mio` library
gives both one interface.

**New ideas in this step:**

- **Non-blocking sockets.** A read with nothing to read returns the error
  `WouldBlock` right away instead of waiting. That "error" is normal. It
  just means "nothing yet, come back later".
- **Edge-triggered.** The operating system tells us once when a connection
  *becomes* ready. So every time we touch a connection we read until
  `WouldBlock`. Otherwise leftover data could sit there with no second
  reminder.
- **Each connection is its own little state machine:**
  ```
  TcpConnecting → TlsHandshaking → (websocket only: Upgrading) → Open
  ```
  Each time the operating system says a connection is ready, we move it
  forward as far as it can go without waiting.
- **The waker.** When the state machine sends an action, it also "wakes" the
  reactor, so the action goes out immediately instead of at the next timeout.
- **The timer.** Every second the reactor writes a `timer` event to the log.
  The state machine can use it to act when the market is quiet. Because it
  goes in the log, replay sees the same ticks.
- **Reconnecting with backoff.** If a connection drops, wait 1 second and
  reconnect. If that fails, wait 2, then 4, 8 and so on up to 30 seconds.
  Hammering a struggling server makes things worse, and Kalshi turns away
  clients that reconnect too fast. After reconnecting we subscribe again,
  because subscriptions don't survive a reconnect.
- **HTTP framing as a pure function.** `parse_response(bytes)` answers either
  "here's a complete response, and it used N bytes" or "not complete yet".
  It never reads from the network itself, so the reactor and the simple
  `hft balance` tool share the same code and tests.

**Something the logs taught us.** The first run showed this line:
```
10.248s  conn2  tcp_close  connection closed by server
```
**Kalshi closes a REST connection after 10 seconds without a request.** That
hides a real danger: if we send an order at the exact moment Kalshi hangs up,
we get an error and **don't know whether the order went through**. Two fixes:
1. We replace the REST connection ourselves after 8 seconds idle, before
   Kalshi does it. A warm connection is always ready.
2. A new order status, **`Unknown`**: "the request failed with no reply, so
   we don't know." The private order feed tells us the truth later. The old
   code would have said "Rejected", which could have been false while the
   order was actually live.

**A Rust lesson: the borrow checker.** The first version didn't compile:
`cannot borrow conn.inbuf as mutable more than once at a time`. The code tried
to use the connection's encrypted stream and its buffer, both mutably, at the
same time. Rust refuses two mutable references to the same thing at once,
because that's how data races and memory corruption happen in other
languages. The fix was to move the buffer out of the connection, use it, and
put it back (`read_into_inbuf`).

**Tests on the real exchange:**

1. **Watching for 20 s:** both connections opened *side by side* (no more
   waiting in line), 20 timer events, and Kalshi's ping was answered
   automatically. Replay was identical.
2. **A forced disconnect** (`--test-disconnect` drops the websocket 10 s after
   it opens):
   ```
   10.14 s  websocket: tcp_close "test: forced disconnect"
            state machine prints "- - -"  (no book: better than a stale one)
   11.14 s  reconnect (after the 1 s backoff)
   11.35 s  subscribed again, fresh snapshot, prices back
   ```
   The REST connection was recycled every 8 s throughout. Replay was
   identical.
3. **The real order again:**

   | | Step 5 | Step 7 |
   |---|---|---|
   | decide to place → request sent | 130 ms (had to open a connection) | **1.3 ms** (warm connection) |
   | place → canceled, total | 250 ms | **103 ms** |

   What's left is almost all network travel: about 45 ms each way to
   Kalshi. Balance unchanged at $20.01. Replay actions matched.

---

## Where we are

| Plan step | Status |
|---|---|
| 1. Connect and log everything | ✅ |
| 2. State machine tracks the book | ✅ |
| 3. Replay gives identical output | ✅ |
| 4. The state machine can act | ✅ |
| 5. Place one real order and cancel it | ✅ |
| 6. Look at what we have | ✅ |
| 7. A proper network loop | ✅ |
| 8. Measure waiting time (kernel timestamps) | ⏳ next |

## Words to know

| Word | Meaning |
|---|---|
| **TCP** | a reliable pipe for bytes between two computers |
| **TLS** | encryption for that pipe (the "s" in https) |
| **WebSocket** | a long-lived connection where both sides send messages any time |
| **Framing** | figuring out where one message ends and the next begins |
| **Event log** | the file where we write down everything that happens |
| **State machine** | code that remembers facts and updates them one event at a time |
| **Deterministic** | same input → same output, every single time |
| **Replay** | running the state machine over an old log file |
| **Order book** | everyone's current offers to buy and sell |
| **Bid / Ask** | offer to buy / offer to sell |
| **Spread** | the gap between the best bid and the best ask |
| **Mid** | halfway between the best bid and the best ask |
| **Snapshot / Delta** | the whole book / one change to it |
| **Sequence number** | a counter on each message, used to detect lost messages |
| **Private key** | your secret, used to sign requests. Never share it. |
| **Thread** | one line of work in a program. Two threads run at the same time. |
