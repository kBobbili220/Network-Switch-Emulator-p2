//! Line-oriented assembler for TinyVM programs.
//!
//! Grammar (one instruction per line, blank lines and `# ...` comments ignored):
//!
//! ```text
//! stage [alu=N] [mem=M]            # begins a new stage; defaults alu=32 mem=1
//!     load    rD, <field>          # field: ip_dst, ip_src, ip_proto, ttl, dscp,
//!                                  #        src_port, dst_port, app_id,
//!                                  #        flow_id, size, custom0..custom3
//!     store   <field>, rS
//!     loadm   rD, mK               # metadata key K
//!     storem  mK, rS
//!     const   rD, <imm>            # imm decimal, 0x.., or 0b..
//!     add     rD, rA, rB
//!     sub     rD, rA, rB
//!     and     rD, rA, rB
//!     or      rD, rA, rB
//!     xor     rD, rA, rB
//!     eq      rD, rA, rB
//!     lt      rD, rA, rB
//!     table   tT, rKey -> mResult  # table_id T, key reg, store entry id in metaKey
//!     rread   aA[rIdx] -> rD       # register array A
//!     rwrite  aA[rIdx], rS
//!     counter cC[rIdx], rVal
//!     bif     rCond -> N           # branch to instruction index N (forward only)
//!     drop
//!     punt    [no_route|ttl_expired|<u32>]
//!     egress  <port>
//!     queue   <queue>
//!     recirc
//!     noop
//! ```
//!
//! Forward branches are required to be acyclic by the simulator's validator.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::*;

#[derive(Debug, Clone)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl ParseError {
    fn at(line: usize, msg: impl Into<String>) -> Self {
        Self { line, message: msg.into() }
    }
}

#[cfg(feature = "std")]
impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ParseError {}

pub fn parse_tiny_program(input: &str) -> Result<TinyProgramW, ParseError> {
    let mut stages: Vec<StageProgramW> = Vec::new();
    let mut current: Option<StageProgramW> = None;

    for (idx, raw) in input.lines().enumerate() {
        let line_no = idx + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        // Tokenize on commas and whitespace.
        let head = first_token(line);
        if head == "stage" {
            if let Some(s) = current.take() {
                stages.push(s);
            }
            let (alu, mem) = parse_stage_attrs(line, line_no)?;
            current = Some(StageProgramW {
                instrs: Vec::new(),
                max_alu_ops: alu,
                max_memory_accesses: mem,
            });
            continue;
        }

        let stage = current.as_mut().ok_or_else(|| {
            ParseError::at(line_no, "instruction outside any `stage` block")
        })?;
        let instr = parse_instr(line, line_no)?;
        stage.instrs.push(instr);
    }

    if let Some(s) = current.take() {
        stages.push(s);
    }
    if stages.is_empty() {
        return Err(ParseError::at(0, "program has no stages"));
    }
    Ok(TinyProgramW { stages })
}

fn strip_comment(s: &str) -> &str {
    match s.find('#') {
        Some(i) => &s[..i],
        None => s,
    }
}

fn first_token(s: &str) -> &str {
    s.split(|c: char| c.is_whitespace() || c == ',')
        .find(|t| !t.is_empty())
        .unwrap_or("")
}

fn split_tokens(s: &str) -> Vec<&str> {
    s.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .collect()
}

fn parse_stage_attrs(line: &str, line_no: usize) -> Result<(u32, u8), ParseError> {
    let mut alu = 32u32;
    let mem_default = 1u8;
    let mut mem = mem_default;
    let toks = split_tokens(line);
    // toks[0] == "stage"
    for tok in &toks[1..] {
        if let Some(rest) = tok.strip_prefix("alu=") {
            alu = rest
                .parse()
                .map_err(|_| ParseError::at(line_no, format!("bad alu: {tok}")))?;
        } else if let Some(rest) = tok.strip_prefix("mem=") {
            mem = rest
                .parse()
                .map_err(|_| ParseError::at(line_no, format!("bad mem: {tok}")))?;
        } else {
            return Err(ParseError::at(line_no, format!("unknown stage attr: {tok}")));
        }
    }
    Ok((alu, mem))
}

fn parse_reg(s: &str, line_no: usize) -> Result<RegW, ParseError> {
    let n = s
        .strip_prefix('r')
        .ok_or_else(|| ParseError::at(line_no, format!("expected register, got `{s}`")))?;
    let v: u8 = n
        .parse()
        .map_err(|_| ParseError::at(line_no, format!("bad register `{s}`")))?;
    Ok(RegW(v))
}

fn parse_meta(s: &str, line_no: usize) -> Result<MetaKeyW, ParseError> {
    let n = s
        .strip_prefix('m')
        .ok_or_else(|| ParseError::at(line_no, format!("expected meta key (mN), got `{s}`")))?;
    let v: u32 = parse_int_u32(n)
        .ok_or_else(|| ParseError::at(line_no, format!("bad meta key `{s}`")))?;
    Ok(MetaKeyW(v))
}

fn parse_table(s: &str, line_no: usize) -> Result<TableIdW, ParseError> {
    let n = s
        .strip_prefix('t')
        .ok_or_else(|| ParseError::at(line_no, format!("expected table id (tN), got `{s}`")))?;
    let v: u32 = parse_int_u32(n)
        .ok_or_else(|| ParseError::at(line_no, format!("bad table id `{s}`")))?;
    Ok(TableIdW(v))
}

fn parse_array(prefix: char, s: &str, line_no: usize) -> Result<u32, ParseError> {
    let n = s
        .strip_prefix(prefix)
        .ok_or_else(|| ParseError::at(line_no, format!("expected `{prefix}N`, got `{s}`")))?;
    parse_int_u32(n).ok_or_else(|| ParseError::at(line_no, format!("bad id `{s}`")))
}

fn parse_int_u32(s: &str) -> Option<u32> {
    if let Some(h) = s.strip_prefix("0x") {
        u32::from_str_radix(h, 16).ok()
    } else if let Some(b) = s.strip_prefix("0b") {
        u32::from_str_radix(b, 2).ok()
    } else {
        s.parse().ok()
    }
}

fn parse_int_u64(s: &str) -> Option<u64> {
    if let Some(h) = s.strip_prefix("0x") {
        u64::from_str_radix(h, 16).ok()
    } else if let Some(b) = s.strip_prefix("0b") {
        u64::from_str_radix(b, 2).ok()
    } else {
        s.parse().ok()
    }
}

fn parse_field(s: &str, line_no: usize) -> Result<PacketFieldW, ParseError> {
    Ok(match s {
        "ip_src" => PacketFieldW::IpSrc,
        "ip_dst" => PacketFieldW::IpDst,
        "ip_proto" => PacketFieldW::IpProto,
        "ttl" | "ip_ttl" => PacketFieldW::IpTtl,
        "dscp" | "ip_dscp" => PacketFieldW::IpDscp,
        "src_port" => PacketFieldW::SrcPort,
        "dst_port" => PacketFieldW::DstPort,
        "app_id" => PacketFieldW::AppId,
        "flow_id" => PacketFieldW::FlowId,
        "size" => PacketFieldW::Size,
        "kind" => PacketFieldW::Kind,
        "label_top" => PacketFieldW::LabelTop,
        "label_depth" => PacketFieldW::LabelDepth,
        s if s.starts_with("custom") => {
            let n = &s[6..];
            let v: u8 = n
                .parse()
                .map_err(|_| ParseError::at(line_no, format!("bad custom field `{s}`")))?;
            PacketFieldW::Custom(v)
        }
        other => return Err(ParseError::at(line_no, format!("unknown field `{other}`"))),
    })
}

fn parse_punt_reason(toks: &[&str], line_no: usize) -> Result<PuntReasonW, ParseError> {
    if toks.is_empty() {
        return Ok(PuntReasonW::Custom(0));
    }
    Ok(match toks[0] {
        "no_route" => PuntReasonW::NoRoute,
        "ttl_expired" => PuntReasonW::TtlExpired,
        other => {
            let v = parse_int_u32(other)
                .ok_or_else(|| ParseError::at(line_no, format!("bad punt reason `{other}`")))?;
            PuntReasonW::Custom(v)
        }
    })
}

fn parse_instr(line: &str, line_no: usize) -> Result<InstrW, ParseError> {
    // Split into tokens, but preserve "->" as its own token.
    // We replace "->" with " -> " before splitting.
    let normalized = line.replace("->", " -> ");
    let toks = split_tokens(&normalized);
    let op = toks[0];

    fn need(toks: &[&str], n: usize, line_no: usize, op: &str) -> Result<(), ParseError> {
        if toks.len() < n {
            return Err(ParseError::at(line_no, format!("`{op}` needs {} args", n - 1)));
        }
        Ok(())
    }

    Ok(match op {
        "load" => {
            need(&toks, 3, line_no, op)?;
            InstrW::LoadField {
                dst: parse_reg(toks[1], line_no)?,
                field: parse_field(toks[2], line_no)?,
            }
        }
        "store" => {
            need(&toks, 3, line_no, op)?;
            InstrW::StoreField {
                field: parse_field(toks[1], line_no)?,
                src: parse_reg(toks[2], line_no)?,
            }
        }
        "loadm" => {
            need(&toks, 3, line_no, op)?;
            InstrW::LoadMeta {
                dst: parse_reg(toks[1], line_no)?,
                key: parse_meta(toks[2], line_no)?,
            }
        }
        "storem" => {
            need(&toks, 3, line_no, op)?;
            InstrW::StoreMeta {
                key: parse_meta(toks[1], line_no)?,
                src: parse_reg(toks[2], line_no)?,
            }
        }
        "const" => {
            need(&toks, 3, line_no, op)?;
            InstrW::Const {
                dst: parse_reg(toks[1], line_no)?,
                value: parse_int_u64(toks[2])
                    .ok_or_else(|| ParseError::at(line_no, format!("bad const `{}`", toks[2])))?,
            }
        }
        "add" | "sub" | "and" | "or" | "xor" | "eq" | "lt" => {
            need(&toks, 4, line_no, op)?;
            let dst = parse_reg(toks[1], line_no)?;
            let a = parse_reg(toks[2], line_no)?;
            let b = parse_reg(toks[3], line_no)?;
            match op {
                "add" => InstrW::Add { dst, a, b },
                "sub" => InstrW::Sub { dst, a, b },
                "and" => InstrW::And { dst, a, b },
                "or"  => InstrW::Or  { dst, a, b },
                "xor" => InstrW::Xor { dst, a, b },
                "eq"  => InstrW::Eq  { dst, a, b },
                "lt"  => InstrW::Lt  { dst, a, b },
                _ => unreachable!(),
            }
        }
        "table" => {
            // table tT, rKey -> mResult
            need(&toks, 5, line_no, op)?;
            if toks[3] != "->" {
                return Err(ParseError::at(line_no, "expected `->` in table instr"));
            }
            InstrW::TableLookup {
                table_id: parse_table(toks[1], line_no)?,
                key_reg: parse_reg(toks[2], line_no)?,
                result_meta: parse_meta(toks[4], line_no)?,
            }
        }
        "rread" => {
            // rread aA[rIdx] -> rD
            need(&toks, 4, line_no, op)?;
            let (array, index) = parse_array_index(toks[1], line_no, 'a')?;
            if toks[2] != "->" {
                return Err(ParseError::at(line_no, "expected `->` in rread"));
            }
            InstrW::RegisterRead {
                array: RegisterArrayIdW(array),
                index,
                dst: parse_reg(toks[3], line_no)?,
            }
        }
        "rwrite" => {
            // rwrite aA[rIdx], rS
            need(&toks, 3, line_no, op)?;
            let (array, index) = parse_array_index(toks[1], line_no, 'a')?;
            InstrW::RegisterWrite {
                array: RegisterArrayIdW(array),
                index,
                src: parse_reg(toks[2], line_no)?,
            }
        }
        "counter" => {
            // counter cC[rIdx], rVal
            need(&toks, 3, line_no, op)?;
            let (counter, index) = parse_array_index(toks[1], line_no, 'c')?;
            InstrW::CounterAdd {
                counter: CounterArrayIdW(counter),
                index,
                value: parse_reg(toks[2], line_no)?,
            }
        }
        "bif" => {
            // bif rCond -> N
            need(&toks, 4, line_no, op)?;
            if toks[2] != "->" {
                return Err(ParseError::at(line_no, "expected `->` in bif"));
            }
            InstrW::BranchIf {
                cond: parse_reg(toks[1], line_no)?,
                target: parse_int_u32(toks[3])
                    .ok_or_else(|| ParseError::at(line_no, format!("bad target `{}`", toks[3])))?,
            }
        }
        "drop" => InstrW::Drop,
        "punt" => InstrW::Punt {
            reason: parse_punt_reason(&toks[1..], line_no)?,
        },
        "egress" => {
            need(&toks, 2, line_no, op)?;
            let p = parse_int_u32(toks[1])
                .ok_or_else(|| ParseError::at(line_no, format!("bad port `{}`", toks[1])))?;
            InstrW::SetEgress { port: PortIdW(p as u16) }
        }
        "queue" => {
            need(&toks, 2, line_no, op)?;
            let q = parse_int_u32(toks[1])
                .ok_or_else(|| ParseError::at(line_no, format!("bad queue `{}`", toks[1])))?;
            InstrW::SetQueue { queue: QueueIdW(q as u16) }
        }
        "recirc" => InstrW::Recirculate,
        "noop" => InstrW::Noop,
        "push_label" => {
            need(&toks, 2, line_no, op)?;
            let v = parse_int_u32(toks[1])
                .ok_or_else(|| ParseError::at(line_no, format!("bad label `{}`", toks[1])))?;
            InstrW::PushLabel { label: v }
        }
        "pop_label" => InstrW::PopLabel,
        "swap_label" => {
            need(&toks, 2, line_no, op)?;
            let v = parse_int_u32(toks[1])
                .ok_or_else(|| ParseError::at(line_no, format!("bad label `{}`", toks[1])))?;
            InstrW::SwapLabel { label: v }
        }
        "mark_trace" => InstrW::MarkTrace,
        other => return Err(ParseError::at(line_no, format!("unknown opcode `{other}`"))),
    })
}

fn parse_array_index(
    s: &str,
    line_no: usize,
    prefix: char,
) -> Result<(u32, RegW), ParseError> {
    // Accept "aA[rIdx]" — bracketed register index.
    let lb = s
        .find('[')
        .ok_or_else(|| ParseError::at(line_no, format!("expected `[` in `{s}`")))?;
    let rb = s
        .find(']')
        .ok_or_else(|| ParseError::at(line_no, format!("expected `]` in `{s}`")))?;
    if rb < lb {
        return Err(ParseError::at(line_no, format!("malformed `{s}`")));
    }
    let head = &s[..lb];
    let inner = &s[lb + 1..rb];
    let array = parse_array(prefix, head, line_no)?;
    let index = parse_reg(inner, line_no)?;
    Ok((array, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_program() {
        let src = r#"
            # one stage that loads ip_dst and looks it up in table 1
            stage alu=4 mem=1
                load    r0, ip_dst
                table   t1, r0 -> m0
        "#;
        let p = parse_tiny_program(src).unwrap();
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].max_alu_ops, 4);
        assert_eq!(p.stages[0].instrs.len(), 2);
    }

    #[test]
    fn parses_arith_and_branch() {
        let src = r#"
            stage
                const   r0, 3
                const   r1, 0x4
                add     r2, r0, r1
                eq      r3, r2, r1
                bif     r3 -> 6
                drop
                noop
        "#;
        let p = parse_tiny_program(src).unwrap();
        assert_eq!(p.stages[0].instrs.len(), 7);
    }

    #[test]
    fn parses_register_and_counter() {
        let src = r#"
            stage
                const   r0, 1
                rwrite  a0[r0], r0
                rread   a0[r0] -> r1
                counter c0[r0], r1
                egress  2
                punt    no_route
                recirc
        "#;
        let p = parse_tiny_program(src).unwrap();
        assert_eq!(p.stages[0].instrs.len(), 7);
    }

    #[test]
    fn rejects_orphan_instruction() {
        let err = parse_tiny_program("noop\n").unwrap_err();
        assert!(err.message.contains("outside"));
    }
}
