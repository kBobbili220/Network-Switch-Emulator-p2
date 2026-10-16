# Assignment 2 scoring: the three floors

`run-world --score` on a world whose `[workload]` is `kind = "mixed"`
prints the A2 report card, computed by `src/floors.rs` from the event log
and the world file. This page says exactly what each line measures, so
that a number on the card can be argued with.

## Reading the card

The card has these parts, in this order.

| part | what it shows |
|---|---|
| header | the world, the part, the result, the load, the run time, and what is not graded |
| `FLOORS` | one row for each floor: the result, the worst value, the value it needs, and where the worst value was |
| `CUSTOMERS` | one row for each customer, for the whole run |
| `FAIR SHARE BY SECOND` | each customer's goodput as a percentage of its fair share, window by window; `.` = not graded, `!` = under the F1 floor. The `total` row is F2 |
| `BOTTLENECKS` | a summary of the links that the world names in `floors.bottlenecks` |
| `LINK LOAD BY SECOND` | all traffic on each busy link direction, as a percentage of capacity |
| `WHERE CUSTOMER PACKETS STOPPED` | drops of customer data, counted by reason |
| `BELIEF vs TRUTH` | every link declaration that the program made (only when it made one) |
| `WHAT WENT WRONG` | for each failed floor: which customers, in which windows, and by how much |

A window is named by the second at which it starts: "second 3" is the
window from 3 s to 4 s. The tables wrap in blocks of 20 windows. The
`--json` file has every number on the card for each window.

## Windows

Time is cut into windows of `floors.window_ms` ([W]) starting at
`params.warmup_ms`. Only whole windows count; the tail of a run that
does not fill a window is not graded. A customer's *goodput* in a window
is the bytes of its **first-time** deliveries in that window — copies of
a `(flow, seq)` that already arrived are duplicates and count for
nothing — divided by the window.

## Customers and demand

Every `[[app]]` row with a `preset` is a customer. Its *demand* in a
window comes from the row, multiplied by `--load-scale`:

| preset | demand in a window |
|---|---|
| `netflix`, `jane_street`, `fortnite` | `rate_mbps` |
| `amazon` | `rate_mbps` in a window its burst is on throughout; not judged otherwise |
| `steam` | unlimited |

Every customer follows one of two sender laws, named on its row.
`law = "aimd"`: it halves its rate on a loss and adds a tenth of its
rate back every 100 ms without one. `law = "rate_match"`: it sends at
1.25 times the rate that was acknowledged over the last 2 × min RTT
(at least 100 ms). It also keeps at most 2 BDP in flight (BDP = the
acknowledged rate × the round trip of its path), and never less than 4
packets. Every 2 s it sends almost nothing for 50 ms and measures the
round trip again, so that it sees a path change. Every world has both
laws. An `amazon` burst is on for 8 to 10 s and off for 8 to 10 s; no
other customer changes its demand during a run. `src/flow.rs` has both
laws.

A switch program is told the same things at `init`: the customer
contract (`input.customers`) has every customer's prefix, its law and
its `rate_mbps × --load-scale` (no limit for `steam`).

Most customers send full-size packets (the world's `packet_size_bytes`,
1000 in the generated worlds). A row with its own `size_bytes` sends
packets of that size: `jane_street` and `fortnite` send 200-byte packets.
Demand, goodput and shares are in bits, so packet size does not enter
the floors; it decides how many packets a second a customer sends.

## F1 — per-customer throughput

In each graded second, every customer's fair share is its max-min share
of what the customers were sending that second: progressive filling,
each customer capped at its demand. Every customer sending throughout
that second must receive at least `share_pct` ([X]) of its share.

Two kinds of window are left out. A window that begins within
`settling_ms` (2 s) of a burst starting is **unsettled**: the apps are still
adapting, and it is nobody's steady state. A window a bursty customer was
sending for only part of is not judged for that customer: max-min
fairness is defined on the demands present while everyone is sending,
and a demand averaged over silence gives a "share" the customer could
not have reached even on an empty network.

Which paths the filling runs over is the world's choice:

- **Part 1** (`detour_hops` absent): the directed links the customer's
  packets used in that window (its last known path if it delivered
  nothing). Only the queues along the path taken are on trial.
- **Part 2** (`detour_hops = k`): every route from the customer's switch
  to its peer's that is at most `k` hops longer than the shortest. Each
  customer is given one of its routes — what a router can do; nobody is
  credited with a share spread over two paths at once — by a fixed
  greedy order: customers with the fewest routes first, then the
  unlimited ones, then the biggest demands; each takes the route whose
  thinnest link, shared with everyone already placed on it, leaves it
  the most, shortest route on ties. The filling then runs over the
  routes given. The share is what the customers could have had with the
  routing done well, so a program that funnels everyone down one link
  while a detour sits empty is judged against the detour. When F1 fails,
  "what went wrong" prints the share on the path taken beside it, so a
  window lost to routing rather than queueing can be told apart.

A world whose bursts repeat faster than one window plus the settling time
has no settled windows, and the card says so (`n/a`). A bursty customer
whose ON time is shorter than a window is never judged by F1; the
generator keeps bursts much longer than that.

## F2 — total throughput

In each window F1 judges, add up the goodput of the judged customers and
add up their fair shares. The first sum must be at least
`utilization_pct` ([U]) of the second. A program that drops so early
that its senders never climb, or that leaves a detour empty, delivers
less than the network could.

F1 protects each customer. F2 protects the network as a whole: every
customer can be above its own floor while much of the capacity is idle.

## F3 — no false link-down

No link fails in Assignment 2, so every `link_down(port)` a program
declares is a lie about the network. One declaration fails the floor.
The card lists every declaration against the truth.

## Delay and loss

Not graded. Both are on the card and in the JSON.

`p50` and `p99` are a customer's median and 99th-percentile one-way delay (first switch ingress to
delivery) in its worst window.

`loss` is printed for a customer whose fair share covers everything
it asks for, in the windows F1 judges, when it sent at least 20 packets:
drops of its data packets at any switch, over packets sent,
retransmissions included. A high number means another customer's
packets were in its way. A customer held below its demand must lose
packets (an AIMD sender finds its rate by loss; a rate-matching sender
sends a quarter more than it is served), so no number is printed for it.

## The rules that never expire

A packet that revisits a switch is a forwarding loop: **fail**. A
customer that delivers nothing all run is starved: **fail**. The A1
delivery gate is gone; loss is legal here.

## Collapse: a note, not a floor

For each direction of each `floors.bottlenecks` link, a window is
**overloaded** when the summed demand of the customers whose packets
used that direction is at least the link's capacity (an unlimited
customer counts as the whole capacity). For each direction the card prints
the number of overloaded windows and the first-time, duplicate and total
load as a percentage of capacity. The `-- collapse` row under `FLOORS`
names the overloaded window with the least first-time data. The JSON has
the three loads for each window.

It is a note and not a floor because a link full of copies is already
failing the fairness floor: copies are not goodput. The number is on the
card because it is the clearest picture of congestion collapse you can
read off a run.

## World fields

```toml
[floors]
share_pct = 40            # [X]
utilization_pct = 55      # [U]
window_ms = 1000          # [W]
settling_ms = 2000
goodput_pct = 50          # [G], the collapse note's line
bottlenecks = [[2, 7], [7, 8]]
detour_hops = 2           # Part 2 only: shares over the network's paths
```

Every bottleneck must be a declared link; both directions are shown.
The numbers above are the defaults in every generated world. The same
worlds set, under `[params]`:

- **[B]** `buffer_budget_packets = 40` and `packet_size_bytes = 1000`:
  each switch's buffer pool is 40 full-size packets, 40,000 bytes. The
  pool is charged in bytes, so a 200-byte packet or a 40-byte ack takes
  only its own size. Queue caps and drop lines a program sets, and the
  buffer series in the card's JSON, are in full-size packets.
- **[Δ]** `control_delay_us = 2000`: every action a program returns
  lands 2 ms later.
- **[T]** AIMD's epoch is 100 ms and its increment a tenth of the
  customer's rate.
- `warmup_ms`: grading starts 1 s in for Part 1 and 3 s in for Part 2,
  where a program has to notice where the load is and move customers
  before the first window opens.

We grade every world at `--load-scale 1` and `--load-scale 1.5`.
