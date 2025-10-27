// Copyright 2025 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Backend selection interfaces and algorithms

pub mod adaptive;
pub mod consistent;
pub mod fastest_server;
pub mod fewest_connections;
pub mod fnv_hash;
pub mod random;
pub mod round_robin;
pub mod utils;

use crate::load_balancing::BackendMetrics;

pub use {
    adaptive::Adaptive, consistent::Consistent, fewest_connections::FewestConnections,
    fnv_hash::FNVHash, random::Random, round_robin::RoundRobin,
};

/// Kept around for backwards compatibility until the next breaking change.
#[doc(hidden)]
pub type FVNHash = fnv_hash::FNVHash;

use super::Backend;
use std::collections::BTreeSet;
use std::hash::Hasher;
use std::sync::Arc;
use std::time::Duration;

/// A builder for a backend selector
pub trait Strategy: Send + Sync {
    type BackendSelector: BackendSelection;

    fn build_backend_selector(&self, backends: &BTreeSet<Backend>) -> Self::BackendSelector;

    /// Define metrics that the strategy needs, these will be stored in the Backend struct.
    fn metrics(&self) -> BackendMetrics {
        None
    }

    /// Determines if the BackendSelector should be periodically rebuilt.
    fn rebuild_frequency(&self) -> Option<Duration> {
        None
    }
}

/// [BackendSelection] is the interface to implement backend selection mechanisms.
pub trait BackendSelection: Send + Sync {
    /// The [BackendIter] returned from iter() below.
    type Iter: BackendIter;

    /// Select backends for a given key.
    ///
    /// An [BackendIter] should be returned. The first item in the iter is the first
    /// choice backend. The user should continue to iterate over it if the first backend
    /// cannot be used due to its health or other reasons.
    fn iter(self: &Arc<Self>, key: &[u8]) -> Self::Iter;
}

/// An iterator to find the suitable backend
///
/// Similar to [Iterator] but allow self referencing.
pub trait BackendIter {
    /// Return `Some(&Backend)` when there are more backends left to choose from.
    fn next(&mut self) -> Option<&Backend>;
}

/// [SelectionAlgorithm] is the interface to implement selection algorithms.
///
/// All [std::hash::Hasher] + [Default] can be used directly as a selection algorithm.
pub trait SelectionAlgorithm {
    /// Create a new implementation
    fn new() -> Self;
    /// Return the next index of backend. The caller should perform modulo to get
    /// the valid index of the backend.
    fn next(&self, key: &[u8]) -> u64;
}

impl<H> SelectionAlgorithm for H
where
    H: Default + Hasher,
{
    fn new() -> Self {
        H::default()
    }
    fn next(&self, key: &[u8]) -> u64 {
        let mut hasher = H::default();
        hasher.write(key);
        hasher.finish()
    }
}
