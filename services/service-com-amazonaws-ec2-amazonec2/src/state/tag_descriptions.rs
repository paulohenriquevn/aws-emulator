// SPDX-License-Identifier: CC0-1.0

//! fork: the tags of every taggable resource, one row per resource, as `DescribeTags`
//! reports them (paulohenriquevn/aws-emulator#16).
//!
//! `CreateTags` and `DeleteTags` reach a resource's tags through
//! [`ComAmazonawsEc2Amazonec2State::resource_tags_mut`], which resolves ONE id against every
//! store that holds a taggable resource. `DescribeTags` asks the opposite question — every
//! tag of every such resource — so [`ComAmazonawsEc2Amazonec2State::tagged_resources`] walks
//! the same stores, expanded from the same list (`taggable_resource_stores!`), and a resource
//! that can be tagged is always one whose tags can be listed.
//!
//! Split out of the [`crate::state`] module root so each file holds one concern, and
//! re-exported from it, so a handler reaches these as `crate::state::<item>` like every
//! other primitive there.

use super::*;

/// One taggable resource as `DescribeTags` reads it: its id, the `ResourceType` a
/// `TagSpecification` names its kind by, and its tags as key -> value pairs.
///
/// The pairs are read the way [`ResourceTagsMut::pairs`] reads them, so the two cannot
/// disagree about what a resource carries: a list entry with no key names no tag and is not
/// reported, an entry with no value reads as the empty value, and the pairs are ordered by
/// key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggedResource<'store> {
    /// The resource's id, the key its store holds it under.
    pub resource_id: &'store str,
    /// The `ResourceType` enum value of the resource's kind (`vpc`, `security-group`, ...).
    pub resource_type: &'static str,
    /// The resource's tags, ordered by key.
    pub tags: BTreeMap<&'store str, &'store str>,
}

impl<'store> TaggedResource<'store> {
    /// A resource whose tags are the modelled optional `Tag` list.
    pub fn from_list(
        resource_id: &'store str,
        resource_type: &'static str,
        tags: &'store [Tag],
    ) -> Self {
        Self {
            resource_id,
            resource_type,
            tags: tags
                .iter()
                .filter_map(|tag| {
                    Some((tag.key.as_deref()?, tag.value.as_deref().unwrap_or_default()))
                })
                .collect(),
        }
    }

    /// A resource whose tags are a key -> value map, the form [`InstanceRecord`] uses.
    pub fn from_map(
        resource_id: &'store str,
        resource_type: &'static str,
        tags: &'store BTreeMap<String, String>,
    ) -> Self {
        Self {
            resource_id,
            resource_type,
            tags: tags
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect(),
        }
    }
}

/// The `ResourceType` of an instance, which [`ComAmazonawsEc2Amazonec2State::resource_tags_mut`]
/// resolves outside the list because its record keeps its tags as a map.
pub const INSTANCE_RESOURCE_TYPE: &str = "instance";

/// The `ResourceType` of an elastic network interface, resolved outside the list because its
/// modelled tag member is `TagSet` rather than `Tags`.
pub const NETWORK_INTERFACE_RESOURCE_TYPE: &str = "network-interface";

impl ComAmazonawsEc2Amazonec2State {
    /// Whether `resource` is a resource of `region`, as far as this store records one.
    ///
    /// EC2 is reached through a regional endpoint, so a describe signed for one Region does
    /// not report what was created in another. The store records that Region for the kinds
    /// below, read the way their own describes read it: a VPC by the Region it was created
    /// in, a resource that belongs to one VPC by its VPC's Region
    /// ([`Self::vpc_is_in_region`]), and an instance by its Availability Zone
    /// ([`instance_region`]).
    ///
    /// Every other taggable kind is held by the ACCOUNT in this store, with no Region on its
    /// record, so it is reported to every Region, which is also how its own describe and
    /// `CreateTags` reach it.
    pub fn tagged_resource_is_in_region(
        &self,
        resource: &TaggedResource<'_>,
        region: &str,
    ) -> bool {
        let id = resource.resource_id;
        match resource.resource_type {
            "vpc" => self.vpc_is_in_region(Some(id), region),
            "subnet" => self.vpc_is_in_region(
                self.subnets.get(id).and_then(|record| record.vpc_id.as_deref()),
                region,
            ),
            "security-group" => self.vpc_is_in_region(
                self.security_groups
                    .get(id)
                    .and_then(|record| record.vpc_id.as_deref()),
                region,
            ),
            "network-acl" => self.vpc_is_in_region(
                self.network_acls.get(id).and_then(|record| record.vpc_id.as_deref()),
                region,
            ),
            "route-table" => self.vpc_is_in_region(
                self.route_tables.get(id).and_then(|record| record.vpc_id.as_deref()),
                region,
            ),
            NETWORK_INTERFACE_RESOURCE_TYPE => self.vpc_is_in_region(
                self.network_interfaces
                    .get(id)
                    .and_then(|record| record.vpc_id.as_deref()),
                region,
            ),
            INSTANCE_RESOURCE_TYPE => self
                .instances
                .get(id)
                .is_some_and(|record| instance_region(record) == region),
            _ => true,
        }
    }
}
