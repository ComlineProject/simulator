//! The simulation's state: the compiled [`ProjectShape`], the nodes on the
//! canvas (each hosting one or more instances), and the connections between
//! instances. Plain data + operations; the engine turns each [`Connection`] into
//! a live wire and the UI renders all of it.
//!
//! Ported from the playground's `model.ts`. Two changes for Rust: the id
//! counters live *in* the [`Session`] (not module globals — so independent
//! sessions in tests don't collide), and the per-function behaviour map is a
//! `BTreeMap` (deterministic serialization for shareable session links).

use std::collections::BTreeMap;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::behavior::{default_kind_for, BehaviorKind};
use crate::faults::{no_faults, FaultSpec};
use crate::format::Codec;
use crate::shape::{find_protocol, ProjectShape};

/// Which side of a protocol an instance plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Server,
    Client,
}

/// Whether the sim advances on wall-clock time or only on explicit steps. The
/// engine itself is always stepped; this records the user's choice for the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockMode {
    Real,
    Stepped,
}

/// One function's configured behaviour on a server instance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BehaviorSetting {
    pub kind: BehaviorKind,
    pub config: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub role: Role,
    /// The schema namespace + protocol this instance speaks.
    pub schema_ns: String,
    pub protocol: String,
    /// Server only: one behaviour per function name. Empty for a client.
    pub behaviors: BTreeMap<String, BehaviorSetting>,
    /// The schema's `ir_hash` when this instance was placed or last resynced. A
    /// `rebuild` does NOT touch it — so editing a schema and returning leaves a
    /// surviving instance built against the old IR, which the handshake then
    /// rejects (the version-skew demo). [`Session::resync_instance`] snaps it
    /// forward.
    pub ir_hash: String,
    /// The canvas node hosting this instance.
    pub node_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: String,
    pub label: String,
    /// Canvas position (the UI owns it; the engine ignores it).
    pub x: f64,
    pub y: f64,
    /// The instances in this box, in display order. Always ≥ 1.
    pub instance_ids: Vec<String>,
}

/// Which framing a connection speaks — `Auto` follows the protocol's `@framing`,
/// the others override it (for the framing / codec compare, 2g).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FramingChoice {
    #[default]
    Auto,
    Datagram,
    Jsonrpc,
}

impl FramingChoice {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(FramingChoice::Auto),
            "datagram" => Some(FramingChoice::Datagram),
            "jsonrpc" => Some(FramingChoice::Jsonrpc),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub id: String,
    pub client_id: String,
    pub server_id: String,
    /// The unreliable-wire spec for this connection. Mutated in place by the
    /// inspector; the engine hands the same object to both transports.
    pub faults: FaultSpec,
    /// The framing this connection speaks. Old links without the field decode
    /// as `Auto`.
    #[serde(default)]
    pub framing: FramingChoice,
    /// The wire format (JSON / MessagePack).
    #[serde(default)]
    pub wire_format: Codec,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    /// The compiled project. Recomputed from the open schemas on load, so the
    /// session link never carries it.
    #[serde(skip)]
    pub shape: ProjectShape,
    pub nodes: Vec<Node>,
    pub instances: Vec<Instance>,
    pub connections: Vec<Connection>,
    /// Fixed per-frame delivery delay for every wire, ms.
    #[serde(default)]
    pub latency_ms: f64,
    /// How long a client waits for a reply before it gives up.
    #[serde(default = "default_call_timeout_ms")]
    pub call_timeout_ms: f64,
    /// Seeds the fault RNG — a stepped run with a fixed seed is reproducible.
    #[serde(default = "default_seed")]
    pub seed: u32,
    #[serde(default = "default_clock_mode")]
    pub clock_mode: ClockMode,
    /// Monotonic id counters. Not serialized; [`Session::reseed_counters`]
    /// rebuilds them past everything in a loaded session.
    #[serde(skip)]
    next_instance: u64,
    #[serde(skip)]
    next_node: u64,
    #[serde(skip)]
    next_conn: u64,
}

// Defaults for a partial session link — mirror `decodeSession`'s `Number(…) || …`.
fn default_call_timeout_ms() -> f64 {
    3000.0
}
fn default_seed() -> u32 {
    1
}
fn default_clock_mode() -> ClockMode {
    ClockMode::Real
}

/// Where a freshly-added instance goes on the canvas.
#[derive(Clone, Debug, Default)]
pub struct Placement {
    /// Drop onto an existing node to add the instance there; otherwise a new
    /// node is created at `x` / `y`.
    pub node_id: Option<String>,
    pub x: f64,
    pub y: f64,
}

/// What an instance speaks — namespace, protocol, and side.
#[derive(Clone, Debug)]
pub struct InstanceSpec {
    pub schema_ns: String,
    pub protocol: String,
    pub role: Role,
}

impl Session {
    /// An empty session over `shape`.
    pub fn empty(shape: ProjectShape) -> Self {
        Self {
            shape,
            nodes: Vec::new(),
            instances: Vec::new(),
            connections: Vec::new(),
            latency_ms: 0.0,
            call_timeout_ms: 3000.0,
            seed: 1,
            clock_mode: ClockMode::Real,
            next_instance: 0,
            next_node: 0,
            next_conn: 0,
        }
    }

    // ── lookups ──────────────────────────────────────────────────────────

    pub fn instance(&self, id: &str) -> Option<&Instance> {
        self.instances.iter().find(|i| i.id == id)
    }

    pub fn instance_mut(&mut self, id: &str) -> Option<&mut Instance> {
        self.instances.iter_mut().find(|i| i.id == id)
    }

    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Every connection `instance_id` is an end of.
    pub fn connections_for(&self, instance_id: &str) -> Vec<&Connection> {
        self.connections
            .iter()
            .filter(|c| c.client_id == instance_id || c.server_id == instance_id)
            .collect()
    }

    // ── instances / nodes ────────────────────────────────────────────────

    /// Add an instance of `spec`, either onto an existing node or in a fresh one.
    pub fn add_instance(&mut self, spec: InstanceSpec, place: Placement) -> String {
        let n = self
            .instances
            .iter()
            .filter(|i| i.protocol == spec.protocol)
            .count()
            + 1;
        self.next_instance += 1;
        let id = format!("i{}", self.next_instance);

        let behaviors = if spec.role == Role::Server {
            self.seed_behaviors(&spec.schema_ns, &spec.protocol)
        } else {
            BTreeMap::new()
        };
        let ir_hash = find_protocol(&self.shape, &spec.schema_ns, &spec.protocol)
            .map(|(schema, _)| schema.ir_hash.clone())
            .unwrap_or_else(|| "0x0".to_string());

        let node_id = match place
            .node_id
            .as_deref()
            .and_then(|nid| self.nodes.iter_mut().find(|nd| nd.id == nid))
        {
            Some(host) => {
                host.instance_ids.push(id.clone());
                host.id.clone()
            }
            None => {
                self.next_node += 1;
                let node = Node {
                    id: format!("n{}", self.next_node),
                    label: format!("Machine {}", self.next_node),
                    x: place.x.max(0.0),
                    y: place.y.max(0.0),
                    instance_ids: vec![id.clone()],
                };
                let nid = node.id.clone();
                self.nodes.push(node);
                nid
            }
        };

        self.instances.push(Instance {
            id: id.clone(),
            name: format!("{}-{}", spec.protocol.to_lowercase(), n),
            role: spec.role,
            schema_ns: spec.schema_ns,
            protocol: spec.protocol,
            behaviors,
            ir_hash,
            node_id,
        });
        id
    }

    /// Rename a machine box. An empty / blank label is ignored.
    pub fn rename_node(&mut self, node_id: &str, label: &str) {
        if let Some(nd) = self.nodes.iter_mut().find(|n| n.id == node_id) {
            let trimmed = label.trim();
            if !trimmed.is_empty() {
                nd.label = trimmed.to_string();
            }
        }
    }

    /// Move a node (and every instance it hosts) on the canvas.
    pub fn move_node(&mut self, node_id: &str, x: f64, y: f64) {
        if let Some(nd) = self.nodes.iter_mut().find(|n| n.id == node_id) {
            nd.x = x.max(0.0).round();
            nd.y = y.max(0.0).round();
        }
    }

    /// Remove an instance, the connections it was an end of, and its node if it
    /// leaves the node empty.
    pub fn remove_instance(&mut self, id: &str) {
        let node_id = self.instance(id).map(|i| i.node_id.clone());
        self.instances.retain(|i| i.id != id);
        self.connections
            .retain(|c| c.client_id != id && c.server_id != id);
        if let Some(node_id) = node_id {
            if let Some(node) = self.nodes.iter_mut().find(|nd| nd.id == node_id) {
                node.instance_ids.retain(|x| x != id);
                if node.instance_ids.is_empty() {
                    self.nodes.retain(|nd| nd.id != node_id);
                }
            }
        }
    }

    // ── connections ──────────────────────────────────────────────────────

    /// Connect a client instance to a server instance of the same protocol.
    /// `Err` on a role / protocol mismatch or a duplicate pair.
    pub fn add_connection(&mut self, client_id: &str, server_id: &str) -> Result<String, String> {
        let client = self
            .instance(client_id)
            .ok_or_else(|| "connect: unknown instance".to_string())?;
        let server = self
            .instance(server_id)
            .ok_or_else(|| "connect: unknown instance".to_string())?;
        if client.role != Role::Client || server.role != Role::Server {
            return Err("connect: need a client and a server".to_string());
        }
        if client.schema_ns != server.schema_ns || client.protocol != server.protocol {
            return Err(format!(
                "connect: {} ≠ {}",
                client.protocol, server.protocol
            ));
        }
        if self
            .connections
            .iter()
            .any(|c| c.client_id == client_id && c.server_id == server_id)
        {
            return Err(format!(
                "connect: {} ↔ {} already connected",
                client.name, server.name
            ));
        }
        self.next_conn += 1;
        let id = format!("c{}", self.next_conn);
        self.connections.push(Connection {
            id: id.clone(),
            client_id: client_id.to_string(),
            server_id: server_id.to_string(),
            faults: no_faults(),
            framing: FramingChoice::Auto,
            wire_format: Codec::default(),
        });
        Ok(id)
    }

    /// Set a connection's framing / wire format (the compare axes, 2g).
    pub fn set_transport(&mut self, conn_id: &str, framing: FramingChoice, wire_format: Codec) {
        if let Some(conn) = self.connections.iter_mut().find(|c| c.id == conn_id) {
            conn.framing = framing;
            conn.wire_format = wire_format;
        }
    }

    pub fn remove_connection(&mut self, conn_id: &str) {
        self.connections.retain(|c| c.id != conn_id);
    }

    // ── behaviours ───────────────────────────────────────────────────────

    /// Set a server instance's behaviour for one function. `config` defaults
    /// from the kind's `default_config` when `None`.
    pub fn set_behavior(
        &mut self,
        instance_id: &str,
        fn_name: &str,
        kind: BehaviorKind,
        config: Option<Value>,
    ) -> Result<(), String> {
        let inst = self
            .instance(instance_id)
            .ok_or_else(|| "setBehavior: not a server instance".to_string())?;
        if inst.role != Role::Server {
            return Err("setBehavior: not a server instance".to_string());
        }
        let (schema, proto) = find_protocol(&self.shape, &inst.schema_ns, &inst.protocol)
            .ok_or_else(|| format!("setBehavior: no function {fn_name}"))?;
        let function = proto
            .functions
            .iter()
            .find(|f| f.name == fn_name)
            .ok_or_else(|| format!("setBehavior: no function {fn_name}"))?;
        let config = config.unwrap_or_else(|| kind.default_config(function, schema));

        self.instance_mut(instance_id)
            .unwrap()
            .behaviors
            .insert(fn_name.to_string(), BehaviorSetting { kind, config });
        Ok(())
    }

    /// Seed a server's per-function behaviour map from the protocol shape.
    fn seed_behaviors(&self, schema_ns: &str, protocol: &str) -> BTreeMap<String, BehaviorSetting> {
        let Some((schema, proto)) = find_protocol(&self.shape, schema_ns, protocol) else {
            return BTreeMap::new();
        };
        proto
            .functions
            .iter()
            .map(|f| {
                let kind = default_kind_for(f);
                (
                    f.name.clone(),
                    BehaviorSetting {
                        kind,
                        config: kind.default_config(f, schema),
                    },
                )
            })
            .collect()
    }

    // ── schema edits ─────────────────────────────────────────────────────

    /// Re-point at a freshly compiled shape. An instance survives if its
    /// `schema_ns::protocol` still exists; its behaviour map keeps the configs
    /// of functions that remain (when the kind still applies) and gains defaults
    /// for new ones. `ir_hash` is deliberately left as-is — see [`Instance::ir_hash`].
    pub fn rebuild(&mut self, shape: ProjectShape) {
        self.shape = shape;

        let mut kept = Vec::new();
        for mut inst in std::mem::take(&mut self.instances) {
            let Some((schema, proto)) = find_protocol(&self.shape, &inst.schema_ns, &inst.protocol)
            else {
                continue;
            };
            if inst.role == Role::Server {
                let mut next = BTreeMap::new();
                for f in &proto.functions {
                    let reuse = inst
                        .behaviors
                        .get(&f.name)
                        .filter(|prev| prev.kind.applies_to(f))
                        .cloned();
                    let setting = reuse.unwrap_or_else(|| {
                        let kind = default_kind_for(f);
                        BehaviorSetting {
                            kind,
                            config: kind.default_config(f, schema),
                        }
                    });
                    next.insert(f.name.clone(), setting);
                }
                inst.behaviors = next;
            }
            kept.push(inst);
        }
        self.instances = kept;

        let live: HashSet<String> = self.instances.iter().map(|i| i.id.clone()).collect();
        for nd in &mut self.nodes {
            nd.instance_ids.retain(|id| live.contains(id));
        }
        self.nodes.retain(|nd| !nd.instance_ids.is_empty());
        self.connections
            .retain(|c| live.contains(&c.client_id) && live.contains(&c.server_id));
    }

    /// Snap an instance's `ir_hash` forward to the currently-compiled schema, so
    /// a connection built after a schema edit handshakes cleanly again.
    pub fn resync_instance(&mut self, id: &str) {
        let hash = self.instance(id).and_then(|inst| {
            find_protocol(&self.shape, &inst.schema_ns, &inst.protocol)
                .map(|(schema, _)| schema.ir_hash.clone())
        });
        if let (Some(hash), Some(inst)) = (hash, self.instance_mut(id)) {
            inst.ir_hash = hash;
        }
    }

    /// After loading a session, bump the id counters past everything in it so
    /// freshly-added instances / nodes / connections don't collide.
    pub fn reseed_counters(&mut self) {
        let max_id = |ids: &mut dyn Iterator<Item = &str>, prefix: char| -> u64 {
            ids.filter_map(|id| id.strip_prefix(prefix)?.parse::<u64>().ok())
                .max()
                .unwrap_or(0)
        };
        self.next_instance = self.next_instance.max(max_id(
            &mut self.instances.iter().map(|i| i.id.as_str()),
            'i',
        ));
        self.next_node = self
            .next_node
            .max(max_id(&mut self.nodes.iter().map(|n| n.id.as_str()), 'n'));
        self.next_conn = self.next_conn.max(max_id(
            &mut self.connections.iter().map(|c| c.id.as_str()),
            'c',
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shape::{ArgShape, FnShape, Framing, ProtocolShape, SchemaShape, TypeDef, TypeRef};
    use serde_json::json;

    fn string_ty() -> TypeRef {
        TypeRef::Prim {
            name: "string".into(),
        }
    }

    fn chat_fn(name: &str, index: u32, oneway: bool) -> FnShape {
        FnShape {
            name: name.into(),
            index,
            oneway,
            args: vec![ArgShape {
                name: "text".into(),
                ty: string_ty(),
            }],
            returns: if oneway {
                None
            } else {
                Some(TypeRef::Ref {
                    name: "Message".into(),
                })
            },
            throws: vec![],
        }
    }

    fn message_type() -> TypeDef {
        TypeDef::Struct {
            name: "Message".into(),
            fields: vec![
                crate::shape::FieldShape {
                    name: "body".into(),
                    ty: string_ty(),
                    optional: false,
                },
                crate::shape::FieldShape {
                    name: "seq".into(),
                    ty: TypeRef::Prim { name: "u64".into() },
                    optional: false,
                },
            ],
        }
    }

    fn chat_shape(functions: Vec<FnShape>, ir_hash: &str) -> ProjectShape {
        ProjectShape {
            schemas: vec![SchemaShape {
                namespace: "chat".into(),
                ir_hash: ir_hash.into(),
                protocols: vec![ProtocolShape {
                    name: "Chat".into(),
                    framing: Framing::Datagram,
                    functions,
                }],
                errors: vec![],
                types: vec![message_type()],
            }],
        }
    }

    fn spec(role: Role) -> InstanceSpec {
        InstanceSpec {
            schema_ns: "chat".into(),
            protocol: "Chat".into(),
            role,
        }
    }

    fn base_session() -> Session {
        Session::empty(chat_shape(vec![chat_fn("send", 0, false)], "0xaaaa"))
    }

    #[test]
    fn adding_an_instance_names_it_and_boxes_it() {
        let mut s = base_session();
        let a = s.add_instance(spec(Role::Server), Placement::default());
        let b = s.add_instance(spec(Role::Client), Placement::default());

        assert_eq!(s.instance(&a).unwrap().name, "chat-1");
        assert_eq!(s.instance(&b).unwrap().name, "chat-2");
        assert_eq!(s.nodes.len(), 2, "each got its own box");
        assert_eq!(s.nodes[0].label, "Machine 1");
        assert_eq!(s.instance(&a).unwrap().ir_hash, "0xaaaa");
    }

    #[test]
    fn a_server_instance_gets_seeded_behaviours_a_client_does_not() {
        let mut s = base_session();
        let srv = s.add_instance(spec(Role::Server), Placement::default());
        let cli = s.add_instance(spec(Role::Client), Placement::default());

        assert_eq!(s.instance(&srv).unwrap().behaviors.len(), 1);
        assert_eq!(
            s.instance(&srv).unwrap().behaviors["send"].kind,
            BehaviorKind::Reply
        );
        assert!(s.instance(&cli).unwrap().behaviors.is_empty());
    }

    #[test]
    fn instances_can_share_a_node() {
        let mut s = base_session();
        let a = s.add_instance(spec(Role::Server), Placement::default());
        let node_id = s.instance(&a).unwrap().node_id.clone();
        let b = s.add_instance(
            spec(Role::Client),
            Placement {
                node_id: Some(node_id.clone()),
                ..Placement::default()
            },
        );

        assert_eq!(s.nodes.len(), 1);
        assert_eq!(s.node(&node_id).unwrap().instance_ids, vec![a, b]);
    }

    #[test]
    fn connecting_validates_roles_protocol_and_duplicates() {
        let mut s = base_session();
        let srv = s.add_instance(spec(Role::Server), Placement::default());
        let cli = s.add_instance(spec(Role::Client), Placement::default());

        assert!(s.add_connection(&srv, &cli).is_err(), "server as client");
        let c = s.add_connection(&cli, &srv).unwrap();
        assert_eq!(c, "c1");
        assert!(s.add_connection(&cli, &srv).is_err(), "duplicate");
        assert_eq!(s.connections_for(&srv).len(), 1);
    }

    #[test]
    fn removing_an_instance_takes_its_connections_and_empty_box() {
        let mut s = base_session();
        let srv = s.add_instance(spec(Role::Server), Placement::default());
        let cli = s.add_instance(spec(Role::Client), Placement::default());
        s.add_connection(&cli, &srv).unwrap();

        s.remove_instance(&srv);
        assert!(s.instance(&srv).is_none());
        assert!(s.connections.is_empty());
        assert_eq!(s.nodes.len(), 1, "the server's box went with it");
    }

    #[test]
    fn set_behavior_defaults_config_and_rejects_clients() {
        let mut s = base_session();
        let srv = s.add_instance(spec(Role::Server), Placement::default());
        let cli = s.add_instance(spec(Role::Client), Placement::default());

        s.set_behavior(&srv, "send", BehaviorKind::Echo, None)
            .unwrap();
        assert_eq!(
            s.instance(&srv).unwrap().behaviors["send"].kind,
            BehaviorKind::Echo
        );

        assert!(s
            .set_behavior(&cli, "send", BehaviorKind::Echo, None)
            .is_err());
        assert!(s
            .set_behavior(&srv, "missing", BehaviorKind::Echo, None)
            .is_err());
    }

    #[test]
    fn move_node_clamps_and_rounds() {
        let mut s = base_session();
        s.add_instance(spec(Role::Server), Placement::default());
        let nid = s.nodes[0].id.clone();
        s.move_node(&nid, -20.0, 33.7);
        assert_eq!(
            (s.node(&nid).unwrap().x, s.node(&nid).unwrap().y),
            (0.0, 34.0)
        );
    }

    #[test]
    fn rename_node_ignores_blank() {
        let mut s = base_session();
        s.add_instance(spec(Role::Server), Placement::default());
        let nid = s.nodes[0].id.clone();
        s.rename_node(&nid, "  Gateway  ");
        assert_eq!(s.node(&nid).unwrap().label, "Gateway");
        s.rename_node(&nid, "   ");
        assert_eq!(s.node(&nid).unwrap().label, "Gateway");
    }

    #[test]
    fn rebuild_keeps_surviving_instances_and_prunes_the_rest() {
        let mut s = base_session();
        let srv = s.add_instance(spec(Role::Server), Placement::default());
        let cli = s.add_instance(spec(Role::Client), Placement::default());
        s.add_connection(&cli, &srv).unwrap();
        s.set_behavior(&srv, "send", BehaviorKind::Echo, None)
            .unwrap();

        // recompile: `send` stays, `ping` (one-way) is added, protocol otherwise same
        s.rebuild(chat_shape(
            vec![chat_fn("send", 0, false), chat_fn("ping", 1, true)],
            "0xbbbb",
        ));

        let b = &s.instance(&srv).unwrap().behaviors;
        assert_eq!(
            b["send"].kind,
            BehaviorKind::Echo,
            "kept the edited behaviour"
        );
        assert_eq!(b["ping"].kind, BehaviorKind::Drop, "new one-way fn → drop");
        assert_eq!(
            s.instance(&srv).unwrap().ir_hash,
            "0xaaaa",
            "rebuild leaves ir_hash stale on purpose"
        );
        assert_eq!(s.connections.len(), 1);
    }

    #[test]
    fn rebuild_drops_instances_whose_protocol_vanished() {
        let mut s = base_session();
        let srv = s.add_instance(spec(Role::Server), Placement::default());
        s.add_instance(spec(Role::Client), Placement::default());

        // a shape with no `chat` schema at all
        s.rebuild(ProjectShape { schemas: vec![] });
        assert!(s.instances.is_empty());
        assert!(s.nodes.is_empty(), "emptied boxes are pruned");
        assert!(s.instance(&srv).is_none());
    }

    #[test]
    fn rebuild_resets_a_behaviour_whose_kind_no_longer_applies() {
        let mut s = base_session();
        let srv = s.add_instance(spec(Role::Server), Placement::default());
        // `raise` needs a throw; give `send` one, set raise, then recompile it away
        s.rebuild(chat_shape(
            vec![FnShape {
                throws: vec![crate::shape::ThrowShape {
                    ordinal: 1,
                    name: "Bad".into(),
                }],
                ..chat_fn("send", 0, false)
            }],
            "0xa",
        ));
        s.set_behavior(&srv, "send", BehaviorKind::Raise, None)
            .unwrap();

        s.rebuild(chat_shape(vec![chat_fn("send", 0, false)], "0xb")); // throw gone
        assert_eq!(
            s.instance(&srv).unwrap().behaviors["send"].kind,
            BehaviorKind::Reply,
            "raise no longer applies → back to the default"
        );
    }

    #[test]
    fn resync_snaps_ir_hash_to_the_current_schema() {
        let mut s = base_session();
        let srv = s.add_instance(spec(Role::Server), Placement::default());
        s.rebuild(chat_shape(vec![chat_fn("send", 0, false)], "0xnew"));

        assert_eq!(s.instance(&srv).unwrap().ir_hash, "0xaaaa");
        s.resync_instance(&srv);
        assert_eq!(s.instance(&srv).unwrap().ir_hash, "0xnew");
    }

    #[test]
    fn reseed_counters_lifts_past_loaded_ids() {
        let mut s = base_session();
        s.instances.push(Instance {
            id: "i7".into(),
            name: "chat-1".into(),
            role: Role::Server,
            schema_ns: "chat".into(),
            protocol: "Chat".into(),
            behaviors: BTreeMap::new(),
            ir_hash: "0x0".into(),
            node_id: "n3".into(),
        });
        s.nodes.push(Node {
            id: "n3".into(),
            label: "Machine 3".into(),
            x: 0.0,
            y: 0.0,
            instance_ids: vec!["i7".into()],
        });
        s.reseed_counters();

        let next = s.add_instance(spec(Role::Client), Placement::default());
        assert_eq!(next, "i8", "counter jumped past i7");
        assert_eq!(s.nodes.last().unwrap().id, "n4");
    }

    #[test]
    fn session_serializes_with_camel_case_and_no_shape() {
        let mut s = base_session();
        s.add_instance(spec(Role::Server), Placement::default());
        s.latency_ms = 40.0;

        let json = serde_json::to_value(&s).unwrap();
        assert!(json.get("shape").is_none(), "shape is never in the link");
        assert_eq!(json["latencyMs"], json!(40.0));
        assert_eq!(json["callTimeoutMs"], json!(3000.0));
        assert_eq!(json["instances"][0]["schemaNs"], json!("chat"));
        assert_eq!(json["instances"][0]["irHash"], json!("0xaaaa"));
        assert_eq!(json["clockMode"], json!("real"));

        // round-trips (shape comes back default; the loader re-injects it)
        let back: Session = serde_json::from_value(json).unwrap();
        assert_eq!(back.instances, s.instances);
        assert_eq!(back.latency_ms, 40.0);
    }
}
