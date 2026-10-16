//! Wire types for SimpleBGP.
//!
//! A SimpleBGP packet carries one `BgpEnvelopeW` postcard-encoded in the
//! packet's payload. The `Packet.kind` field is set to `SimpleBgp` so the
//! data plane can distinguish it without parsing the payload.
//!
//! Wasm programs can build envelopes using only this module — the host
//! mirrors every type into Rust-native equivalents on receipt.

use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AsIdW(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrefixW {
    pub addr: u32,
    pub len: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TrafficClassW {
    BestEffort,
    LowLatency,
    Throughput,
    UptimeSensitive,
    LowLatencyThroughput,
    Vod,
    Gaming,
    Enterprise,
    Backup,
    Hft,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsPathW {
    pub ases: Vec<AsIdW>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BgpTagW {
    NoExport,
    BackupOnly,
    LowLatencyHint,
    BulkOkay,
    Blackhole,
    Experimental(u32),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeepAliveW {
    pub nonce: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckW {
    pub acked_msg_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutePromiseW {
    pub promise_id: u64,
    pub prefix: PrefixW,
    pub traffic_class: Option<TrafficClassW>,
    pub promised_paths: Vec<AsPathW>,
    pub tags: Vec<BgpTagW>,
    pub valid_until_ns: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteWithdrawW {
    pub withdrawn_promise_id: Option<u64>,
    pub prefix: Option<PrefixW>,
    pub traffic_class: Option<TrafficClassW>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SimpleBgpMessageW {
    KeepAlive(KeepAliveW),
    Ack(AckW),
    Promise(RoutePromiseW),
    Withdraw(RouteWithdrawW),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BgpEnvelopeW {
    pub msg_id: u64,
    pub sender_as: AsIdW,
    pub receiver_as: AsIdW,
    pub sender_bgp_ip: u32,
    pub receiver_bgp_ip: u32,
    pub message: SimpleBgpMessageW,
}

/// Encode a `BgpEnvelopeW` to a postcard byte vec for use as a packet payload.
pub fn encode_envelope(envelope: &BgpEnvelopeW) -> Vec<u8> {
    postcard::to_allocvec(envelope).expect("postcard encode")
}

pub fn decode_envelope(bytes: &[u8]) -> Result<BgpEnvelopeW, postcard::Error> {
    postcard::from_bytes(bytes)
}
