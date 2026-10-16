use std::time::Duration;

pub type SimTime = Duration;

macro_rules! id_type {
    ($name:ident, $inner:ty) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        pub struct $name(pub $inner);

        impl From<$inner> for $name {
            fn from(v: $inner) -> Self {
                Self(v)
            }
        }

        impl $name {
            pub fn new(v: $inner) -> Self {
                Self(v)
            }
            pub fn raw(self) -> $inner {
                self.0
            }
        }
    };
}

id_type!(PacketId, u64);
id_type!(NodeId, u32);
id_type!(SwitchId, u32);
id_type!(AppId, u32);
id_type!(LinkId, u32);
id_type!(FlowId, u64);
id_type!(PortId, u16);
id_type!(QueueId, u16);
id_type!(TableId, u32);
id_type!(RegisterArrayId, u32);
id_type!(CounterArrayId, u32);
id_type!(EntryId, u64);
id_type!(MetaKey, u32);
id_type!(Reg, u8);
id_type!(InstrIndex, u32);
id_type!(OwnerId, u32);
id_type!(AsId, u32);
id_type!(BgpMsgId, u64);
id_type!(PromiseId, u64);
id_type!(TraceId, u64);

/// IPv4 prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Prefix {
    pub addr: u32,
    pub len: u8,
}

impl Prefix {
    pub fn new(addr: u32, len: u8) -> Self {
        Self { addr, len }
    }

    pub fn contains(&self, ip: u32) -> bool {
        if self.len == 0 {
            return true;
        }
        if self.len >= 32 {
            return self.addr == ip;
        }
        let mask: u32 = !0u32 << (32 - self.len);
        (ip & mask) == (self.addr & mask)
    }
}
