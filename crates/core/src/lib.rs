//! Lenslet's facts about AWS calls, as pure functions of a call's request and
//! response: which service and operation it is, the IAM access it needs (checked
//! against a hand-written corpus of recorded SDK calls), why it failed, a short
//! name for its target, and the IAM policy for a set of permissions.

pub mod access;
pub mod body;
pub mod cbor;
pub mod policy;
pub mod problem;
pub mod target;
pub mod trace;
pub mod wire;
