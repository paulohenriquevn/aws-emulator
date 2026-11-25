// SPDX-License-Identifier: CC0-1.0

//! fork: the `cloudfront` module a stored function runs with.
//!
//! A `cloudfront-js-2.0` function reaches the key value store its configuration associates
//! through `cf.kvs()`: "To use this module in the function code, make sure that you have
//! associated a key value store with the function." The store's ID is the one this service
//! minted for it, and its keys are the ones the key value store data plane
//! (`cloudfront-keyvaluestore`) holds under the store's ARN, read LIVE at each call, so a key
//! a caller puts is what the next viewer request reads.

use std::time::SystemTime;

use ::service_com_amazonaws_cloudfrontkeyvaluestore_cloudfrontkeyvaluestore::state as kvs;

use super::{
    AssociatedKeyValueStore, CloudFrontModule, ComAmazonawsCloudfrontCloudfront20200531State,
    KeyValueStoreMeta, KeyValueStoreReader, CLOUDFRONT_MODULE_RUNTIME,
};
use crate::ops::types::FunctionConfig;

/// The module a function of `config` runs with: `None` for a runtime that has no module, and
/// a module with no store for a function that associates none.
///
/// Read under the store's lock; the reader it builds holds only the key value store
/// service's own state handle, so nothing of this store is held while the function runs.
pub fn cloudfront_module_for<Eng: Send + Sync + 'static>(
    engine: &Eng,
    store: &ComAmazonawsCloudfrontCloudfront20200531State,
    config: &FunctionConfig,
) -> Option<CloudFrontModule> {
    if config.runtime != CLOUDFRONT_MODULE_RUNTIME {
        return None;
    }
    let associated = config
        .key_value_store_associations
        .as_ref()
        .and_then(|associations| associations.items.as_deref())
        .and_then(|items| items.first())
        .and_then(|association| store.key_value_store_by_arn(&association.key_value_store_arn));
    let key_value_store = associated.and_then(|record| {
        let Some(state) = kvs::service_state(engine) else {
            ::tracing::warn!(
                key_value_store = %record.arn,
                "the key value store data plane holds no state in this engine, so the function \
                 runs with no store to read"
            );
            return None;
        };
        let reader = DataPlaneReader {
            state,
            arn: record.arn.clone(),
            recorded: record.last_modified_time,
        };
        Some(AssociatedKeyValueStore {
            id: record.id.clone(),
            reader: ::std::sync::Arc::new(reader),
        })
    });
    Some(CloudFrontModule { key_value_store })
}

/// One associated store, read from the key value store data plane's state at each call.
struct DataPlaneReader {
    state: ::engine::ServiceState<kvs::ComAmazonawsCloudfrontkeyvaluestoreCloudfrontkeyvaluestoreState>,
    arn: String,
    /// When this service last changed the store's record, which is what `meta()` reports for
    /// a store the data plane has never been written through.
    recorded: SystemTime,
}

impl KeyValueStoreReader for DataPlaneReader {
    fn value(&self, key: &str) -> Option<String> {
        let state = self.state.lock();
        let held = state.store(&self.arn).ok().flatten()?;
        held.keys.get(key).cloned()
    }

    fn meta(&self) -> KeyValueStoreMeta {
        let state = self.state.lock();
        match state.store(&self.arn).ok().flatten() {
            Some(held) => KeyValueStoreMeta {
                creation: held.created,
                last_updated: held.last_modified,
                key_count: held.keys.len() as u64,
            },
            None => KeyValueStoreMeta {
                creation: self.recorded,
                last_updated: self.recorded,
                key_count: 0,
            },
        }
    }
}
