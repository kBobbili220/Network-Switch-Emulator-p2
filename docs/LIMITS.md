# Limits

Your program runs inside a sandbox with hard ceilings. They are the same
for everyone, and the grader uses these exact numbers.

## Per call into your program

| Limit | Value | What happens if you exceed it |
|-------|-------|-------------------------------|
| Instruction budget ("fuel") | 4,000,000 per call | The call traps. Your program is marked failed and stops being called for the rest of the run. |
| Returned actions, encoded | 4,096 bytes | The call is rejected with an output-too-large error. |

The budget is **per call**, not per run. `init`, each `on_punt` and each
`on_timer` gets its own 4,000,000. Spending it is free; overrunning it is
fatal, so keep per-call work bounded — a loop over every switch you know
about is fine, and on a 15-switch world a loop over every pair of them
still fits with room to spare.

The ceiling is set well above what a working solution needs, so you should
not have to think about it. If you are hitting it, the problem is almost
certainly an accidental unbounded loop, not honest work.

4,096 bytes is roughly 150 table-entry installs in one return. If you need
more, install them across several calls.

## When you break one of these

The simulator tells you. If your program is killed mid-run you get a line
naming the switch, the simulation time, and which handler was running:

```text
program failed: switch 7 stopped at t=3400 ms in on_timer
  ran out of fuel -- this one call used more than 4,000,000 instructions.
  That is the per-call ceiling, not a budget for the whole run, so the
  cause is almost always a loop that never ends.
```

Read that before you read the report card. A program that stopped early
keeps forwarding on the routes it had at the moment it died, and nothing
updates them afterwards. From the outside that looks exactly like a
routing bug — stale routes, no recovery after a link dies — so it is easy
to spend a long time fixing code that was never wrong.

Running out of fuel and panicking are reported differently, because the
fix is different. "Ran out of fuel" means a loop that does not end.
"Trapped" means the handler panicked: an `unwrap` on `None`, an index past
the end of a slice, a divide by zero.

## What `init` may declare

These are checked once, when your program is installed. Declaring more than
any of them means your program does not load at all, and the error names
what was over.

| Limit | Value |
|-------|-------|
| Tables | 16 |
| Entries per table | 1,024 |
| Register arrays | 16 |
| Slots per register array | 4,096 |
| Counter arrays | 16 |
| Slots per counter array | 4,096 |

For scale: worlds have between 5 and 15 switches, and one customer app
on each. A correct solution needs tens of routes, not thousands.

## Per switch, in memory

| Limit | Value | What happens if you exceed it |
|-------|-------|-------------------------------|
| Linear memory, per switch | 128 MiB | The allocation fails, your program stops, and the run ends naming this ceiling. |

This is a guard against one runaway allocation taking the grader down, not
a budget to design against. Measured, not guessed: a complete solution
peaks near **1 MiB**, on a 6-switch world and a 15-switch world alike,
because what it holds is the allocator's own footprint rather than
anything that grows with the network. The ceiling is about 100x that.

If you ever hit it, the cause is almost certainly state that grows on every
event and is never bounded: a record of every packet seen, or a map keyed by
something that keeps changing.

## Per program, in the data plane

| Limit | Value | What happens if you exceed it |
|-------|-------|-------------------------------|
| TinyVM stages | 8 | The program is refused at load: "the program has N TinyVM stages; a pipeline has at most 8 -- recirculate instead". |

A packet pays one stage's processing delay for every stage of the
pipeline -- eight if your program has eight, four if it has four or
fewer (the hardware pipeline is four deep; a shorter program does not
make it faster). A program that needs more work per packet recirculates
it, which costs a second pass through the whole pipeline and is the
trade-off the assignment is about.

## Per stage, in the data plane

Each TinyVM stage declares its own budget when you build it:

- `max_alu_ops` — arithmetic and logic operations the stage may run.
- `max_memory_accesses` — table lookups, register and counter touches.

These are yours to choose. The simulator does not currently reject a
stage that exceeds its own declared budget; treat the numbers as
documentation of what your stage intends to do.

## Per switch, in buffer (Assignment 2)

| Limit | Value | What happens if you exceed it |
|-------|-------|-------------------------------|
| Buffer pool, per switch | `buffer_budget_packets` x `packet_size_bytes` bytes, from the world's `[params]` (the handout's *[B]*: 40 x 1000 = 40,000 bytes) | The packet is dropped as `queue full`. Every port on the switch draws from this one pool. |
| Queues per port | up to 16 | Larger `set_queues` lists are truncated. |
| Queue cap, drop line, DRR weight | in full-size packets; whatever you ask for | A cap larger than the pool is harmless — the pool binds first. |

A "packet" of cap or drop line is a full-size packet, `packet_size_bytes`
bytes, and the pool and the queues are charged in bytes. Most customers
send full-size packets; a customer whose row has its own `size_bytes`
sends smaller ones (Jane Street and Fortnite: 200 bytes), an ack is 40
bytes, and a packet your own controller injects is 20 bytes plus its
payload. Each takes only its own size, so a queue capped at 4 packets
holds 4 full-size packets or 20 of Fortnite's.

You are told what you dropped: every `on_timer` carries `port_drops`, one
entry per port that refused a packet since your previous timer, with the
count, how many of those were your own drop threshold, and the split by
queue. You are also told, at `init`, how fast the link on each of your
ports is. Nothing else about queue state is reported; occupancy you must
infer.

## Control time (Assignment 2)

Every action your controller returns — a route install, a queue
configuration, a `link_down` — reaches the data plane
`control_delay_us` (the handout's *[Δ]*) after the call that returned it,
plus a few hundred nanoseconds of config-pipe serialization. Until then
the switch runs on what it had.

## Not limited

How many packets you send. Control traffic you invent
is never counted as workload delivery — but it does consume link bandwidth
like anything else, so flooding is not free.

Grading additionally stops any single run that runs past a wall-clock limit,
as a backstop against a program that hangs the simulator. A 60-second Part 2
world simulates in well under a second, so the limit sits orders of magnitude
above honest work and should never be something you think about.
