//! Rivet-backed sequencer: the second implementation of the
//! actor-runtime seam (plan.md D3).
//!
//! The in-process [`choir_sequencer`] gets its single-writer guarantee
//! from owning an OS thread; here the same guarantee comes from Rivet's
//! actor model — one [`SequencerActor`] instance per repo, actions
//! serialized by the runtime, state persisted by the engine. The seam
//! contract is behavioral: total order, hash chain, per-client FIFO —
//! proven by `tests/conformance.rs`, the same properties the in-process
//! implementation's tests assert.
//!
//! [`choir_sequencer`]: ../choir_sequencer/index.html

use std::{future::Future, pin::Pin, sync::Arc};

use async_trait::async_trait;
use choir_oplog::{OpEntry, Witness, FORMAT_VERSION};
use rivetkit::{action, Action, Actor, Ctx, Handles};
use serde::{Deserialize, Serialize};

type BoxFuture<T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send>>;

/// Persisted actor state: the op log entries in order. Rivet owns
/// durability; the hash chain (each entry's `parent`) stays verifiable
/// independently of the runtime, per the D16 one-way-door rule.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LogState {
    /// Entries in sequence order.
    pub entries: Vec<OpEntry>,
}

/// Submit one op for ordering (the analogue of
/// `SequencerHandle::try_submit`).
#[derive(Debug, Serialize, Deserialize)]
pub struct SubmitOp {
    /// Signature-covered collaboration channel submitting the op.
    /// Serialized under the v1 `workspace` name for action compatibility.
    #[serde(rename = "workspace")]
    pub channel: String,
    /// Opaque operation body.
    pub payload: Vec<u8>,
    /// Author signature over `(channel, payload)`, if signed.
    pub author_sig: Option<Witness>,
}

/// The sequencer's acknowledgement.
#[derive(Debug, Serialize, Deserialize)]
pub struct SubmitReply {
    /// Position assigned in the total order.
    pub seq: u64,
    /// Hex content hash of the appended entry (the new head).
    pub hash_hex: String,
}

impl Action for SubmitOp {
    type Output = SubmitReply;
    const NAME: &'static str = "submit";
}

/// Fetch the full ordered log (conformance verification; a production
/// reader would page or subscribe instead).
#[derive(Debug, Serialize, Deserialize)]
pub struct DumpLog;

impl Action for DumpLog {
    type Output = Vec<OpEntry>;
    const NAME: &'static str = "dump";
}

/// One repo's sequencer, hosted on the Rivet runtime.
pub struct SequencerActor;

#[async_trait]
impl Actor for SequencerActor {
    type State = LogState;
    type Input = ();
    type Actions = (SubmitOp, DumpLog);
    type Events = ();
    type Queue = ();
    type ConnParams = ();
    type ConnState = ();
    type Action = action::Raw;

    async fn create_state(_ctx: &Ctx<Self>, _input: Self::Input) -> anyhow::Result<Self::State> {
        Ok(LogState::default())
    }

    async fn create(_ctx: &Ctx<Self>) -> anyhow::Result<Self> {
        Ok(Self)
    }
}

impl Handles<SubmitOp> for SequencerActor {
    type Future = BoxFuture<SubmitReply>;

    fn handle(self: Arc<Self>, ctx: Ctx<Self>, op: SubmitOp) -> Self::Future {
        Box::pin(async move {
            let mut state = ctx.state_mut();
            let parent = state.entries.last().map(OpEntry::content_hash);
            let seq = state.entries.len() as u64;
            let entry = OpEntry {
                format_version: FORMAT_VERSION,
                parent,
                seq,
                channel: op.channel,
                payload: op.payload,
                witnesses: Vec::new(),
                author_sig: op.author_sig,
            };
            let hash_hex = entry.content_hash().to_hex();
            state.entries.push(entry);
            Ok(SubmitReply { seq, hash_hex })
        })
    }
}

impl Handles<DumpLog> for SequencerActor {
    type Future = BoxFuture<Vec<OpEntry>>;

    fn handle(self: Arc<Self>, ctx: Ctx<Self>, _op: DumpLog) -> Self::Future {
        Box::pin(async move { Ok(ctx.state().entries.clone()) })
    }
}

/// Registers the sequencer under its canonical actor name.
pub fn register(registry: &mut rivetkit::Registry) {
    registry.register_actor::<SequencerActor>("choirSequencer");
}
