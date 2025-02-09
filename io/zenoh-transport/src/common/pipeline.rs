use crate::common::batch::BatchConfig;

//
// Copyright (c) 2023 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use super::{
    batch::{Encode, WBatch},
    priority::{TransportChannelTx, TransportPriorityTx},
};
use flume::{bounded, Receiver, Sender};
use ringbuffer_spsc::{RingBuffer, RingBufferReader, RingBufferWriter};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use std::{
    sync::atomic::{AtomicBool, AtomicU16, Ordering},
    time::Instant,
};
use zenoh_buffers::{
    reader::{HasReader, Reader},
    writer::HasWriter,
    ZBuf,
};
use zenoh_codec::{transport::batch::BatchError, WCodec, Zenoh080};
use zenoh_config::QueueSizeConf;
use zenoh_core::zlock;
use zenoh_protocol::core::Reliability;
use zenoh_protocol::network::NetworkMessage;
use zenoh_protocol::{
    core::Priority,
    transport::{
        fragment::FragmentHeader,
        frame::{self, FrameHeader},
        BatchSize, TransportMessage,
    },
};

// It's faster to work directly with nanoseconds.
// Backoff will never last more the u32::MAX nanoseconds.
type NanoSeconds = u32;

const RBLEN: usize = QueueSizeConf::MAX;

// Inner structure to reuse serialization batches
struct StageInRefill {
    n_ref_r: Receiver<()>,
    s_ref_r: RingBufferReader<WBatch, RBLEN>,
}

impl StageInRefill {
    fn pull(&mut self) -> Option<WBatch> {
        self.s_ref_r.pull()
    }

    fn wait(&self) -> bool {
        self.n_ref_r.recv().is_ok()
    }

    fn wait_deadline(&self, instant: Instant) -> bool {
        self.n_ref_r.recv_deadline(instant).is_ok()
    }
}

// Inner structure to link the initial stage with the final stage of the pipeline
struct StageInOut {
    n_out_w: Sender<()>,
    s_out_w: RingBufferWriter<WBatch, RBLEN>,
    bytes: Arc<AtomicU16>,
    backoff: Arc<AtomicBool>,
}

impl StageInOut {
    #[inline]
    fn notify(&self, bytes: BatchSize) {
        self.bytes.store(bytes, Ordering::Relaxed);
        if !self.backoff.load(Ordering::Relaxed) {
            let _ = self.n_out_w.try_send(());
        }
    }

    #[inline]
    fn move_batch(&mut self, batch: WBatch) {
        let _ = self.s_out_w.push(batch);
        self.bytes.store(0, Ordering::Relaxed);
        let _ = self.n_out_w.try_send(());
    }
}

// Inner structure containing mutexes for current serialization batch and SNs
struct StageInMutex {
    current: Arc<Mutex<Option<WBatch>>>,
    priority: TransportPriorityTx,
}

impl StageInMutex {
    #[inline]
    fn current(&self) -> MutexGuard<'_, Option<WBatch>> {
        zlock!(self.current)
    }

    #[inline]
    fn channel(&self, is_reliable: bool) -> MutexGuard<'_, TransportChannelTx> {
        if is_reliable {
            zlock!(self.priority.reliable)
        } else {
            zlock!(self.priority.best_effort)
        }
    }
}

// This is the initial stage of the pipeline where messages are serliazed on
struct StageIn {
    s_ref: StageInRefill,
    s_out: StageInOut,
    mutex: StageInMutex,
    fragbuf: ZBuf,
}

impl StageIn {
    fn push_network_message(
        &mut self,
        msg: &mut NetworkMessage,
        priority: Priority,
        deadline_before_drop: Option<Instant>,
    ) -> bool {
        // Lock the current serialization batch.
        let mut c_guard = self.mutex.current();

        macro_rules! zgetbatch_rets {
            ($fragment:expr, $restore_sn:expr) => {
                loop {
                    match c_guard.take() {
                        Some(batch) => break batch,
                        None => match self.s_ref.pull() {
                            Some(mut batch) => {
                                batch.clear();
                                break batch;
                            }
                            None => {
                                drop(c_guard);
                                match deadline_before_drop {
                                    Some(deadline) if !$fragment => {
                                        // We are in the congestion scenario and message is droppable
                                        // Wait for an available batch until deadline
                                        if !self.s_ref.wait_deadline(deadline) {
                                            // Still no available batch.
                                            // Restore the sequence number and drop the message
                                            $restore_sn;
                                            return false
                                        }
                                    }
                                    _ => {
                                        // Block waiting for an available batch
                                        if !self.s_ref.wait() {
                                            // Some error prevented the queue to wait and give back an available batch
                                            // Restore the sequence number and drop the message
                                            $restore_sn;
                                            return false;
                                        }
                                    }
                                }
                                c_guard = self.mutex.current();
                            }
                        },
                    }
                }
            };
        }

        macro_rules! zretok {
            ($batch:expr) => {{
                // Move out existing batch
                self.s_out.move_batch($batch);
                return true;
            }};
        }

        // Get the current serialization batch.
        let mut batch = zgetbatch_rets!(false, {});
        // Attempt the serialization on the current batch
        let e = match batch.encode(&*msg) {
            Ok(_) => zretok!(batch),
            Err(e) => e,
        };

        // Lock the channel. We are the only one that will be writing on it.
        let mut tch = self.mutex.channel(msg.is_reliable());

        // Retrieve the next SN
        let sn = tch.sn.get();

        // The Frame
        let frame = FrameHeader {
            reliability: Reliability::Reliable, // TODO
            sn,
            ext_qos: frame::ext::QoSType::new(priority),
        };

        if let BatchError::NewFrame = e {
            // Attempt a serialization with a new frame
            if batch.encode((&*msg, &frame)).is_ok() {
                zretok!(batch);
            }
        }

        if !batch.is_empty() {
            // Move out existing batch
            self.s_out.move_batch(batch);
            batch = zgetbatch_rets!(false, tch.sn.set(sn).unwrap());
        }

        // Attempt a second serialization on fully empty batch
        if batch.encode((&*msg, &frame)).is_ok() {
            zretok!(batch);
        }

        // The second serialization attempt has failed. This means that the message is
        // too large for the current batch size: we need to fragment.
        // Reinsert the current batch for fragmentation.
        *c_guard = Some(batch);

        // Take the expandable buffer and serialize the totality of the message
        self.fragbuf.clear();

        let mut writer = self.fragbuf.writer();
        let codec = Zenoh080::new();
        codec.write(&mut writer, &*msg).unwrap();

        // Fragment the whole message
        let mut fragment = FragmentHeader {
            reliability: frame.reliability,
            more: true,
            sn,
            ext_qos: frame.ext_qos,
        };
        let mut reader = self.fragbuf.reader();
        while reader.can_read() {
            // Get the current serialization batch
            // Treat all messages as non-droppable once we start fragmenting
            batch = zgetbatch_rets!(true, tch.sn.set(sn).unwrap());

            // Serialize the message fragment
            match batch.encode((&mut reader, &mut fragment)) {
                Ok(_) => {
                    // Update the SN
                    fragment.sn = tch.sn.get();
                    // Move the serialization batch into the OUT pipeline
                    self.s_out.move_batch(batch);
                }
                Err(_) => {
                    // Restore the sequence number
                    tch.sn.set(sn).unwrap();
                    // Reinsert the batch
                    *c_guard = Some(batch);
                    tracing::warn!(
                        "Zenoh message dropped because it can not be fragmented: {:?}",
                        msg
                    );
                    break;
                }
            }
        }

        // Clean the fragbuf
        self.fragbuf.clear();

        true
    }

    #[inline]
    fn push_transport_message(&mut self, msg: TransportMessage) -> bool {
        // Lock the current serialization batch.
        let mut c_guard = self.mutex.current();

        macro_rules! zgetbatch_rets {
            () => {
                loop {
                    match c_guard.take() {
                        Some(batch) => break batch,
                        None => match self.s_ref.pull() {
                            Some(mut batch) => {
                                batch.clear();
                                break batch;
                            }
                            None => {
                                drop(c_guard);
                                if !self.s_ref.wait() {
                                    return false;
                                }
                                c_guard = self.mutex.current();
                            }
                        },
                    }
                }
            };
        }

        macro_rules! zretok {
            ($batch:expr) => {{
                let bytes = $batch.len();
                *c_guard = Some($batch);
                drop(c_guard);
                self.s_out.notify(bytes);
                return true;
            }};
        }

        // Get the current serialization batch.
        let mut batch = zgetbatch_rets!();
        // Attempt the serialization on the current batch
        // Attempt the serialization on the current batch
        match batch.encode(&msg) {
            Ok(_) => zretok!(batch),
            Err(_) => {
                if !batch.is_empty() {
                    self.s_out.move_batch(batch);
                    batch = zgetbatch_rets!();
                }
            }
        };

        // The first serialization attempt has failed. This means that the current
        // batch is full. Therefore, we move the current batch to stage out.
        batch.encode(&msg).is_ok()
    }
}

// The result of the pull operation
enum Pull {
    Some(WBatch),
    None,
    Backoff(NanoSeconds),
}

// Inner structure to keep track and signal backoff operations
#[derive(Clone)]
struct Backoff {
    tslot: NanoSeconds,
    retry_time: NanoSeconds,
    last_bytes: BatchSize,
    bytes: Arc<AtomicU16>,
    backoff: Arc<AtomicBool>,
}

impl Backoff {
    fn new(tslot: NanoSeconds, bytes: Arc<AtomicU16>, backoff: Arc<AtomicBool>) -> Self {
        Self {
            tslot,
            retry_time: 0,
            last_bytes: 0,
            bytes,
            backoff,
        }
    }

    fn next(&mut self) {
        if self.retry_time == 0 {
            self.retry_time = self.tslot;
            self.backoff.store(true, Ordering::Relaxed);
        } else {
            match self.retry_time.checked_mul(2) {
                Some(rt) => {
                    self.retry_time = rt;
                }
                None => {
                    self.retry_time = NanoSeconds::MAX;
                    tracing::warn!(
                        "Pipeline pull backoff overflow detected! Retrying in {}ns.",
                        self.retry_time
                    );
                }
            }
        }
    }

    fn reset(&mut self) {
        self.retry_time = 0;
        self.backoff.store(false, Ordering::Relaxed);
    }
}

// Inner structure to link the final stage with the initial stage of the pipeline
struct StageOutIn {
    s_out_r: RingBufferReader<WBatch, RBLEN>,
    current: Arc<Mutex<Option<WBatch>>>,
    backoff: Backoff,
}

impl StageOutIn {
    #[inline]
    fn try_pull(&mut self) -> Pull {
        if let Some(batch) = self.s_out_r.pull() {
            return Pull::Some(batch);
        }

        self.try_pull_deep()
    }

    fn try_pull_deep(&mut self) -> Pull {
        let new_bytes = self.backoff.bytes.load(Ordering::Relaxed);
        let old_bytes = self.backoff.last_bytes;
        self.backoff.last_bytes = new_bytes;

        if new_bytes == old_bytes {
            // It seems no new bytes have been written on the batch, try to pull
            if let Ok(mut g) = self.current.try_lock() {
                // First try to pull from stage OUT to make sure we are not in the case
                // where new_bytes == old_bytes are because of two identical serializations
                if let Some(batch) = self.s_out_r.pull() {
                    return Pull::Some(batch);
                }

                // An incomplete (non-empty) batch may be available in the state IN pipeline.
                match g.take() {
                    Some(batch) => {
                        return Pull::Some(batch);
                    }
                    None => {
                        return Pull::None;
                    }
                }
            }
            // Go to backoff
        }

        // Do backoff
        self.backoff.next();
        Pull::Backoff(self.backoff.retry_time)
    }
}

struct StageOutRefill {
    n_ref_w: Sender<()>,
    s_ref_w: RingBufferWriter<WBatch, RBLEN>,
}

impl StageOutRefill {
    fn refill(&mut self, batch: WBatch) {
        assert!(self.s_ref_w.push(batch).is_none());
        let _ = self.n_ref_w.try_send(());
    }
}

struct StageOut {
    s_in: StageOutIn,
    s_ref: StageOutRefill,
}

impl StageOut {
    #[inline]
    fn try_pull(&mut self) -> Pull {
        self.s_in.try_pull()
    }

    #[inline]
    fn refill(&mut self, batch: WBatch) {
        self.s_ref.refill(batch);
    }

    fn drain(&mut self, guard: &mut MutexGuard<'_, Option<WBatch>>) -> Vec<WBatch> {
        let mut batches = vec![];
        // Empty the ring buffer
        while let Some(batch) = self.s_in.s_out_r.pull() {
            batches.push(batch);
        }
        // Take the current batch
        if let Some(batch) = guard.take() {
            batches.push(batch);
        }
        batches
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TransmissionPipelineConf {
    pub(crate) batch: BatchConfig,
    pub(crate) queue_size: [usize; Priority::NUM],
    pub(crate) wait_before_drop: Duration,
    pub(crate) backoff: Duration,
}

// A 2-stage transmission pipeline
pub(crate) struct TransmissionPipeline;
impl TransmissionPipeline {
    // A MPSC pipeline
    pub(crate) fn make(
        config: TransmissionPipelineConf,
        priority: &[TransportPriorityTx],
    ) -> (TransmissionPipelineProducer, TransmissionPipelineConsumer) {
        let mut stage_in = vec![];
        let mut stage_out = vec![];

        let default_queue_size = [config.queue_size[Priority::default() as usize]];
        let size_iter = if priority.len() == 1 {
            default_queue_size.iter()
        } else {
            config.queue_size.iter()
        };

        // Create the channel for notifying that new batches are in the out ring buffer
        // This is a MPSC channel
        let (n_out_w, n_out_r) = bounded(1);

        for (prio, num) in size_iter.enumerate() {
            assert!(*num != 0 && *num <= RBLEN);

            // Create the refill ring buffer
            // This is a SPSC ring buffer
            let (mut s_ref_w, s_ref_r) = RingBuffer::<WBatch, RBLEN>::init();
            // Fill the refill ring buffer with batches
            for _ in 0..*num {
                let batch = WBatch::new(config.batch);
                assert!(s_ref_w.push(batch).is_none());
            }
            // Create the channel for notifying that new batches are in the refill ring buffer
            // This is a SPSC channel
            let (n_ref_w, n_ref_r) = bounded(1);

            // Create the refill ring buffer
            // This is a SPSC ring buffer
            let (s_out_w, s_out_r) = RingBuffer::<WBatch, RBLEN>::init();
            let current = Arc::new(Mutex::new(None));
            let bytes = Arc::new(AtomicU16::new(0));
            let backoff = Arc::new(AtomicBool::new(false));

            stage_in.push(Mutex::new(StageIn {
                s_ref: StageInRefill { n_ref_r, s_ref_r },
                s_out: StageInOut {
                    n_out_w: n_out_w.clone(),
                    s_out_w,
                    bytes: bytes.clone(),
                    backoff: backoff.clone(),
                },
                mutex: StageInMutex {
                    current: current.clone(),
                    priority: priority[prio].clone(),
                },
                fragbuf: ZBuf::empty(),
            }));

            // The stage out for this priority
            stage_out.push(StageOut {
                s_in: StageOutIn {
                    s_out_r,
                    current,
                    backoff: Backoff::new(config.backoff.as_nanos() as NanoSeconds, bytes, backoff),
                },
                s_ref: StageOutRefill { n_ref_w, s_ref_w },
            });
        }

        let active = Arc::new(AtomicBool::new(true));
        let producer = TransmissionPipelineProducer {
            stage_in: stage_in.into_boxed_slice().into(),
            active: active.clone(),
            wait_before_drop: config.wait_before_drop,
        };
        let consumer = TransmissionPipelineConsumer {
            stage_out: stage_out.into_boxed_slice(),
            n_out_r,
            active,
        };

        (producer, consumer)
    }
}

#[derive(Clone)]
pub(crate) struct TransmissionPipelineProducer {
    // Each priority queue has its own Mutex
    stage_in: Arc<[Mutex<StageIn>]>,
    active: Arc<AtomicBool>,
    wait_before_drop: Duration,
}

impl TransmissionPipelineProducer {
    #[inline]
    pub(crate) fn push_network_message(&self, mut msg: NetworkMessage) -> bool {
        // If the queue is not QoS, it means that we only have one priority with index 0.
        let (idx, priority) = if self.stage_in.len() > 1 {
            let priority = msg.priority();
            (priority as usize, priority)
        } else {
            (0, Priority::default())
        };
        // If message is droppable, compute a deadline after which the sample could be dropped
        let deadline_before_drop = if msg.is_droppable() {
            Some(Instant::now() + self.wait_before_drop)
        } else {
            None
        };
        // Lock the channel. We are the only one that will be writing on it.
        let mut queue = zlock!(self.stage_in[idx]);
        queue.push_network_message(&mut msg, priority, deadline_before_drop)
    }

    #[inline]
    pub(crate) fn push_transport_message(&self, msg: TransportMessage, priority: Priority) -> bool {
        // If the queue is not QoS, it means that we only have one priority with index 0.
        let priority = if self.stage_in.len() > 1 {
            priority as usize
        } else {
            0
        };
        // Lock the channel. We are the only one that will be writing on it.
        let mut queue = zlock!(self.stage_in[priority]);
        queue.push_transport_message(msg)
    }

    pub(crate) fn disable(&self) {
        self.active.store(false, Ordering::Relaxed);

        // Acquire all the locks, in_guard first, out_guard later
        // Use the same locking order as in drain to avoid deadlocks
        let mut in_guards: Vec<MutexGuard<'_, StageIn>> =
            self.stage_in.iter().map(|x| zlock!(x)).collect();

        // Unblock waiting pullers
        for ig in in_guards.iter_mut() {
            ig.s_out.notify(BatchSize::MAX);
        }
    }
}

pub(crate) struct TransmissionPipelineConsumer {
    // A single Mutex for all the priority queues
    stage_out: Box<[StageOut]>,
    n_out_r: Receiver<()>,
    active: Arc<AtomicBool>,
}

impl TransmissionPipelineConsumer {
    pub(crate) async fn pull(&mut self) -> Option<(WBatch, usize)> {
        // Reset backoff before pulling
        for queue in self.stage_out.iter_mut() {
            queue.s_in.backoff.reset();
        }

        while self.active.load(Ordering::Relaxed) {
            // Calculate the backoff maximum
            let mut bo = NanoSeconds::MAX;
            for (prio, queue) in self.stage_out.iter_mut().enumerate() {
                match queue.try_pull() {
                    Pull::Some(batch) => {
                        return Some((batch, prio));
                    }
                    Pull::Backoff(b) => {
                        if b < bo {
                            bo = b;
                        }
                    }
                    Pull::None => {}
                }
            }

            // In case of writing many small messages, `recv_async()` will most likely return immedietaly.
            // While trying to pull from the queue, the stage_in `lock()` will most likely taken, leading to
            // a spinning behaviour while attempting to take the lock. Yield the current task to avoid
            // spinning the current task indefinitely.
            tokio::task::yield_now().await;

            // Wait for the backoff to expire or for a new message
            let res =
                tokio::time::timeout(Duration::from_nanos(bo as u64), self.n_out_r.recv_async())
                    .await;
            match res {
                Ok(Ok(())) => {
                    // We have received a notification from the channel that some bytes are available, retry to pull.
                }
                Ok(Err(_channel_error)) => {
                    // The channel is closed, we can't be notified anymore. Break the loop and return None.
                    break;
                }
                Err(_timeout) => {
                    // The backoff timeout expired. Be aware that tokio timeout may not sleep for short duration since
                    // it has time resolution of 1ms: https://docs.rs/tokio/latest/tokio/time/fn.sleep.html
                }
            }
        }
        None
    }

    pub(crate) fn refill(&mut self, batch: WBatch, priority: usize) {
        self.stage_out[priority].refill(batch);
    }

    pub(crate) fn drain(&mut self) -> Vec<(WBatch, usize)> {
        // Drain the remaining batches
        let mut batches = vec![];

        // Acquire all the locks, in_guard first, out_guard later
        // Use the same locking order as in disable to avoid deadlocks
        let locks = self
            .stage_out
            .iter()
            .map(|x| x.s_in.current.clone())
            .collect::<Vec<_>>();
        let mut currents: Vec<MutexGuard<'_, Option<WBatch>>> =
            locks.iter().map(|x| zlock!(x)).collect::<Vec<_>>();

        for (prio, s_out) in self.stage_out.iter_mut().enumerate() {
            let mut bs = s_out.drain(&mut currents[prio]);
            for b in bs.drain(..) {
                batches.push((b, prio));
            }
        }

        batches
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        convert::TryFrom,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };
    use tokio::task;
    use tokio::time::timeout;
    use zenoh_buffers::{
        reader::{DidntRead, HasReader},
        ZBuf,
    };
    use zenoh_codec::{RCodec, Zenoh080};
    use zenoh_protocol::{
        core::{Bits, CongestionControl, Encoding, Priority},
        network::{ext, Push},
        transport::{BatchSize, Fragment, Frame, TransportBody, TransportSn},
        zenoh::{PushBody, Put},
    };
    use zenoh_result::ZResult;

    const SLEEP: Duration = Duration::from_millis(100);
    const TIMEOUT: Duration = Duration::from_secs(60);

    const CONFIG_STREAMED: TransmissionPipelineConf = TransmissionPipelineConf {
        batch: BatchConfig {
            mtu: BatchSize::MAX,
            is_streamed: true,
            #[cfg(feature = "transport_compression")]
            is_compression: true,
        },
        queue_size: [1; Priority::NUM],
        wait_before_drop: Duration::from_millis(1),
        backoff: Duration::from_micros(1),
    };

    const CONFIG_NOT_STREAMED: TransmissionPipelineConf = TransmissionPipelineConf {
        batch: BatchConfig {
            mtu: BatchSize::MAX,
            is_streamed: false,
            #[cfg(feature = "transport_compression")]
            is_compression: false,
        },
        queue_size: [1; Priority::NUM],
        wait_before_drop: Duration::from_millis(1),
        backoff: Duration::from_micros(1),
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tx_pipeline_flow() -> ZResult<()> {
        fn schedule(queue: TransmissionPipelineProducer, num_msg: usize, payload_size: usize) {
            // Send reliable messages
            let key = "test".into();
            let payload = ZBuf::from(vec![0_u8; payload_size]);

            let message: NetworkMessage = Push {
                wire_expr: key,
                ext_qos: ext::QoSType::new(Priority::Control, CongestionControl::Block, false),
                ext_tstamp: None,
                ext_nodeid: ext::NodeIdType::default(),
                payload: PushBody::Put(Put {
                    timestamp: None,
                    encoding: Encoding::default(),
                    ext_sinfo: None,
                    #[cfg(feature = "shared-memory")]
                    ext_shm: None,
                    ext_attachment: None,
                    ext_unknown: vec![],
                    payload,
                }),
            }
            .into();

            println!(
                "Pipeline Flow [>>>]: Sending {num_msg} messages with payload size of {payload_size} bytes"
            );
            for i in 0..num_msg {
                println!(
                    "Pipeline Flow [>>>]: Pushed {} msgs ({payload_size} bytes)",
                    i + 1
                );
                queue.push_network_message(message.clone());
            }
        }

        async fn consume(mut queue: TransmissionPipelineConsumer, num_msg: usize) {
            let mut batches: usize = 0;
            let mut bytes: usize = 0;
            let mut msgs: usize = 0;
            let mut fragments: usize = 0;

            while msgs != num_msg {
                let (batch, priority) = queue.pull().await.unwrap();
                batches += 1;
                bytes += batch.len() as usize;
                // Create a ZBuf for deserialization starting from the batch
                let bytes = batch.as_slice();
                // Deserialize the messages
                let mut reader = bytes.reader();
                let codec = Zenoh080::new();

                loop {
                    let res: Result<TransportMessage, DidntRead> = codec.read(&mut reader);
                    match res {
                        Ok(msg) => {
                            match msg.body {
                                TransportBody::Frame(Frame { payload, .. }) => {
                                    msgs += payload.len()
                                }
                                TransportBody::Fragment(Fragment { more, .. }) => {
                                    fragments += 1;
                                    if !more {
                                        msgs += 1;
                                    }
                                }
                                _ => {
                                    msgs += 1;
                                }
                            }
                            println!("Pipeline Flow [<<<]: Pulled {} msgs", msgs + 1);
                        }
                        Err(_) => break,
                    }
                }
                println!("Pipeline Flow [+++]: Refill {} msgs", msgs + 1);
                // Reinsert the batch
                queue.refill(batch, priority);
            }

            println!(
                "Pipeline Flow [<<<]: Received {msgs} messages, {bytes} bytes, {batches} batches, {fragments} fragments"
            );
        }

        // Pipeline priorities
        let tct = TransportPriorityTx::make(Bits::from(TransportSn::MAX))?;
        let priorities = vec![tct];

        // Total amount of bytes to send in each test
        let bytes: usize = 100_000_000;
        let max_msgs: usize = 1_000;
        // Payload size of the messages
        let payload_sizes = [8, 64, 512, 4_096, 8_192, 32_768, 262_144, 2_097_152];

        for ps in payload_sizes.iter() {
            if u64::try_from(*ps).is_err() {
                break;
            }

            // Compute the number of messages to send
            let num_msg = max_msgs.min(bytes / ps);

            let (producer, consumer) =
                TransmissionPipeline::make(CONFIG_NOT_STREAMED, priorities.as_slice());

            let t_c = task::spawn(async move {
                consume(consumer, num_msg).await;
            });

            let c_ps = *ps;
            let t_s = task::spawn_blocking(move || {
                schedule(producer, num_msg, c_ps);
            });

            let res = tokio::time::timeout(TIMEOUT, futures::future::join_all([t_c, t_s])).await;
            assert!(res.is_ok());
        }

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tx_pipeline_blocking() -> ZResult<()> {
        fn schedule(queue: TransmissionPipelineProducer, counter: Arc<AtomicUsize>, id: usize) {
            // Make sure to put only one message per batch: set the payload size
            // to half of the batch in such a way the serialized zenoh message
            // will be larger then half of the batch size (header + payload).
            let payload_size = (CONFIG_STREAMED.batch.mtu / 2) as usize;

            // Send reliable messages
            let key = "test".into();
            let payload = ZBuf::from(vec![0_u8; payload_size]);

            let message: NetworkMessage = Push {
                wire_expr: key,
                ext_qos: ext::QoSType::new(Priority::Control, CongestionControl::Block, false),
                ext_tstamp: None,
                ext_nodeid: ext::NodeIdType::default(),
                payload: PushBody::Put(Put {
                    timestamp: None,
                    encoding: Encoding::default(),
                    ext_sinfo: None,
                    #[cfg(feature = "shared-memory")]
                    ext_shm: None,
                    ext_attachment: None,
                    ext_unknown: vec![],
                    payload,
                }),
            }
            .into();

            // The last push should block since there shouldn't any more batches
            // available for serialization.
            let num_msg = 1 + CONFIG_STREAMED.queue_size[0];
            for i in 0..num_msg {
                println!(
                    "Pipeline Blocking [>>>]: ({id}) Scheduling message #{i} with payload size of {payload_size} bytes"
                );
                queue.push_network_message(message.clone());
                let c = counter.fetch_add(1, Ordering::AcqRel);
                println!(
                    "Pipeline Blocking [>>>]: ({}) Scheduled message #{} (tot {}) with payload size of {} bytes",
                    id, i, c + 1,
                    payload_size
                );
            }
        }

        // Pipeline
        let tct = TransportPriorityTx::make(Bits::from(TransportSn::MAX))?;
        let priorities = vec![tct];
        let (producer, mut consumer) =
            TransmissionPipeline::make(CONFIG_NOT_STREAMED, priorities.as_slice());

        let counter = Arc::new(AtomicUsize::new(0));

        let c_producer = producer.clone();
        let c_counter = counter.clone();
        let h1 = task::spawn_blocking(move || {
            schedule(c_producer, c_counter, 1);
        });

        let c_counter = counter.clone();
        let h2 = task::spawn_blocking(move || {
            schedule(producer, c_counter, 2);
        });

        // Wait to have sent enough messages and to have blocked
        println!(
            "Pipeline Blocking [---]: waiting to have {} messages being scheduled",
            CONFIG_STREAMED.queue_size[Priority::MAX as usize]
        );
        let check = async {
            while counter.load(Ordering::Acquire)
                < CONFIG_STREAMED.queue_size[Priority::MAX as usize]
            {
                tokio::time::sleep(SLEEP).await;
            }
        };

        timeout(TIMEOUT, check).await?;

        // Disable and drain the queue
        timeout(
            TIMEOUT,
            task::spawn_blocking(move || {
                println!("Pipeline Blocking [---]: draining the queue");
                let _ = consumer.drain();
            }),
        )
        .await??;

        // Make sure that the tasks scheduling have been unblocked
        println!("Pipeline Blocking [---]: waiting for schedule (1) to be unblocked");
        timeout(TIMEOUT, h1).await??;
        println!("Pipeline Blocking [---]: waiting for schedule (2) to be unblocked");
        timeout(TIMEOUT, h2).await??;

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn tx_pipeline_thr() {
        // Queue
        let tct = TransportPriorityTx::make(Bits::from(TransportSn::MAX)).unwrap();
        let priorities = vec![tct];
        let (producer, mut consumer) =
            TransmissionPipeline::make(CONFIG_STREAMED, priorities.as_slice());
        let count = Arc::new(AtomicUsize::new(0));
        let size = Arc::new(AtomicUsize::new(0));

        let c_size = size.clone();
        task::spawn_blocking(move || {
            loop {
                let payload_sizes: [usize; 16] = [
                    8, 16, 32, 64, 128, 256, 512, 1_024, 2_048, 4_096, 8_192, 16_384, 32_768,
                    65_536, 262_144, 1_048_576,
                ];
                for size in payload_sizes.iter() {
                    c_size.store(*size, Ordering::Release);

                    // Send reliable messages
                    let key = "pipeline/thr".into();
                    let payload = ZBuf::from(vec![0_u8; *size]);

                    let message: NetworkMessage = Push {
                        wire_expr: key,
                        ext_qos: ext::QoSType::new(
                            Priority::Control,
                            CongestionControl::Block,
                            false,
                        ),
                        ext_tstamp: None,
                        ext_nodeid: ext::NodeIdType::default(),
                        payload: PushBody::Put(Put {
                            timestamp: None,
                            encoding: Encoding::default(),
                            ext_sinfo: None,
                            #[cfg(feature = "shared-memory")]
                            ext_shm: None,
                            ext_attachment: None,
                            ext_unknown: vec![],
                            payload,
                        }),
                    }
                    .into();

                    let duration = Duration::from_millis(5_500);
                    let start = Instant::now();
                    while start.elapsed() < duration {
                        producer.push_network_message(message.clone());
                    }
                }
            }
        });

        let c_count = count.clone();
        task::spawn(async move {
            loop {
                let (batch, priority) = consumer.pull().await.unwrap();
                c_count.fetch_add(batch.len() as usize, Ordering::AcqRel);
                consumer.refill(batch, priority);
            }
        });

        let mut prev_size: usize = usize::MAX;
        loop {
            let received = count.swap(0, Ordering::AcqRel);
            let current: usize = size.load(Ordering::Acquire);
            if current == prev_size {
                let thr = (8.0 * received as f64) / 1_000_000_000.0;
                println!("{} bytes: {:.6} Gbps", current, 2.0 * thr);
            }
            prev_size = current;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
