use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

use competitive_net_sim::app::{App, TrafficPattern};
use competitive_net_sim::controller::NoopController;
use competitive_net_sim::link::LinkConfig;
use competitive_net_sim::network::Node;
use competitive_net_sim::packet::{IpAddr, PacketField};
use competitive_net_sim::sim::Simulator;
use competitive_net_sim::switch::{Switch, SwitchConfig};
use competitive_net_sim::tinyvm::{
    Instr, MatchActionTable, MatchKind, StageProgram, TableAction, TableEntry, TinyProgram,
    TinyVmState,
};
use competitive_net_sim::types::{AppId, EntryId, MetaKey, PortId, Reg, SwitchId, TableId};

#[derive(Parser, Debug)]
#[command(
    name = "competitive_net_sim",
    about = "Discrete-event packet network simulator",
    version,
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run a world file: build the topology it declares, run it, and
    /// report what the apps saw.
    RunWorld {
        /// Path to the world's TOML file.
        world: PathBuf,

        /// Optional path to a `.simlog` file.
        #[arg(long)]
        log: Option<PathBuf>,

        /// Override the world's own run length, in milliseconds.
        #[arg(long)]
        until_ms: Option<u64>,

        /// Path to a failure schedule. Without it, no link ever dies —
        /// that is Part 1.
        #[arg(long)]
        failures: Option<PathBuf>,

        /// Multiply every app's offered load: the same world at 0.5×, 1×,
        /// 2× demand.
        #[arg(long, default_value_t = 1.0)]
        load_scale: f64,

        /// Print a report card at the end. Needs an event log, so one is
        /// written to a scratch file when `--log` is not given.
        #[arg(long)]
        score: bool,

        /// With `--score`, also write the card as JSON to this path (the
        /// grading pipeline reads it; the text card is for people).
        #[arg(long)]
        json: Option<PathBuf>,

        /// Ignore packets sent before this point. A program is allowed to
        /// not know the topology yet. Defaults to the world's own value.
        #[arg(long)]
        warmup_ms: Option<u64>,

        /// Whole-run delivery floor, as a percentage. Defaults to the
        /// world's own value.
        #[arg(long)]
        delivery_floor_pct: Option<f64>,

        /// Path to a `.wasm` switch program to install on every switch.
        /// Without it the switches run empty and forward nothing — the
        /// hello-world baseline.
        #[cfg(feature = "wasm")]
        #[arg(long)]
        program: Option<PathBuf>,
    },

    /// Generate world files. Same seed, same bytes, forever.

    /// Generate failure schedules for a world. Separate seed from the
    /// world's, so one topology can host many schedules.

    /// Score a `.simlog` that already exists, and print the report card.
    Score {
        /// Path to the `.simlog` file.
        log: PathBuf,

        /// The world the run was produced from.
        #[arg(long)]
        world: PathBuf,

        /// The failure schedule, if the run had one. Only the recovery
        /// budget is read from it; the events come from the log.
        #[arg(long)]
        failures: Option<PathBuf>,

        /// Ignore packets sent before this point. Defaults to the
        /// world's own value.
        #[arg(long)]
        warmup_ms: Option<u64>,

        /// How long a program gets to recover after each link event.
        /// Taken from the schedule when one is given.
        #[arg(long)]
        recovery_budget_ms: Option<u64>,

        /// Whole-run delivery floor, as a percentage. Defaults to the
        /// world's own value.
        #[arg(long)]
        delivery_floor_pct: Option<f64>,
    },

    /// Run the built-in demo topology (one app pair through one switch).
    Simulate {
        /// Optional path to a `.simlog` file. When set, every topology
        /// change, link/switch state change, packet ingress/egress,
        /// table edit, and program install is recorded.
        #[arg(long)]
        log: Option<PathBuf>,

        /// How long to run, in milliseconds of simulated time.
        #[arg(long, default_value_t = 100)]
        until_ms: u64,
    },
}

fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Simulate { log, until_ms } => run_demo(log, until_ms),
        Cmd::Score {
            log,
            world,
            failures,
            warmup_ms,
            recovery_budget_ms,
            delivery_floor_pct,
        } => score_log(
            log,
            world,
            failures,
            warmup_ms,
            recovery_budget_ms,
            delivery_floor_pct,
            None,
        ),
        #[cfg(feature = "wasm")]
        Cmd::RunWorld {
            world,
            log,
            until_ms,
            failures,
            load_scale,
            score,
            json,
            warmup_ms,
            delivery_floor_pct,
            program,
        } => run_world(
            world,
            log,
            until_ms,
            failures,
            load_scale,
            score,
            json,
            warmup_ms,
            delivery_floor_pct,
            program,
        ),
        #[cfg(not(feature = "wasm"))]
        Cmd::RunWorld {
            world,
            log,
            until_ms,
            failures,
            load_scale,
            score,
            json,
            warmup_ms,
            delivery_floor_pct,
        } => run_world(
            world,
            log,
            until_ms,
            failures,
            load_scale,
            score,
            json,
            warmup_ms,
            delivery_floor_pct,
        ),
    }
}



#[allow(clippy::too_many_arguments)]
fn run_world(
    path: PathBuf,
    log: Option<PathBuf>,
    until_ms: Option<u64>,
    failures: Option<PathBuf>,
    load_scale: f64,
    score: bool,
    json: Option<PathBuf>,
    warmup_ms: Option<u64>,
    delivery_floor_pct: Option<f64>,
    #[cfg(feature = "wasm")] program: Option<PathBuf>,
) -> std::io::Result<()> {
    let world = competitive_net_sim::world::World::load(&path).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })?;

    // The scorer reads a log, so asking for a score means writing one.
    // When the caller did not name a file we put it in the temp directory
    // and delete it afterwards. The name carries the pid: two processes
    // scoring the same world at once (a parallel sweep) would otherwise
    // share one file and read each other's half-written bytes.
    let scratch = std::env::temp_dir().join(format!(
        "{}-{}-score.simlog",
        world.world.name,
        std::process::id()
    ));
    let log_path: Option<PathBuf> = match (&log, score) {
        (Some(p), _) => Some(p.clone()),
        (None, true) => Some(scratch.clone()),
        (None, false) => None,
    };

    let mut sim = Simulator::new();
    if let Some(p) = log_path.as_ref() {
        sim.start_logging(p)?;
    }
    let handles = world.build_with_load(&mut sim, load_scale).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })?;

    #[cfg(feature = "wasm")]
    if let Some(prog) = program.as_ref() {
        let bytes = std::fs::read(prog)?;
        for as_id in &handles.as_ids {
            sim.install_program(
                competitive_net_sim::types::OwnerId::new(*as_id),
                &bytes,
                competitive_net_sim::wasm::WasmLimits::default(),
            )
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("installing {}: {e:?}", prog.display()),
                )
            })?;
        }
        // Bootstrap one controller timer per switch; the program
        // re-schedules itself from there.
        for s in &world.switches {
            sim.schedule_controller_timer(SwitchId::new(s.id), Duration::ZERO);
        }
    }

    // A failure schedule, if one was given. The world is the topology;
    // the schedule is what happens to it. They are separate files so one
    // world can host many schedules.
    let schedule = match failures.as_ref() {
        None => None,
        Some(p) => {
            let s = competitive_net_sim::failures::Schedule::load(p).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
            })?;
            s.validate(&world).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
            })?;
            Some(s)
        }
    };

    // A schedule declares the run length it needs. Running past it pads
    // the delivery rate with clean time and flatters the result, so the
    // schedule wins over the world's own duration unless told otherwise.
    let until = match (until_ms, schedule.as_ref()) {
        (Some(ms), _) => Duration::from_millis(ms),
        (None, Some(s)) => Duration::from_millis(s.schedule.duration_ms),
        (None, None) => world.params.duration(),
    };

    if let Some(sched) = schedule.as_ref() {
        use competitive_net_sim::failures::Action;
        for (at, (a, b), action) in sched.events() {
            let at = Duration::from_millis(at);
            if at > until {
                break;
            }
            sim.run_until(at);
            let link = handles.links[&(a, b)];
            match action {
                Action::Down => sim.fail_link(link),
                Action::Up => sim.restore_link(link),
            }
            println!(
                "  t={:>6} ms  link {a}-{b} {}",
                at.as_millis(),
                match action {
                    Action::Down => "DOWN",
                    Action::Up => "UP",
                }
            );
        }
    }
    sim.run_until(until);

    println!(
        "world '{}': {} switches, {} links, {} apps, ran {} ms",
        world.world.name,
        world.switches.len(),
        handles.link_order.len(),
        world.apps.len(),
        until.as_millis()
    );
    let mut sent = 0u64;
    let mut recv = 0u64;
    // By app number, so two runs print the same lines in the same order.
    let mut by_id: Vec<_> = sim.apps.iter().collect();
    by_id.sort_by_key(|(id, _)| id.raw());
    for (id, app) in by_id {
        sent += app.metrics.packets_sent;
        recv += app.metrics.packets_received;
        println!(
            "  app {:>3}  sent {:>6}  received {:>6}",
            id.raw(),
            app.metrics.packets_sent,
            app.metrics.packets_received
        );
    }
    let rate = if sent == 0 {
        0.0
    } else {
        100.0 * recv as f64 / sent as f64
    };
    println!("  total: sent {sent}, received {recv} ({rate:.2}%)");
    if let Some(p) = log.as_ref() {
        println!("event log: {}", p.display());
    }

    // A program that died mid-run has already said so on stderr, at the
    // moment it happened. Repeat the count here so it cannot be lost in
    // the scroll above a failing report card.
    let stopped = sim
        .switches
        .values()
        .filter(|s| s.controller.failure().is_some())
        .count();
    if stopped > 0 {
        eprintln!(
            "\n{stopped} of {} switch programs stopped early.\n\
             \x20 Those switches kept forwarding on the routes they already had;\n\
             \x20 nothing updated them after that. The delivery number below is a\n\
             \x20 symptom -- the cause is in the 'program failed' lines above.",
            sim.switches.len()
        );
    }


    if score {
        // Dropping the logger flushes it. Nothing may read the file
        // before that happens.
        sim.log = None;
        let log_path = log_path.expect("score implies a log");
        let budget = schedule
            .as_ref()
            .map(|s| s.schedule.recovery_budget_ms)
            .unwrap_or(competitive_net_sim::failures::DEFAULT_RECOVERY_BUDGET_MS);
        // Grading parameters live in the world file. The flags override
        // them, for experiments.
        let params = competitive_net_sim::score::ScoreParams {
            warmup_ms: warmup_ms.unwrap_or(world.params.warmup_ms),
            recovery_budget_ms: budget,
            delivery_floor: delivery_floor_pct
                .unwrap_or(world.params.delivery_floor_pct)
                / 100.0,
            ..Default::default()
        };
        #[cfg(feature = "wasm")]
        let prog_name = program.as_ref().map(|p| p.display().to_string());
        #[cfg(not(feature = "wasm"))]
        let prog_name: Option<String> = None;
        // A closed-loop world is graded on the A2 floors; an open-loop
        // one on the A1 criteria.
        let (card, passed, as_json) = match world.workload {
            competitive_net_sim::world::Workload::Mixed { .. } => {
                let r = competitive_net_sim::floors::score_file(
                    &log_path,
                    &world,
                    prog_name,
                    load_scale,
                )
                .map_err(|e| std::io::Error::other(e.to_string()))?;
                let passed = r.status() == competitive_net_sim::floors::Status::Pass;
                (r.render(), passed, r.to_json())
            }
            competitive_net_sim::world::Workload::AllPairsCbr { .. } => {
                let r = competitive_net_sim::score::score_file(
                    &log_path,
                    &world,
                    schedule.as_ref(),
                    prog_name,
                    &params,
                )
                .map_err(|e| std::io::Error::other(e.to_string()))?;
                let passed = r.status() == competitive_net_sim::score::Status::Pass;
                // The A1 card has no JSON form; the pipeline that grades it
                // reads the text. Say so rather than write nothing.
                let j = serde_json::json!({
                    "assignment": "a1",
                    "passed": passed,
                    "status": if passed { "PASS" } else { "FAIL" },
                });
                (r.render(), passed, j)
            }
        };
        if let Some(path) = &json {
            std::fs::write(path, serde_json::to_string_pretty(&as_json)? + "\n")?;
        }
        println!();
        print!("{card}");
        if log.is_none() {
            let _ = std::fs::remove_file(&log_path);
        }
        if !passed {
            std::process::exit(1);
        }
    }
    Ok(())
}

/// Score a `.simlog` that already exists.
#[allow(clippy::too_many_arguments)]
fn score_log(
    log: PathBuf,
    world_path: PathBuf,
    failures: Option<PathBuf>,
    warmup_ms: Option<u64>,
    recovery_budget_ms: Option<u64>,
    delivery_floor_pct: Option<f64>,
    program: Option<String>,
) -> std::io::Result<()> {
    let invalid = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidData, m);

    let world = competitive_net_sim::world::World::load(&world_path)
        .map_err(|e| invalid(e.to_string()))?;
    let schedule = match failures.as_ref() {
        None => None,
        Some(p) => {
            let s = competitive_net_sim::failures::Schedule::load(p)
                .map_err(|e| invalid(e.to_string()))?;
            Some(s)
        }
    };

    let budget = recovery_budget_ms
        .or_else(|| schedule.as_ref().map(|s| s.schedule.recovery_budget_ms))
        .unwrap_or(competitive_net_sim::failures::DEFAULT_RECOVERY_BUDGET_MS);
    let params = competitive_net_sim::score::ScoreParams {
        warmup_ms: warmup_ms.unwrap_or(world.params.warmup_ms),
        recovery_budget_ms: budget,
        delivery_floor: delivery_floor_pct
            .unwrap_or(world.params.delivery_floor_pct)
            / 100.0,
        ..Default::default()
    };
    let report = competitive_net_sim::score::score_file(
        &log,
        &world,
        schedule.as_ref(),
        program,
        &params,
    )
    .map_err(|e| invalid(e.to_string()))?;
    print!("{}", report.render());
    if report.status() != competitive_net_sim::score::Status::Pass {
        std::process::exit(1);
    }
    Ok(())
}

fn run_demo(log: Option<PathBuf>, until_ms: u64) -> std::io::Result<()> {
    let mut sim = Simulator::new();
    if let Some(path) = log.as_ref() {
        sim.start_logging(path)?;
    }

    let app_a = AppId::new(1);
    let app_b = AppId::new(2);
    let switch_id = SwitchId::new(0);

    let a = App::new(
        app_a,
        IpAddr(0x0a000001),
        32,
        IpAddr(0x0a000002),
        32,
        TrafficPattern::ConstantBitrate {
            interval: Duration::from_millis(10),
            size_bytes: 100,
        },
    );
    let b = App::new(
        app_b,
        IpAddr(0x0a000002),
        32,
        IpAddr(0x0a000001),
        32,
        TrafficPattern::BulkTransfer {
            total_bytes: 0,
            sent_bytes: 0,
            pacing: Duration::ZERO,
            size_bytes: 0,
        },
    );

    let mut state = TinyVmState::default();
    let mut table = MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16);
    table
        .install(TableEntry {
            id: EntryId::new(1),
            key: 0x0a000002,
            prefix_len: 32,
            priority: 0,
            action: TableAction::SetEgress {
                port: PortId::new(1),
            },
        })
        .unwrap();
    state.tables.push(table);

    let prog = TinyProgram {
        stages: vec![StageProgram {
            instrs: vec![
                Instr::LoadField {
                    dst: Reg::new(0),
                    field: PacketField::IpDst,
                },
                Instr::TableLookup {
                    table_id: TableId::new(1),
                    key_reg: Reg::new(0),
                    result_meta: MetaKey::new(0),
                },
            ],
            max_alu_ops: 0,
            max_memory_accesses: 1,
        }],
    };

    let switch = Switch::new(
        SwitchConfig::defaults(switch_id),
        prog,
        state,
        Box::new(NoopController),
        4,
        1 << 16,
    );

    sim.add_switch(switch);
    sim.add_app(a);
    sim.add_app(b);

    sim.connect(
        Node::App(app_a),
        PortId::new(0),
        Node::Switch(switch_id),
        PortId::new(0),
        LinkConfig {
            latency: Duration::from_micros(100),
            bandwidth_bps: 1_000_000_000,
            queue_capacity_bytes: 1 << 16,
        },
    );
    sim.connect(
        Node::Switch(switch_id),
        PortId::new(1),
        Node::App(app_b),
        PortId::new(0),
        LinkConfig {
            latency: Duration::from_micros(100),
            bandwidth_bps: 1_000_000_000,
            queue_capacity_bytes: 1 << 16,
        },
    );

    sim.run_until(Duration::from_millis(until_ms));

    let metrics_a = &sim.apps[&app_a].metrics;
    let metrics_b = &sim.apps[&app_b].metrics;
    println!(
        "A: sent {} packets, B: received {} packets, avg delay {:?}",
        metrics_a.packets_sent, metrics_b.packets_received, metrics_b.avg_delay
    );
    if let Some(p) = log {
        println!("event log: {}", p.display());
    }
    Ok(())
}
