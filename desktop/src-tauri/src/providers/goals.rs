//! The one reply channel the streaming loop still hands back to a caller.
//!
//! Goals used to live here as a remote protocol: Pantheon carries a goal in
//! the session's system prompt, set through the `_pantheon/goal` extension
//! before a turn, so the desktop's goal state is local (see
//! `task_controls::task_goal`). What remains is the generic reply type the
//! app-server style control requests use.
use super::*;
use tokio::sync::oneshot;

pub(super) type RpcReply = oneshot::Sender<Result<Value, String>>;
