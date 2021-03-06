// Copyright 2018 The Exonum Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Transaction definitions for the configuration service.

use exonum::blockchain::{ExecutionResult, Schema as CoreSchema, StoredConfiguration, Transaction};
use exonum::crypto::{CryptoHash, Hash, PublicKey};
use exonum::encoding::Error as EncodingError;
use exonum::messages::{Message, RawTransaction};
use exonum::node::State;
use exonum::storage::{Fork, Snapshot};

use errors::Error as ServiceError;
use schema::{MaybeVote, ProposeData, Schema};

transactions! {
    Any {
        const SERVICE_ID = super::SERVICE_ID;

        /// Propose a new configuration.
        ///
        /// # Notes
        ///
        /// See [`ErrorCode`] for the description of error codes emitted by the `execute()`
        /// method.
        ///
        /// [`ErrorCode`]: enum.ErrorCode.html
        struct Propose {
            /// Sender of the transaction.
            ///
            /// Should be one of validators as per the active configuration.
            from: &PublicKey,

            /// Configuration in JSON format.
            ///
            /// Should be convertible into `StoredConfiguration`.
            cfg: &str,
        }

        /// Vote for the new configuration.
        ///
        /// # Notes
        ///
        /// The stored version of the transaction has a special variant corresponding to absence
        /// of a vote. See [`MaybeVote`] for details.
        ///
        /// See [`ErrorCode`] for the description of error codes emitted by the `execute()`
        /// method.
        ///
        /// [`MaybeVote`]: struct.MaybeVote.html
        /// [`ErrorCode`]: enum.ErrorCode.html
        struct Vote {
            /// Sender of the transaction.
            ///
            /// Should be one of validators as per the active configuration.
            from: &PublicKey,

            /// Hash of the configuration that this vote is for.
            ///
            /// See [crate docs](index.html) for more details on how the hash is calculated.
            cfg_hash: &Hash,
        }
    }
}

/// Checks if a specified key belongs to one of the current validators.
///
/// # Return value
///
/// The index of the validator authoring the transaction, or `None` if no validator matches
/// the supplied public key.
fn validator_index(snapshot: &Snapshot, key: &PublicKey) -> Option<usize> {
    let actual_config = CoreSchema::new(snapshot).actual_configuration();
    let keys = actual_config.validator_keys;
    keys.iter().position(|k| k.service_key == *key)
}

/// Checks if there is enough votes for a particular configuration hash.
fn enough_votes_to_commit(snapshot: &Snapshot, cfg_hash: &Hash) -> bool {
    let actual_config = CoreSchema::new(snapshot).actual_configuration();

    let schema = Schema::new(snapshot);
    let votes = schema.votes_by_config_hash(cfg_hash);
    let votes_count = votes.iter().filter(|vote| vote.is_some()).count();
    votes_count >= State::byzantine_majority_count(actual_config.validator_keys.len())
}

impl Propose {
    /// Performs context-dependent checks on the proposal.
    ///
    /// # Return value
    ///
    /// Configuration parsed from the transaction together with its hash.
    fn precheck(&self, snapshot: &Snapshot) -> Result<(StoredConfiguration, Hash), ServiceError> {
        use exonum::storage::StorageValue;
        use self::ServiceError::*;

        let following_config = CoreSchema::new(snapshot).following_configuration();
        if let Some(following) = following_config {
            Err(AlreadyScheduled(following))?;
        }
        if validator_index(snapshot, self.from()).is_none() {
            Err(UnknownSender)?;
        }

        let config_candidate = StoredConfiguration::try_deserialize(self.cfg().as_bytes())
            .map_err(InvalidConfig)?;
        self.check_config_candidate(&config_candidate, snapshot)?;

        let cfg = StoredConfiguration::from_bytes(self.cfg().as_bytes().into());
        let cfg_hash = CryptoHash::hash(&cfg);
        if let Some(old_propose) = Schema::new(snapshot).propose(&cfg_hash) {
            Err(AlreadyProposed(old_propose))?;
        }

        Ok((cfg, cfg_hash))
    }

    /// Checks the consistency of a candidate next configuration.
    fn check_config_candidate(
        &self,
        candidate: &StoredConfiguration,
        snapshot: &Snapshot,
    ) -> Result<(), ServiceError> {
        use self::ServiceError::*;

        let actual_config = CoreSchema::new(snapshot).actual_configuration();
        if candidate.previous_cfg_hash != actual_config.hash() {
            return Err(InvalidConfigRef(actual_config));
        }

        let current_height = CoreSchema::new(snapshot).height().next();
        if candidate.actual_from <= current_height {
            return Err(ActivationInPast(current_height));
        }

        Ok(())
    }

    /// Saves this proposal to the service schema.
    fn save(&self, fork: &mut Fork, cfg: &StoredConfiguration, cfg_hash: Hash) {
        let prev_cfg = CoreSchema::new(fork.as_ref())
            .configs()
            .get(&cfg.previous_cfg_hash)
            .unwrap();

        // Start writing to storage.
        // NB. DO NOT write to the service schema anywhere else during `Propose::execute`, it may
        // break invariants.
        let mut schema = Schema::new(fork);

        let propose_data = {
            let mut votes_table = schema.votes_by_config_hash_mut(&cfg_hash);
            debug_assert!(votes_table.is_empty());

            let num_validators = prev_cfg.validator_keys.len();
            for _ in 0..num_validators {
                votes_table.push(MaybeVote::none());
            }

            ProposeData::new(
                self.clone(),
                &votes_table.root_hash(),
                num_validators as u64,
            )
        };

        {
            let mut propose_data_table = schema.propose_data_by_config_hash_mut();
            debug_assert!(propose_data_table.get(&cfg_hash).is_none());
            propose_data_table.put(&cfg_hash, propose_data);
        }

        schema.config_hash_by_ordinal_mut().push(cfg_hash);
    }
}

impl Transaction for Propose {
    fn verify(&self) -> bool {
        self.verify_signature(self.from())
    }

    fn execute(&self, fork: &mut Fork) -> ExecutionResult {
        let (cfg, cfg_hash) = self.precheck(fork.as_ref()).map_err(|err| {
            error!("Discarding propose {:?}: {}", self, err);
            err
        })?;

        self.save(fork, &cfg, cfg_hash);
        trace!("Put propose {:?} to config_proposes table", self);
        Ok(())
    }
}

impl Vote {
    /// Checks context-dependent conditions for a `Vote` transaction.
    ///
    /// # Return value
    ///
    /// Returns a configuration this vote is for on success, or an error (if any).
    fn precheck(&self, snapshot: &Snapshot) -> Result<StoredConfiguration, ServiceError> {
        use self::ServiceError::*;

        let following_config = CoreSchema::new(snapshot).following_configuration();
        if let Some(following) = following_config {
            Err(AlreadyScheduled(following))?;
        }

        if let Some(validator_id) = validator_index(snapshot, self.from()) {
            let vote = Schema::new(snapshot)
                .votes_by_config_hash(self.cfg_hash())
                .get(validator_id as u64)
                .unwrap();
            if vote.is_some() {
                Err(AlreadyVoted)?;
            }
        } else {
            Err(UnknownSender)?;
        }

        let propose = Schema::new(snapshot).propose(self.cfg_hash()).ok_or_else(
            || {
                UnknownConfigRef(*self.cfg_hash())
            },
        )?;

        let parsed = StoredConfiguration::try_deserialize(propose.cfg().as_bytes()).unwrap();
        propose.check_config_candidate(&parsed, snapshot)?;
        Ok(parsed)
    }

    /// Saves this vote into the service schema.
    fn save(&self, fork: &mut Fork) {
        use exonum::storage::StorageValue;

        let cfg_hash = self.cfg_hash();
        let propose_data: ProposeData = Schema::new(fork.as_ref())
            .propose_data_by_config_hash()
            .get(cfg_hash)
            .unwrap();

        let propose = propose_data.tx_propose();
        let prev_cfg_hash = StoredConfiguration::from_bytes(propose.cfg().as_bytes().into())
            .previous_cfg_hash;
        let prev_cfg = CoreSchema::new(fork.as_ref())
            .configs()
            .get(&prev_cfg_hash)
            .unwrap();
        let validator_id = prev_cfg
            .validator_keys
            .iter()
            .position(|pk| pk.service_key == *self.from())
            .unwrap();

        // Start writing to storage.
        // NB. DO NOT write to the service schema anywhere else during `Vote::execute`, it may
        // break invariants.
        let mut schema = Schema::new(fork);

        let propose_data = {
            let mut votes = schema.votes_by_config_hash_mut(cfg_hash);
            votes.set(validator_id as u64, MaybeVote::some(self.clone()));
            propose_data.set_history_hash(&votes.root_hash())
        };

        schema.propose_data_by_config_hash_mut().put(
            cfg_hash,
            propose_data,
        );
    }
}

impl Transaction for Vote {
    fn verify(&self) -> bool {
        self.verify_signature(self.from())
    }

    fn execute(&self, fork: &mut Fork) -> ExecutionResult {
        let parsed_config = self.precheck(fork.as_ref()).map_err(|err| {
            error!("Discarding vote {:?}: {}", self, err);
            err
        })?;

        self.save(fork);
        trace!(
            "Put Vote:{:?} to corresponding cfg votes_by_config_hash table",
            self
        );

        if enough_votes_to_commit(fork.as_ref(), self.cfg_hash()) {
            CoreSchema::new(fork).commit_configuration(parsed_config);
        }
        Ok(())
    }
}

/// Parses a transaction from its raw representation.
pub(crate) fn tx_from_raw(raw: RawTransaction) -> Result<Box<Transaction>, EncodingError> {
    use exonum::blockchain::TransactionSet;

    Any::tx_from_raw(raw).map(Any::into)
}
