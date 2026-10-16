use crate::types::{AppId, LinkId, NodeId, PortId, SwitchId};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Node {
    Switch(SwitchId),
    App(AppId),
}

impl Node {
    pub fn as_node_id(&self) -> NodeId {
        match self {
            Node::Switch(s) => NodeId::new(0x8000_0000 | s.raw()),
            Node::App(a) => NodeId::new(a.raw()),
        }
    }

    /// Inverse of [`as_node_id`](Self::as_node_id).
    pub fn from_node_id(id: NodeId) -> Node {
        if (id.raw() & 0x8000_0000) != 0 {
            Node::Switch(SwitchId::new(id.raw() & 0x7fff_ffff))
        } else {
            Node::App(AppId::new(id.raw()))
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Endpoint {
    pub node: Node,
    pub port: PortId,
}

#[derive(Default, Debug)]
pub struct Network {
    /// Map from (node, port) -> link id. Each side of the link is registered.
    pub endpoint_to_link: HashMap<(NodeId, u16), LinkId>,
    /// All link ids known.
    pub links: Vec<LinkId>,
}

impl Network {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_link(&mut self, link: LinkId, a: Endpoint, b: Endpoint) {
        self.endpoint_to_link
            .insert((a.node.as_node_id(), a.port.raw()), link);
        self.endpoint_to_link
            .insert((b.node.as_node_id(), b.port.raw()), link);
        self.links.push(link);
    }

    pub fn link_at(&self, node: Node, port: PortId) -> Option<LinkId> {
        self.endpoint_to_link
            .get(&(node.as_node_id(), port.raw()))
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_lookup() {
        let mut n = Network::new();
        n.register_link(
            LinkId::new(0),
            Endpoint {
                node: Node::App(AppId::new(1)),
                port: PortId::new(0),
            },
            Endpoint {
                node: Node::Switch(SwitchId::new(2)),
                port: PortId::new(3),
            },
        );
        assert_eq!(
            n.link_at(Node::App(AppId::new(1)), PortId::new(0)),
            Some(LinkId::new(0))
        );
        assert_eq!(
            n.link_at(Node::Switch(SwitchId::new(2)), PortId::new(3)),
            Some(LinkId::new(0))
        );
    }
}
