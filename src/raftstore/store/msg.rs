// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::boxed::FnBox;
use std::fmt;
use std::time::Instant;

use kvproto::import_sstpb::SSTMeta;
use kvproto::metapb;
use kvproto::metapb::RegionEpoch;
use kvproto::pdpb::CheckPolicy;
use kvproto::raft_cmdpb::{RaftCmdRequest, RaftCmdResponse};
use kvproto::raft_serverpb::RaftMessage;

use raft::SnapshotStatus;
use util::escape;
use util::rocksdb::CompactedEvent;

use super::{Peer, RegionSnapshot};
use raftstore::store::worker::apply::ApplyRes;

#[derive(Debug, Clone)]
pub struct ReadResponse {
    pub response: RaftCmdResponse,
    pub snapshot: Option<RegionSnapshot>,
}

#[derive(Debug)]
pub struct WriteResponse {
    pub response: RaftCmdResponse,
}

#[derive(Debug)]
pub enum SeekRegionResult {
    Found {
        local_peer: metapb::Peer,
        region: metapb::Region,
    },
    LimitExceeded {
        next_key: Vec<u8>,
    },
    Ended,
}

pub type ReadCallback = Box<FnBox(ReadResponse) + Send>;
pub type WriteCallback = Box<FnBox(WriteResponse) + Send>;
pub type BatchReadCallback = Box<FnBox(Vec<Option<ReadResponse>>) + Send>;

pub type SeekRegionCallback = Box<FnBox(SeekRegionResult) + Send>;
pub type SeekRegionFilter = Box<Fn(&Peer) -> bool + Send>;

/// Variants of callbacks for `Msg`.
///  - `Read`: a callbak for read only requests including `StatusRequest`,
///         `GetRequest` and `SnapRequest`
///  - `Write`: a callback for write only requests including `AdminRequest`
///          `PutRequest`, `DeleteRequest` and `DeleteRangeRequest`.
///  - `BatchRead`: callbacks for a batch read request.
pub enum Callback {
    /// No callback.
    None,
    /// Read callback.
    Read(ReadCallback),
    /// Write callback.
    Write(WriteCallback),
    /// Batch read callbacks.
    BatchRead(BatchReadCallback),
}

impl Callback {
    pub fn invoke_with_response(self, resp: RaftCmdResponse) {
        match self {
            Callback::None => (),
            Callback::Read(read) => {
                let resp = ReadResponse {
                    response: resp,
                    snapshot: None,
                };
                read(resp);
            }
            Callback::Write(write) => {
                let resp = WriteResponse { response: resp };
                write(resp);
            }
            Callback::BatchRead(_) => unreachable!(),
        }
    }

    pub fn invoke_read(self, args: ReadResponse) {
        match self {
            Callback::Read(read) => read(args),
            other => panic!("expect Callback::Read(..), got {:?}", other),
        }
    }

    pub fn invoke_batch_read(self, args: Vec<Option<ReadResponse>>) {
        match self {
            Callback::BatchRead(batch_read) => batch_read(args),
            other => panic!("expect Callback::BatchRead(..), got {:?}", other),
        }
    }
}

impl fmt::Debug for Callback {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Callback::None => write!(fmt, "Callback::None"),
            Callback::Read(_) => write!(fmt, "Callback::Read(..)"),
            Callback::Write(_) => write!(fmt, "Callback::Write(..)"),
            Callback::BatchRead(_) => write!(fmt, "Callback::BatchRead(..)"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum PeerTick {
    Raft,
    RaftLogGc,
    SplitRegionCheck,
    PdHeartbeat,
    ConsistencyCheck,
    CheckMerge,
    CheckPeerStaleState,
}

#[derive(Debug, Clone, Copy)]
pub enum StoreTick {
    CompactCheck,
    CompactLockCf,
    PdStoreHeartbeat,
    SnapGc,
    CleanupImportSST,
}

#[derive(Debug, PartialEq)]
pub enum SignificantMsg {
    SnapshotStatus {
        to_peer_id: u64,
        status: SnapshotStatus,
    },
    Unreachable {
        to_peer_id: u64,
    },
}

pub enum PeerMsg {
    Quit,

    // For notify.
    RaftMessage(RaftMessage),

    RaftCmd {
        send_time: Instant,
        request: RaftCmdRequest,
        callback: Callback,
    },

    BatchRaftSnapCmds {
        send_time: Instant,
        batch: Vec<RaftCmdRequest>,
        on_finished: Callback,
    },

    SplitRegion {
        region_epoch: RegionEpoch,
        // It's an encoded key.
        // TODO: support meta key.
        split_keys: Vec<Vec<u8>>,
        callback: Callback,
    },

    // For consistency check
    ComputeHashResult {
        index: u64,
        hash: Vec<u8>,
    },

    // For region size
    RegionApproximateSize { size: u64 },

    // For region keys
    RegionApproximateKeys { keys: u64 },

    // Compaction finished event
    CompactedEvent(CompactedEvent),
    HalfSplitRegion {
        region_epoch: RegionEpoch,
        policy: CheckPolicy,
    },

    MergeFail,

    Tick(PeerTick),
    SignificantMsg(SignificantMsg),
    ApplyRes(ApplyRes),
}

pub enum StoreMsg {
    Quit,
    // Redirect to store if region not found.
    RaftMessage(RaftMessage),

    // For snapshot stats.
    SnapshotStats,

    // Compaction finished event
    CompactedEvent(CompactedEvent),

    ValidateSSTResult {
        invalid_ssts: Vec<SSTMeta>,
    },

    SeekRegion {
        from_key: Vec<u8>,
        filter: SeekRegionFilter,
        limit: u32,
        callback: SeekRegionCallback,
    },

    Tick(StoreTick),
}

impl fmt::Debug for PeerMsg {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            PeerMsg::Quit => write!(fmt, "Quit"),
            PeerMsg::RaftMessage(_) => write!(fmt, "Raft Message"),
            PeerMsg::RaftCmd { .. } => write!(fmt, "Raft Command"),
            PeerMsg::BatchRaftSnapCmds { .. } => write!(fmt, "Batch Raft Commands"),
            PeerMsg::ComputeHashResult {
                index,
                ref hash,
            } => write!(
                fmt,
                "ComputeHashResult [index: {}, hash: {}]",
                index,
                escape(hash)
            ),
            PeerMsg::SplitRegion {
                ref split_keys,
                ..
            } => write!(fmt, "Split at key {:?}", split_keys),
            PeerMsg::RegionApproximateSize { size } => write!(
                fmt,
                "Region's approximate size [size: {:?}]",
                size
            ),
            PeerMsg::RegionApproximateKeys { keys } => write!(
                fmt,
                "Region's approximate keys [keys: {:?}]",
                keys
            ),
            PeerMsg::HalfSplitRegion { .. } => {
                write!(fmt, "Half Split region")
            }
            PeerMsg::MergeFail => write!(fmt, "MergeFail"),
            PeerMsg::Tick(t) => write!(fmt, "{:?}", t),
            PeerMsg::SignificantMsg(ref msg) => write!(fmt, "{:?}", msg),
            PeerMsg::ApplyRes(ref res) => write!(fmt, "ApplyRes"),
        }
    }
}

impl PeerMsg {
    pub fn new_raft_cmd(request: RaftCmdRequest, callback: Callback) -> PeerMsg {
        PeerMsg::RaftCmd {
            send_time: Instant::now(),
            request,
            callback,
        }
    }

    pub fn new_half_split_region(
        region_epoch: RegionEpoch,
        policy: CheckPolicy,
    ) -> PeerMsg {
        PeerMsg::HalfSplitRegion {
            region_epoch,
            policy,
        }
    }
}

impl fmt::Debug for StoreMsg {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            StoreMsg::Quit => write!(fmt, "Quit"),
            StoreMsg::SnapshotStats => write!(fmt, "Snapshot stats"),
            StoreMsg::CompactedEvent(ref event) => write!(fmt, "CompactedEvent cf {}", event.cf),
            StoreMsg::ValidateSSTResult { .. } => write!(fmt, "Validate SST Result"),
            StoreMsg::SeekRegion { ref from_key, .. } => {
                write!(fmt, "Seek Region from_key {:?}", from_key)
            }
            StoreMsg::Tick(t) => write!(fmt, "{:?}", t)
        }
    }
}
