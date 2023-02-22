//! Concept for a mid-level API as it might exist in the library at some point
//! Main difference to the low-level API that exists now:
//! - Compile-time errors are moved to the runtime (e.g. when receiving messages
//!   or when trying to update a not-fully opened channel)
//! - Easier handling because it is just a single struct instead of having
//!   things in the type system for compile time errors.

use alloc::string::String;
use perun::{
    abiencode::types::U256,
    channel::{self, ProposedChannel},
    messages::{FunderReplyMessage, ParticipantMessage, WatcherReplyMessage},
    wire::MessageBus,
};

pub struct Channel<'cl, 'ch, B: MessageBus> {
    inner: ChannelInner<'cl, 'ch, B>,
}

/// Because we're storing both ActiveChannel and ChannelUpdate in this enum it
/// does not have `Copy` and it can only be moved if the variant is not `Active`
/// or the `update` Option is `None`.
///
/// Reason: ChannelUpdate contains a reference to the `ActiveChannel`, which
/// would break if we try to move it, since that reference is not
/// (self)-relative within `ChannelInner` (which is not easily possible in Rust
/// if at all). All references are fixed addresses and thus break on a memcopy.
enum ChannelInner<'cl, 'ch, B: MessageBus> {
    Proposed(channel::ProposedChannel<'cl, B>),
    AgreedUpon(channel::AgreedUponChannel<'cl, B>),
    Signed(channel::SignedChannel<'cl, B>, bool, bool),
    Active(
        channel::ActiveChannel<'cl, B>,
        Option<channel::ChannelUpdate<'cl, 'ch, B>>,
    ),
    // We store owned values in this enum and need to move the channel out of
    // the previous enum to be able to transition to the next state. While we
    // could lift that restriction (it is added just to forbid duplicating a
    // channel and doesn't have anything in its drop function) that would mean
    // less invariants in the type system. Unfortunately this means (at least as
    // far as I can tell) that we need to have this invalid state to move to
    // during the transition itself. It is only reachable when using reentrancy
    // or some other multi-threaded shenanigans. It might be reachable if the
    // transition panics for some reason instead of returning an Error. This
    // state should be unreachable.
    //
    // I hate having to do this but I couldn't find a good way to do it without
    // lifting the no-duplication invariant or using unsafe blocks, which could
    // (according to stack overflow) in the worst case result in undefined
    // behavior in the above panic case.
    TemporaryInvalidState,

    ForceClosed,
}

pub enum Error {
    InvalidState,
    HandleAccept(channel::HandleAcceptError),
    Rejected { reason: String },
    ProposalBuild(channel::ProposalBuildError),
    Signing(channel::SignError),
    AddSignature(channel::AddSignatureError),
    Build(channel::BuildError),
    ProposeUpdate(channel::ProposeUpdateError),
    HandleUpdate(channel::HandleUpdateError),
    Accept(channel::AcceptError),
    ApplyUpdate(channel::ApplyError),
}
impl From<channel::HandleAcceptError> for Error {
    fn from(e: channel::HandleAcceptError) -> Self {
        Self::HandleAccept(e)
    }
}
impl From<channel::ProposalBuildError> for Error {
    fn from(e: channel::ProposalBuildError) -> Self {
        Self::ProposalBuild(e)
    }
}
impl From<channel::SignError> for Error {
    fn from(e: channel::SignError) -> Self {
        Self::Signing(e)
    }
}
impl From<channel::AddSignatureError> for Error {
    fn from(e: channel::AddSignatureError) -> Self {
        Self::AddSignature(e)
    }
}
impl From<channel::BuildError> for Error {
    fn from(e: channel::BuildError) -> Self {
        Self::Build(e)
    }
}
impl From<channel::ProposeUpdateError> for Error {
    fn from(e: channel::ProposeUpdateError) -> Self {
        Self::ProposeUpdate(e)
    }
}
impl From<channel::HandleUpdateError> for Error {
    fn from(e: channel::HandleUpdateError) -> Self {
        Self::HandleUpdate(e)
    }
}
impl From<channel::AcceptError> for Error {
    fn from(e: channel::AcceptError) -> Self {
        Self::Accept(e)
    }
}
impl From<channel::ApplyError> for Error {
    fn from(e: channel::ApplyError) -> Self {
        Self::ApplyUpdate(e)
    }
}

impl<'cl, 'ch, B: MessageBus> Channel<'cl, 'ch, B> {
    pub fn new(channel: ProposedChannel<'cl, B>) -> Self {
        Self {
            inner: ChannelInner::Proposed(channel),
        }
    }

    /// Progress the inner state machine with the logic given in f.
    ///
    /// # Safety
    /// `f` should not panic, otherwise the channel will end up in
    /// `ChannelInner::TemporaryInvalidState` and we'll most likely panic the
    /// next time someone attempts a state transition, if the programm doesn't
    /// crash immediately due to the panic in `f`.
    fn progress<F>(&mut self, f: F) -> Result<(), Error>
    where
        F: FnOnce(
            ChannelInner<'cl, 'ch, B>,
        ) -> Result<ChannelInner<'cl, 'ch, B>, (ChannelInner<'cl, 'ch, B>, Error)>,
    {
        // Move ChannelInner out of self so we get ownership of the variant.
        let mut inner = ChannelInner::TemporaryInvalidState;
        core::mem::swap(&mut self.inner, &mut inner);
        // Call the transition function and receive the new Inner state (which
        // can be what we put in, too).
        //
        // Whatever happens: Write back to self.inner. As far as I can tell this
        // can only fail if f panics. If that happens inner will be dropped,
        // leaving channel.inner at TemporaryInvalidState and does not break
        // memory guarantees in any way (no Undefined Behavior). Alternatively
        // we could mark progress as unsafe and require f to not panic, that
        // would require an unsafe block everywhere it is used.
        match f(inner) {
            Ok(ch) => {
                self.inner = ch;
                Ok(())
            }
            Err((ch, e)) => {
                self.inner = ch;
                Err(e)
            }
        }
    }

    pub fn update(&'ch mut self, amount: U256, is_final: bool) -> Result<(), Error> {
        // Trying to use self.progress here was a pain due the nature of how we
        // currently store the pending update. self.inner can neither be moved
        // nor copied while a pending update exists, since it is currently
        // self-referential. Thus the same applies to Self, too.
        match self.inner {
            ChannelInner::Active(ref mut ch, ref mut update) => {
                let mut new_state = ch.state().make_next_state();
                new_state.outcome.balances.0[0].0[0] += amount;
                new_state.outcome.balances.0[0].0[1] -= amount;
                new_state.is_final = is_final;
                match ch.update(new_state) {
                    Ok(u) => {
                        *update = Some(u);
                        Ok(())
                    }
                    Err(e) => Err(e.into()),
                }
            }
            ChannelInner::TemporaryInvalidState => unreachable!(),
            _ => return Err(Error::InvalidState),
        }
    }

    pub fn force_close(&mut self) -> Result<(), Error> {
        self.progress(|inner| match inner {
            ChannelInner::Active(ch, update) => {
                // TODO: Make sure this works even if update is not None
                match ch.force_close() {
                    Ok(_) => Ok(ChannelInner::ForceClosed),
                    Err((ch, e)) => Err((ChannelInner::Active(ch, update), e.into())),
                }
            }
            ChannelInner::TemporaryInvalidState => unreachable!(),
            inner => return Err((inner, Error::InvalidState)),
        })
    }

    pub fn process_watcher_reply(&mut self, msg: WatcherReplyMessage) -> Result<(), Error> {
        self.progress(|inner| match (inner, msg) {
            (ChannelInner::Signed(ch, _, funded), WatcherReplyMessage::Ack { .. }) => {
                if funded {
                    Ok(ChannelInner::Active(ch.mark_funded(), None))
                } else {
                    Ok(ChannelInner::Signed(ch, true, funded))
                }
            }
            // We're currently not processing acknowledge messages, as there is
            // currently no mechanism to auto-reject updates if there is no
            // recent acknowledged state. Nor is there an automatic
            // re-transmission in case of network connection closure, so there
            // is currently no need to do anything with the acknowledgement.
            (inner @ ChannelInner::Active(_, _), WatcherReplyMessage::Ack { .. }) => Ok(inner),
            (inner @ ChannelInner::Active(_, _), WatcherReplyMessage::DisputeAck { .. }) => {
                Ok(inner)
            }
            (ChannelInner::Active(_, _), WatcherReplyMessage::DisputeNotification { .. }) => {
                Ok(ChannelInner::ForceClosed)
            }
            (ChannelInner::TemporaryInvalidState, _) => unreachable!(),
            (inner, _) => Err((inner, Error::InvalidState)),
        })
    }

    pub fn process_funder_reply(&mut self, msg: FunderReplyMessage) -> Result<(), Error> {
        self.progress(|inner| match (inner, msg) {
            (ChannelInner::Signed(ch, watching, _), FunderReplyMessage::Funded { .. }) => {
                if watching {
                    Ok(ChannelInner::Active(ch.mark_funded(), None))
                } else {
                    Ok(ChannelInner::Signed(ch, watching, true))
                }
            }
            (ChannelInner::TemporaryInvalidState, _) => unreachable!(),
            (inner, _) => Err((inner, Error::InvalidState)),
        })
    }

    pub fn process_participant_msg(&mut self, msg: ParticipantMessage) -> Result<(), Error> {
        // Notes on invalid pairs:
        // - `ParticipantMessage::Auth` is not for a single channel and doesn't
        //   make sense in this context.
        // - Incomming proposals `ParticipantMessage::ChannelProposal` are not
        //   for a specific channel either
        self.progress(|inner| match (inner, msg) {
            // ProposedChannel
            (ChannelInner::Proposed(mut ch), ParticipantMessage::ProposalAccepted(msg)) => {
                match ch.participant_accepted(1, msg) {
                    Ok(_) => {},
                    Err(e) => return Err((ChannelInner::Proposed(ch), e.into())),
                }
                let mut ch = match ch.build() {
                    Ok(ch) => ch,
                    Err((ch, e)) => return Err((ChannelInner::Proposed(ch), e.into())),
                };
                match ch.sign() {
                    Ok(_) => Ok(ChannelInner::AgreedUpon(ch)),
                    Err(e) => unreachable!("We could reach this only by failing to abiencode the state, and there currently is no way to recover from this except for panicing. Error: {e:?}"),
                }

            }
            (inner @ChannelInner::Proposed(_), ParticipantMessage::ProposalRejected { reason, .. }) => {
                Err((inner, Error::Rejected { reason }))
            }

            // AgreedUponChannel
            (
                ChannelInner::AgreedUpon(mut ch),
                ParticipantMessage::ChannelUpdateAccepted(msg),
            ) => {
                // ch.add_signature(msg).or_else(|e| Err((ChannelInner::AgreedUpon(ch), e.into())))?;
                match ch.add_signature(msg) {
                    Ok(_) => {},
                    Err(e) => return Err((ChannelInner::AgreedUpon(ch), e.into())),
                }
                match ch.build() {
                    Ok(ch) => Ok(ChannelInner::Signed(ch, false, false)),
                    Err((ch, e)) => Err((ChannelInner::AgreedUpon(ch), e.into())),
                }
            }
            (
                inner @ChannelInner::AgreedUpon(_),
                ParticipantMessage::ChannelUpdateRejected { reason, .. },
            ) => Err((inner, Error::Rejected { reason })),

            // SignedChannel: (nothing is valid until we receive a response from Watcher/Funder, though production code probably
            // will have to cache update requests.

            // ActiveChannel: Note that this will only accept incomming
            // ChannelUpdates if we have not proposed one ourselfes, which could
            // result in a deadlock if both participants try to propose an
            // update at the same time. The way to prevent this would be to add
            // timeouts, drop our own update, immediately reply with a
            // RejectMessage or let the application decide which update it
            // wants. Since we are the application in this demo we're just
            // rejecting incomming updates.
            //
            // Technical debt: At the moment we cannot use `ch.handle_update` to
            // handle the incomming message if update != None, because the
            // existing update contains a mutable reference to the channel
            // (`ch`). This is also the reason why we're currently rejecting the
            // incomming message with `InvalidMsg` on the client side instead of
            // replying with a rejected message. We will have to think about how
            // exactly we want to handle the situation of multiple updates. As
            // mentioned before it is currently strictly forbidden to have
            // multiple `ChannelUpdate` objects for the same channel.
            (ChannelInner::Active(mut ch, None), ParticipantMessage::ChannelUpdate(msg)) => {
                // Technical debt: We always return update=None because the
                // update is entirely handled within this block. The `update` in
                // `ChannelInner::Active` is only meant for updates we've
                // proposed, which means we have to wait for the other
                // participant(s). This does however limit this abstraction to
                // 2-party channels only. Changing it to work with >2
                // participants shouldn't be too difficult (as we'd just have to
                // look into the error message on apply), but
                // `ChannelInner::Signed` already does that, too. Additionally,
                // this abstraction currently cannot handle receiving a
                // `ParticipantMessage::ChannelUpdateAccepted` before receiving
                // the corresponding `ParticipantMessage::ChannelUpdate`, which
                // can happen with >2 participants.

                let mut update = match ch.handle_update(msg) {
                    Ok(v) => v,
                    Err(e) => return Err((ChannelInner::Active(ch, None), e.into())),
                };
                match update.accept() {
                    Ok(_) => {},
                    Err(e) => return Err((ChannelInner::Active(ch, None), e.into())),
                }
                match update.apply() {
                    Ok(_) => {},
                    Err((_, e)) => return Err((ChannelInner::Active(ch, None), e.into())),
                }
                Ok(ChannelInner::Active(ch, None))
            }
            (ChannelInner::Active(ch, Some(mut update)), ParticipantMessage::ChannelUpdateAccepted(msg)) => {
                match update.participant_accepted(1, msg) {
                    Ok(_) => {},
                    Err(e) => return Err((ChannelInner::Active(ch, Some(update)), e.into())),
                }
                match update.apply() {
                    Ok(_) => Ok(ChannelInner::Active(ch, None)),
                    Err((u, e)) => Err((ChannelInner::Active(ch, Some(u)), e.into())),
                }
            },
            (
                ChannelInner::Active(ch, Some(_)),
                ParticipantMessage::ChannelUpdateRejected {
                    ..
                },
            ) => {
                Ok(ChannelInner::Active(ch, None))
            },
            (ChannelInner::TemporaryInvalidState, _) => unreachable!(),
            (inner, _) => Err((inner, Error::InvalidState)),
        })
    }
}
