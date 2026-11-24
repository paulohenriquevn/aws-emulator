// SPDX-License-Identifier: CC0-1.0

//! The store writes a PUSH makes, shared by the two surfaces a push arrives on.
//!
//! A client pushes an image either through the control-plane API (`InitiateLayerUpload`,
//! `UploadLayerPart`, `CompleteLayerUpload`, `PutImage`) or through the Docker Registry v2
//! endpoint `docker push` talks to (`service/registry_v2.rs`). Both are the same push into
//! the same repository, so both go through the functions here: an upload session is one
//! [`LayerUpload`] record whichever surface opened it, a completed blob is one
//! [`LayerBlob`] in [`Repo::layer_blobs`], and a stored manifest is one [`ImageRecord`]
//! written by [`store_pushed_image`](ComAmazonawsEcrAmazonec2containerregistryV20150921State::store_pushed_image).
//! What differs between the two surfaces is only how each one words a refusal, which is why
//! every refusal here is a typed reason and never a rendered error.

use std::time::SystemTime;

use super::{
    manifest_media_type, mint_hex, ComAmazonawsEcrAmazonec2containerregistryV20150921State,
    Fault, ImageRecord, LayerBlob, LayerUpload, Repo, MAX_IMAGES_PER_REPOSITORY,
    MAX_LAYERS_PER_REPOSITORY, MAX_LAYER_UPLOADS,
    MAX_TAGS_PER_REPOSITORY, REPORTED_LAYER_PART_SIZE,
};

/// A fresh `UploadId`: 32 hex digits in the `8-4-4-4-12` layout the model's `UploadId`
/// pattern requires
/// (`^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$`).
///
/// The digits are the service's one mint, [`mint_hex`] — nothing in them is derived from
/// the request — laid out in the five groups the pattern requires. A hex id of 16 bytes is
/// 32 digits, which is exactly what the layout consumes, so the slices cannot be short.
pub fn new_upload_id() -> String {
    const UPLOAD_ID_BYTES: usize = 16;

    let hex = mint_hex(UPLOAD_ID_BYTES);
    let group = |from: usize, to: usize| hex.get(from..to).unwrap_or_default();

    format!(
        "{}-{}-{}-{}-{}",
        group(0, 8),
        group(8, 12),
        group(12, 16),
        group(16, 20),
        group(20, 32)
    )
}

/// The registry already holds [`MAX_LAYER_UPLOADS`] in-flight uploads and none of them is
/// old enough to reclaim, so no new one is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadCapReached;

/// The repository already holds [`MAX_LAYERS_PER_REPOSITORY`] blobs, so no new one is
/// registered on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerCeilingReached;

/// What registering a blob on a repository did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobRegistration {
    /// The blob is new to the repository and is now held by it.
    Registered,
    /// The repository already held a blob under that digest, so nothing was written.
    AlreadyHeld,
}

impl Repo {
    /// Register `content` on this repository under `digest`, which the caller computed over
    /// exactly these bytes ([`super::content_digest`]).
    ///
    /// A blob the repository already holds is left as it is ([`BlobRegistration::AlreadyHeld`]);
    /// past [`MAX_LAYERS_PER_REPOSITORY`] a new one is refused. What the bytes are a
    /// filesystem OF is read once here, while they are in hand, so a later basic image scan
    /// does not walk the archive again. The media type stays unset: a blob's media type is
    /// what a manifest referencing it declares.
    pub fn register_layer_blob(
        &mut self,
        digest: &str,
        content: Vec<u8>,
    ) -> Result<BlobRegistration, LayerCeilingReached> {
        if self.layer_blobs.contains_key(digest) {
            return Ok(BlobRegistration::AlreadyHeld);
        }
        if self.layer_blobs.len() >= MAX_LAYERS_PER_REPOSITORY {
            return Err(LayerCeilingReached);
        }
        let size = i64::try_from(content.len()).unwrap_or(i64::MAX);
        let filesystem = super::scan_layer_filesystem(&content);
        self.layer_blobs.insert(
            digest.to_string(),
            LayerBlob {
                digest: digest.to_string(),
                size: Some(size),
                media_type: None,
                filesystem,
                content,
            },
        );
        Ok(BlobRegistration::Registered)
    }
}

/// One manifest a client pushes into a repository.
#[derive(Debug, Clone, Copy)]
pub struct ImagePush<'a> {
    /// The repository the manifest is pushed into.
    pub repository_name: &'a str,
    /// The manifest document, byte for byte as the client sent it.
    pub manifest: &'a str,
    /// The media type the client named for it, if it named one; otherwise the one the
    /// document states about itself is recorded.
    pub media_type: Option<&'a str>,
    /// The tag the push attaches, or `None` for a push by digest.
    pub tag: Option<&'a str>,
    /// The manifest's content digest ([`super::manifest_digest`]).
    pub digest: &'a str,
}

/// Why a manifest push stored nothing. Each surface renders these in its own envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImagePushRefusal {
    /// No repository of that name; carries the fault `PutImage` renders.
    RepositoryNotFound(String),
    /// Storing the manifest would take the registry past
    /// [`super::max_registry_caller_bytes`].
    ByteBudgetExhausted,
    /// The manifest references blobs the repository does not hold, named in order.
    LayersNotFound {
        missing: Vec<String>,
        repository_name: String,
        registry_id: String,
    },
    /// The very same manifest is already in the repository under the very same tag (or,
    /// for an untagged push, at all): there is nothing to push.
    ImageAlreadyExists {
        digest: String,
        tag: Option<String>,
        repository_name: String,
        registry_id: String,
    },
    /// The tag names another manifest and the repository does not let it be overwritten.
    TagImmutable { tag: String, repository_name: String },
    /// The repository already holds [`MAX_IMAGES_PER_REPOSITORY`] images.
    ImageCeilingReached { repository_name: String },
    /// The repository already holds [`MAX_TAGS_PER_REPOSITORY`] tags.
    TagCeilingReached { repository_name: String },
    /// The record was not found after it was written: a broken invariant, never a caller's
    /// fault.
    NotStored { digest: String },
}

/// What a successful push stored, read back from the stored record.
#[derive(Debug, Clone)]
pub struct StoredPush {
    pub registry_id: String,
    pub repository_name: String,
    pub digest: String,
    pub manifest: String,
    pub manifest_media_type: Option<String>,
    pub manifest_size: i64,
    /// The image action the push performed, for the caller to publish once the store's
    /// lock is released.
    pub action: super::image_events::ImageAction,
}

impl ComAmazonawsEcrAmazonec2containerregistryV20150921State {
    /// Open an in-flight layer upload against `repository_name` (which the caller has
    /// already resolved) and answer its id.
    ///
    /// An upload a client opened and abandoned otherwise holds its slot for the life of the
    /// process, so abandoned ones are reclaimed when — and only when — the registry is full.
    pub fn open_layer_upload(
        &mut self,
        repository_name: &str,
        now: SystemTime,
    ) -> Result<String, UploadCapReached> {
        if self.uploads.len() >= MAX_LAYER_UPLOADS {
            self.reclaim_abandoned_uploads(now);
        }
        if self.uploads.len() >= MAX_LAYER_UPLOADS {
            return Err(UploadCapReached);
        }
        let upload_id = new_upload_id();
        self.uploads.insert(
            upload_id.clone(),
            LayerUpload {
                repository_name: repository_name.to_string(),
                part_size: REPORTED_LAYER_PART_SIZE,
                started_at: now,
                received_bytes: Vec::new(),
            },
        );
        Ok(upload_id)
    }

    /// Store a pushed manifest as an image of its repository — the one write `PutImage` and
    /// a registry manifest `PUT` both make.
    ///
    /// The manifest is only stored once every blob it references is in the repository, the
    /// repository's tag mutability is honoured, and the registry's byte budget and the
    /// repository's caps are respected; a refusal stores nothing. The manifest is stored once
    /// per digest, so re-pushing the same content under a new tag keeps the record (and the
    /// instant it was first pushed). The replications and signing jobs a push starts are
    /// set on the record that STORES the image, because both act on an image as it arrives.
    pub fn store_pushed_image(
        &mut self,
        push: ImagePush<'_>,
        now: SystemTime,
        planned_signings: Vec<super::signing::ImageSigning>,
    ) -> Result<StoredPush, ImagePushRefusal> {
        let digest = push.digest;
        // A manifest the repository already holds costs nothing more against the budget.
        let manifest_bytes = {
            let repo = self
                .repository(push.repository_name)
                .map_err(repository_not_found)?;
            if repo.images.contains_key(digest) {
                0
            } else {
                push.manifest.len()
            }
        };
        if !self.has_byte_budget_for(manifest_bytes) {
            return Err(ImagePushRefusal::ByteBudgetExhausted);
        }

        let planned_replications = self.planned_replications(push.repository_name, now);
        let repo = self
            .repository_mut(push.repository_name)
            .map_err(repository_not_found)?;

        let missing = repo.missing_layers(push.manifest);
        if !missing.is_empty() {
            return Err(ImagePushRefusal::LayersNotFound {
                missing,
                repository_name: repo.name.clone(),
                registry_id: repo.registry_id.clone(),
            });
        }

        match push.tag {
            Some(tag) if repo.digest_of_tag(tag).map(String::as_str) == Some(digest) => {
                return Err(ImagePushRefusal::ImageAlreadyExists {
                    digest: digest.to_string(),
                    tag: Some(tag.to_string()),
                    repository_name: repo.name.clone(),
                    registry_id: repo.registry_id.clone(),
                });
            }
            Some(tag) if repo.digest_of_tag(tag).is_some() && !repo.tag_is_mutable(tag) => {
                return Err(ImagePushRefusal::TagImmutable {
                    tag: tag.to_string(),
                    repository_name: repo.name.clone(),
                });
            }
            None if repo.images.contains_key(digest) => {
                return Err(ImagePushRefusal::ImageAlreadyExists {
                    digest: digest.to_string(),
                    tag: None,
                    repository_name: repo.name.clone(),
                    registry_id: repo.registry_id.clone(),
                });
            }
            _ => {}
        }

        if !repo.images.contains_key(digest) && repo.images.len() >= MAX_IMAGES_PER_REPOSITORY {
            return Err(ImagePushRefusal::ImageCeilingReached {
                repository_name: repo.name.clone(),
            });
        }
        let new_tag = push
            .tag
            .is_some_and(|tag| repo.digest_of_tag(tag).is_none());
        if new_tag && repo.tag_index.len() >= MAX_TAGS_PER_REPOSITORY {
            return Err(ImagePushRefusal::TagCeilingReached {
                repository_name: repo.name.clone(),
            });
        }

        repo.images.entry(digest.to_string()).or_insert_with(|| {
            let mut stored = ImageRecord::pushed(
                digest.to_string(),
                push.manifest.to_string(),
                push.media_type
                    .map(ToString::to_string)
                    .or_else(|| manifest_media_type(push.manifest)),
                now,
            );
            stored.replications = planned_replications;
            stored.signings = planned_signings;
            stored
        });

        // Attaching the tag detaches it from whatever held it.
        if let Some(tag) = push.tag {
            repo.move_tag(tag, digest);
        }

        // A manifest list's size is a statement about the images stored beside it, so every
        // image's reported size is recomputed whenever the image set changes.
        repo.refresh_image_sizes();

        let Some(stored) = repo.images.get(digest) else {
            return Err(ImagePushRefusal::NotStored {
                digest: digest.to_string(),
            });
        };
        Ok(StoredPush {
            registry_id: repo.registry_id.clone(),
            repository_name: repo.name.clone(),
            digest: stored.digest.clone(),
            manifest: stored.manifest.clone(),
            manifest_media_type: stored.manifest_media_type.clone(),
            manifest_size: stored.manifest_size(),
            action: super::image_events::ImageAction {
                repository_name: repo.name.clone(),
                action_type: super::image_events::ACTION_TYPE_PUSH,
                image_digest: digest.to_string(),
                image_tag: push.tag.map(ToString::to_string),
                at: now,
            },
        })
    }
}

/// The refusal a repository lookup that failed becomes.
fn repository_not_found(fault: Fault) -> ImagePushRefusal {
    ImagePushRefusal::RepositoryNotFound(fault.message().to_string())
}

/// The `RCTAppliedFor` value that makes a repository creation template create a
/// repository a push names and the registry does not hold.
pub const CREATE_ON_PUSH: &str = "CREATE_ON_PUSH";

/// The template prefix that applies to every repository: "ROOT" is the catch-all a more
/// specific prefix wins over.
pub const ROOT_TEMPLATE_PREFIX: &str = "ROOT";

/// Why a push to a repository the registry does not hold created none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateOnPushRefusal {
    /// No repository creation template applied for `CREATE_ON_PUSH` matches the name: "When
    /// there isn't a repository creation template that matches the target repository for an
    /// image push, Amazon ECR will not create a repository with default settings."
    NoMatchingTemplate,
    /// The registry already holds [`super::MAX_REPOSITORIES`] repositories.
    RepositoryCeilingReached,
}

/// Whether the template stored under the normalized `prefix` applies to `repository_name`:
/// `ROOT` applies to every repository, and any other prefix to the repositories whose name
/// begins with it followed by `/` ("a trailing `/` is assumed").
fn template_prefix_matches(prefix: &str, repository_name: &str) -> bool {
    prefix == ROOT_TEMPLATE_PREFIX
        || repository_name
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// How specific a matching prefix is: `ROOT` is the least specific of all, and otherwise a
/// longer prefix names a narrower namespace.
fn template_specificity(prefix: &str) -> usize {
    if prefix == ROOT_TEMPLATE_PREFIX {
        0
    } else {
        prefix.len().saturating_add(1)
    }
}

impl ComAmazonawsEcrAmazonec2containerregistryV20150921State {
    /// The repository creation template a push to `repository_name` creates the repository
    /// from: the most specific one whose prefix matches the name and whose `appliedFor`
    /// includes [`CREATE_ON_PUSH`], or `None` when no such template exists.
    pub fn create_on_push_template(&self, repository_name: &str) -> Option<&super::CreationTemplate> {
        self.creation_templates
            .iter()
            .filter(|(prefix, template)| {
                template.applied_for.iter().any(|use_| use_ == CREATE_ON_PUSH)
                    && template_prefix_matches(prefix, repository_name)
            })
            .max_by_key(|(prefix, _)| template_specificity(prefix))
            .map(|(_, template)| template)
    }

    /// Create the repository a push names and this registry does not hold, from the
    /// [`create_on_push_template`](Self::create_on_push_template) that matches it.
    ///
    /// The template's settings — tag mutability and its exclusion filters, encryption,
    /// repository policy, lifecycle policy and resource tags — are applied once, here, at
    /// creation: a later change to the template changes no repository it already created.
    /// `region` is the Region the repository ARN names.
    pub fn create_repository_on_push(
        &mut self,
        repository_name: &str,
        region: &str,
        now: SystemTime,
    ) -> Result<(), CreateOnPushRefusal> {
        let Some(template) = self.create_on_push_template(repository_name).cloned() else {
            return Err(CreateOnPushRefusal::NoMatchingTemplate);
        };
        if self.repositories.len() >= super::MAX_REPOSITORIES {
            return Err(CreateOnPushRefusal::RepositoryCeilingReached);
        }
        let mut repo = Repo {
            arn: super::repository_arn(region, repository_name),
            registry_id: super::registry_id().to_string(),
            name: repository_name.to_string(),
            created_at: now,
            image_tag_mutability: template.image_tag_mutability,
            image_tag_mutability_exclusion_filters: template.image_tag_mutability_exclusion_filters,
            // A template carries no scanning setting, so the repository scans nothing on
            // push, as one created without a scanning configuration does.
            image_scanning_config: super::ImageScanningConfiguration::default(),
            encryption_config: super::EncryptionConfiguration {
                encryption_type: template.encryption_config.encryption_type,
                kms_key: template.encryption_config.kms_key,
            },
            tags: Vec::new(),
            policy_text: template
                .repository_policy
                .as_deref()
                .map(super::canonical_policy_document),
            images: std::collections::BTreeMap::new(),
            tag_index: std::collections::BTreeMap::new(),
            layer_blobs: std::collections::BTreeMap::new(),
            lifecycle_policy: template.lifecycle_policy.as_deref().map(|text| {
                super::StoredLifecyclePolicy {
                    text: super::normalize_lifecycle_policy(text),
                    last_evaluated_at: now,
                }
            }),
            lifecycle_preview: None,
        };
        repo.merge_tags(template.resource_tags);
        self.repositories.insert(repository_name.to_string(), repo);
        Ok(())
    }
}
