//! Example switch program: installs a static route on the first punt.
//!
//! Demonstrates branching on `switch_id` so a single program can ship
//! per-switch behavior. The owner could compile this once and install it on
//! every switch they own; each instance differentiates by switch_id.

use switch_program_sdk::*;

pub struct LearningSwitch {
    switch_id: u32,
    installed: bool,
}

impl SwitchProgram for LearningSwitch {
    fn init(switch_id: u32, _local_ports: Vec<u16>) -> (Self, ProgramSetup) {
        let mut setup = ProgramSetup::new();
        setup.declare_table(1, MatchKind::Exact, 16);
        setup.set_program(
            text::parse_tiny_program(
                "
                stage alu=4 mem=1
                    load    r0, ip_dst
                    table   t1, r0 -> m0
                ",
            )
            .unwrap(),
        );
        (
            Self {
                switch_id,
                installed: false,
            },
            setup,
        )
    }

    fn on_punt(&mut self, ev: PuntEvent) -> Vec<Action> {
        if self.installed {
            return Vec::new();
        }
        self.installed = true;
        // Switch 0 sends to port 1, switch 1 sends to port 2 — one program,
        // per-switch behavior keyed by switch_id.
        let egress = if self.switch_id == 0 { 1 } else { 2 };
        vec![actions::install_route(
            1,
            42,
            ev.ip_dst as u64,
            32,
            egress,
        )]
    }
}

switch_program!(LearningSwitch);
