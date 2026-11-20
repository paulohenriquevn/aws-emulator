// SPDX-License-Identifier: CC0-1.0

//! fork: whether the security groups of a resource let a TCP connection from a given address
//! arrive, for the services whose endpoints the emulator serves on a socket (an RDS DB
//! instance, an ElastiCache replication group).
//!
//! A security group "controls the traffic that is allowed to reach and leave the resources that
//! it is associated with" (Amazon VPC User Guide, "Security groups"), and admits only what an
//! inbound rule of it names. The source of the connection is identified by its address: the
//! network interface of the account holding that address gives the source's own security groups,
//! which a rule may name, and the address itself is what an address range rule is read against.
//!
//! Nothing is decided from silence. A connection from an address no network interface of the
//! account holds (a client on the emulator's host), a resource with no security group, a group
//! the inventory does not hold and a rule naming a prefix list all leave the question
//! undecided ([`admits_connection`] answers `None`), and the caller lets an undecided
//! connection through.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use crate::ops::types::{IpPermission, NetworkInterface};

/// Where a connection comes from, as the inbound rules of a group can name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionSource {
    /// The address the connection comes from.
    pub address: IpAddr,
    /// The `GroupId` of each security group of the network interface holding that address.
    pub group_ids: Vec<String>,
}

/// The source the account's inventory makes of `address`: `None` when no network interface
/// of the account holds it.
#[must_use]
pub fn connection_source(
    interfaces: &BTreeMap<String, NetworkInterface>,
    address: IpAddr,
) -> Option<ConnectionSource> {
    interfaces.values().find_map(|interface| {
        let holds = interface_addresses(interface)
            .any(|held| held.trim().parse::<IpAddr>().is_ok_and(|held| held == address));
        holds.then(|| ConnectionSource {
            address,
            group_ids: interface
                .groups
                .iter()
                .flatten()
                .filter_map(|group| group.group_id.clone())
                .collect(),
        })
    })
}

fn interface_addresses(interface: &NetworkInterface) -> impl Iterator<Item = &String> {
    interface
        .private_ip_address
        .iter()
        .chain(
            interface
                .private_ip_addresses
                .iter()
                .flatten()
                .filter_map(|address| address.private_ip_address.as_ref()),
        )
        .chain(
            interface
                .ipv6_addresses
                .iter()
                .flatten()
                .filter_map(|address| address.ipv6_address.as_ref()),
        )
}

/// Whether the groups `guarding` names admit a TCP connection to `port` from `source`:
/// `Some(true)` when an inbound rule of one of them names it, `Some(false)` when every rule of
/// every group is known and none does, `None` when the question is undecided.
///
/// `ingress` is the inbound rules of the account's security groups, keyed by `GroupId`.
#[must_use]
pub fn admits_connection(
    guarding: &[String],
    ingress: &BTreeMap<String, Vec<IpPermission>>,
    port: i32,
    source: &ConnectionSource,
) -> Option<bool> {
    if guarding.is_empty() {
        return None;
    }
    let mut undecided = false;
    for group in guarding.iter().map(String::as_str).collect::<BTreeSet<_>>() {
        let Some(permissions) = ingress.get(group) else {
            undecided = true;
            continue;
        };
        for permission in permissions.iter().filter(|permission| covers_tcp_port(permission, port)) {
            match rule_admits(permission, source) {
                Some(true) => return Some(true),
                Some(false) => {}
                None => undecided = true,
            }
        }
    }
    (!undecided).then_some(false)
}

/// Whether the account's EC2 store admits a connection from `address` to `port` of a resource
/// guarded by `guarding`, read in one lock of the store.
#[must_use]
pub fn store_admits_connection(
    store: &super::ComAmazonawsEc2Amazonec2State,
    guarding: &[String],
    address: IpAddr,
    port: i32,
) -> Option<bool> {
    let source = connection_source(&store.network_interfaces, address)?;
    let ingress: BTreeMap<String, Vec<IpPermission>> = guarding
        .iter()
        .filter_map(|id| {
            let group = store.security_groups.get(id)?;
            Some((id.clone(), group.ip_permissions.clone().unwrap_or_default()))
        })
        .collect();
    admits_connection(guarding, &ingress, port, &source)
}

const ALL_PROTOCOLS: &str = "-1";

fn covers_tcp_port(permission: &IpPermission, port: i32) -> bool {
    let protocol = permission
        .ip_protocol
        .as_deref()
        .unwrap_or(ALL_PROTOCOLS)
        .trim()
        .to_ascii_lowercase();
    match protocol.as_str() {
        ALL_PROTOCOLS => true,
        "tcp" | "6" => {
            permission.from_port.is_none_or(|from| from <= port)
                && permission.to_port.is_none_or(|to| port <= to)
        }
        _ => false,
    }
}

fn rule_admits(permission: &IpPermission, source: &ConnectionSource) -> Option<bool> {
    let names_group = permission.user_id_group_pairs.iter().flatten().any(|pair| {
        pair.group_id
            .as_deref()
            .is_some_and(|id| source.group_ids.iter().any(|held| held == id))
    });
    if names_group {
        return Some(true);
    }
    let blocks = permission
        .ip_ranges
        .iter()
        .flatten()
        .filter_map(|range| range.cidr_ip.as_deref())
        .chain(
            permission
                .ipv6_ranges
                .iter()
                .flatten()
                .filter_map(|range| range.cidr_ipv6.as_deref()),
        );
    let mut undecided = false;
    for block in blocks {
        match block_holds(block, source.address) {
            Some(true) => return Some(true),
            Some(false) => {}
            None => undecided = true,
        }
    }
    if permission.prefix_list_ids.iter().flatten().next().is_some() {
        undecided = true;
    }
    (!undecided).then_some(false)
}

/// Whether the CIDR block `cidr` holds `address`; `None` when `cidr` is not a block.
fn block_holds(cidr: &str, address: IpAddr) -> Option<bool> {
    let (network, length) = cidr.trim().split_once('/')?;
    let network: IpAddr = network.parse().ok()?;
    let length: u32 = length.parse().ok()?;
    let (network, address, bits) = match (network, address) {
        (IpAddr::V4(network), IpAddr::V4(address)) => (u128::from(u32::from(network)), u128::from(u32::from(address)), 32),
        (IpAddr::V6(network), IpAddr::V6(address)) => (u128::from(network), u128::from(address), 128),
        // A block of the other address family holds none of this family's addresses.
        _ => return Some(false),
    };
    if length > bits {
        return None;
    }
    let shift = bits - length;
    let mask = if shift >= 128 { 0 } else { u128::MAX << shift };
    Some(network & mask == address & mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::types::{IpRange, UserIdGroupPair};

    fn tcp(port: i32) -> IpPermission {
        IpPermission {
            ip_protocol: Some("tcp".to_string()),
            from_port: Some(port),
            to_port: Some(port),
            ..IpPermission::default()
        }
    }

    fn from_group(port: i32, group: &str) -> IpPermission {
        IpPermission {
            user_id_group_pairs: Some(vec![UserIdGroupPair {
                group_id: Some(group.to_string()),
                ..UserIdGroupPair::default()
            }]),
            ..tcp(port)
        }
    }

    fn from_block(port: i32, cidr: &str) -> IpPermission {
        IpPermission {
            ip_ranges: Some(vec![IpRange {
                cidr_ip: Some(cidr.to_string()),
                ..IpRange::default()
            }]),
            ..tcp(port)
        }
    }

    fn source() -> ConnectionSource {
        ConnectionSource {
            address: "10.0.1.20".parse().unwrap(),
            group_ids: vec!["sg-client".to_string()],
        }
    }

    fn guarded(rules: Vec<IpPermission>) -> (Vec<String>, BTreeMap<String, Vec<IpPermission>>) {
        (vec!["sg-db".to_string()], BTreeMap::from([("sg-db".to_string(), rules)]))
    }

    #[test]
    fn a_rule_naming_the_source_group_on_the_port_admits_the_connection() {
        let (groups, ingress) = guarded(vec![from_group(5432, "sg-client")]);

        assert_eq!(admits_connection(&groups, &ingress, 5432, &source()), Some(true));
    }

    #[test]
    fn a_rule_for_another_port_refuses_the_connection() {
        let (groups, ingress) = guarded(vec![from_group(6379, "sg-client")]);

        assert_eq!(admits_connection(&groups, &ingress, 5432, &source()), Some(false));
    }

    #[test]
    fn a_rule_naming_another_group_refuses_the_connection() {
        let (groups, ingress) = guarded(vec![from_group(5432, "sg-other")]);

        assert_eq!(admits_connection(&groups, &ingress, 5432, &source()), Some(false));
    }

    #[test]
    fn a_block_holding_the_source_address_admits_the_connection() {
        let (groups, ingress) = guarded(vec![from_block(5432, "10.0.0.0/16")]);

        assert_eq!(admits_connection(&groups, &ingress, 5432, &source()), Some(true));
    }

    #[test]
    fn a_block_without_the_source_address_refuses_the_connection() {
        let (groups, ingress) = guarded(vec![from_block(5432, "10.1.0.0/16")]);

        assert_eq!(admits_connection(&groups, &ingress, 5432, &source()), Some(false));
    }

    #[test]
    fn a_group_the_inventory_does_not_hold_decides_nothing() {
        let groups = vec!["sg-missing".to_string()];

        assert_eq!(admits_connection(&groups, &BTreeMap::new(), 5432, &source()), None);
    }

    #[test]
    fn a_resource_with_no_group_decides_nothing() {
        assert_eq!(admits_connection(&[], &BTreeMap::new(), 5432, &source()), None);
    }

    #[test]
    fn an_all_protocols_rule_covers_every_port() {
        let rule = IpPermission {
            ip_protocol: Some("-1".to_string()),
            ..from_group(0, "sg-client")
        };
        let (groups, ingress) = guarded(vec![IpPermission { from_port: None, to_port: None, ..rule }]);

        assert_eq!(admits_connection(&groups, &ingress, 5432, &source()), Some(true));
    }

    #[test]
    fn blocks_are_read_by_prefix_length() {
        let address: IpAddr = "172.29.100.10".parse().unwrap();
        assert_eq!(block_holds("172.29.0.0/16", address), Some(true));
        assert_eq!(block_holds("172.30.0.0/16", address), Some(false));
        assert_eq!(block_holds("0.0.0.0/0", address), Some(true));
        assert_eq!(block_holds("::/0", address), Some(false));
        assert_eq!(block_holds("172.29.0.0/33", address), None);
    }

    #[test]
    fn an_address_no_interface_holds_is_no_source() {
        let interfaces = BTreeMap::from([(
            "eni-1".to_string(),
            NetworkInterface {
                network_interface_id: Some("eni-1".to_string()),
                private_ip_address: Some("10.0.1.20".to_string()),
                ..NetworkInterface::default()
            },
        )]);

        assert!(connection_source(&interfaces, "10.0.1.21".parse().unwrap()).is_none());
        assert_eq!(
            connection_source(&interfaces, "10.0.1.20".parse().unwrap()).map(|source| source.address),
            Some("10.0.1.20".parse().unwrap())
        );
    }
}
