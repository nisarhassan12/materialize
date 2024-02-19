// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::{BTreeMap, BTreeSet};
use std::pin::Pin;

use mysql_async::BinlogStream;
use timely::dataflow::channels::pushers::TeeCore;
use timely::dataflow::operators::{Capability, CapabilitySet};
use timely::progress::Antichain;
use tracing::trace;

use mz_mysql_util::{Config, MySqlTableDesc};
use mz_repr::Row;
use mz_sql_parser::ast::UnresolvedItemName;
use mz_storage_types::sources::mysql::GtidPartition;
use mz_timely_util::builder_async::AsyncOutputHandle;

use super::super::{DefiniteError, RewindRequest};
use crate::source::RawSourceCreationConfig;

/// A container to hold various context information for the replication process, used when
/// processing events from the binlog stream.
pub(super) struct ReplContext<'a> {
    pub(super) config: &'a RawSourceCreationConfig,
    pub(super) connection_config: &'a Config,
    pub(super) stream: Pin<&'a mut futures::stream::Peekable<BinlogStream>>,
    pub(super) table_info: &'a BTreeMap<UnresolvedItemName, (usize, MySqlTableDesc)>,
    pub(super) data_output: &'a mut AsyncOutputHandle<
        GtidPartition,
        Vec<((usize, Result<Row, DefiniteError>), GtidPartition, i64)>,
        TeeCore<GtidPartition, Vec<((usize, Result<Row, DefiniteError>), GtidPartition, i64)>>,
    >,
    pub(super) data_cap_set: &'a mut CapabilitySet<GtidPartition>,
    pub(super) upper_cap_set: &'a mut CapabilitySet<GtidPartition>,
    // Owned values:
    pub(super) rewinds:
        BTreeMap<UnresolvedItemName, ([Capability<GtidPartition>; 2], RewindRequest)>,
    // Binlog Table Id -> Table Name (its key in the `table_info` map)
    pub(super) table_id_map: BTreeMap<u64, UnresolvedItemName>,
    pub(super) skipped_table_ids: BTreeSet<u64>,
    pub(super) errored_tables: BTreeSet<UnresolvedItemName>,
}

impl<'a> ReplContext<'a> {
    pub(super) fn new(
        config: &'a RawSourceCreationConfig,
        connection_config: &'a Config,
        stream: Pin<&'a mut futures::stream::Peekable<BinlogStream>>,
        table_info: &'a BTreeMap<UnresolvedItemName, (usize, MySqlTableDesc)>,
        data_output: &'a mut AsyncOutputHandle<
            GtidPartition,
            Vec<((usize, Result<Row, DefiniteError>), GtidPartition, i64)>,
            TeeCore<GtidPartition, Vec<((usize, Result<Row, DefiniteError>), GtidPartition, i64)>>,
        >,
        data_cap_set: &'a mut CapabilitySet<GtidPartition>,
        upper_cap_set: &'a mut CapabilitySet<GtidPartition>,
        rewinds: BTreeMap<UnresolvedItemName, ([Capability<GtidPartition>; 2], RewindRequest)>,
    ) -> Self {
        Self {
            config,
            connection_config,
            stream,
            table_info,
            data_output,
            data_cap_set,
            upper_cap_set,
            rewinds,
            table_id_map: BTreeMap::new(),
            skipped_table_ids: BTreeSet::new(),
            errored_tables: BTreeSet::new(),
        }
    }

    /// Advances the frontier of the data and upper capability sets to `new_upper`,
    /// and drops any existing rewind requests that are no longer applicable.
    pub(super) fn advance(&mut self, new_upper: Antichain<GtidPartition>) {
        let (id, worker_id) = (self.config.id, self.config.worker_id);

        trace!(%id, "timely-{worker_id} advancing frontier to {new_upper:?}");

        self.data_cap_set.downgrade(&*new_upper);
        self.upper_cap_set.downgrade(&*new_upper);

        self.rewinds.retain(|_, (_, req)| {
            // We need to retain the rewind requests whose snapshot_upper
            // has at least one timestamp such that new_upper is less than
            // that timestamp
            let res = req.snapshot_upper.iter().any(|ts| new_upper.less_than(ts));
            if !res {
                trace!(%id, "timely-{worker_id} removing rewind request {req:?}");
            }
            res
        });
    }
}