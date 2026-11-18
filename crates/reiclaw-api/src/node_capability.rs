//! Kernel-owned Node capability definitions. Registration never grants access.

use serde_json::{Map, Value, json};

/// Side-effect class owned by the kernel, never supplied by a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRisk {
    ReadOnly,
    /// Effects remain unavailable until a separate grant admission path exists.
    Effect,
}

/// A capability's public contract and its authoritative argument validator.
pub struct NodeCapability {
    pub name: &'static str,
    pub description: &'static str,
    pub risk: NodeRisk,
    schema: fn() -> Value,
    validate: fn(&Map<String, Value>) -> bool,
}

impl NodeCapability {
    pub fn parameters_schema(&self) -> Value {
        (self.schema)()
    }

    pub fn validate_args(&self, args: &Value) -> bool {
        args.as_object().is_some_and(self.validate)
    }

    /// Eligibility only; live advertisement and requester grants still apply.
    pub fn invocable(&self) -> bool {
        self.risk == NodeRisk::ReadOnly
    }
}

const CATALOG: &[NodeCapability] = &[
    NodeCapability {
        name: "system.info",
        description: "Read-only info or echo from an explicitly authorized local node.",
        risk: NodeRisk::ReadOnly,
        schema: || json!({"type":"object","properties":{},"additionalProperties":false}),
        validate: Map::is_empty,
    },
    NodeCapability {
        name: "test.echo",
        description: "Read-only info or echo from an explicitly authorized local node.",
        risk: NodeRisk::ReadOnly,
        schema: || json!({"type":"object","properties":{"text":{"type":"string","maxLength":4096}},"required":["text"],"additionalProperties":false}),
        validate: |object| {
            object.len() == 1
                && object
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.len() <= 4096)
        },
    },
];

pub fn catalog() -> &'static [NodeCapability] {
    CATALOG
}

pub fn lookup(name: &str) -> Option<&'static NodeCapability> {
    catalog().iter().find(|capability| capability.name == name)
}

pub fn invocable(name: &str) -> Option<&'static NodeCapability> {
    lookup(name).filter(|capability| capability.invocable())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn catalog_is_unique_and_exposes_only_the_two_retained_read_only_capabilities() {
        let names: HashSet<_> = catalog().iter().map(|entry| entry.name).collect();
        assert_eq!(names.len(), catalog().len());
        assert_eq!(names, HashSet::from(["system.info", "test.echo"]));
        for entry in catalog() {
            assert_eq!(lookup(entry.name).unwrap().name, entry.name);
            assert!(invocable(entry.name).is_some());
            assert_eq!(entry.risk, NodeRisk::ReadOnly);
            let schema = entry.parameters_schema();
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], false);
        }
        assert!(lookup("unregistered.action").is_none());
        assert!(invocable("unregistered.action").is_none());
    }

    #[test]
    fn info_accepts_only_an_empty_object() {
        let capability = lookup("system.info").unwrap();
        assert!(capability.validate_args(&json!({})));
        for args in [
            json!({"approved": true}),
            json!({"text": ""}),
            json!([]),
            Value::Null,
        ] {
            assert!(!capability.validate_args(&args));
        }
    }

    #[test]
    fn echo_preserves_the_byte_limit_and_rejects_extra_authority_fields() {
        let capability = lookup("test.echo").unwrap();
        for text in [
            String::new(),
            "a".repeat(4096),
            format!("{}a", "界".repeat(1365)),
        ] {
            assert!(capability.validate_args(&json!({"text": text})));
        }
        for args in [
            json!({"text": "a".repeat(4097)}),
            json!({"text": "界".repeat(1366)}),
            json!({"text": "", "approved": true}),
            json!({"text": "", "grant_proof": {}}),
            json!({"text": 1}),
            json!({}),
            json!("text"),
            Value::Null,
        ] {
            assert!(!capability.validate_args(&args));
        }
    }

    #[test]
    fn registering_an_effect_does_not_make_it_invocable() {
        let capability = NodeCapability {
            name: "unavailable.effect",
            description: "Effect without an admission path",
            risk: NodeRisk::Effect,
            schema: || json!({"type": "object", "additionalProperties": false}),
            validate: Map::is_empty,
        };
        assert!(capability.validate_args(&json!({})));
        assert!(!capability.invocable());
    }
}
