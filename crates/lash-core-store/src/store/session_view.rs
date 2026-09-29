//! The permanent thin session view over the multi-session store (ADR 0112 §3).
use std::num::NonZeroU32;
use std::sync::Arc;

use super::*;
use crate::SessionId;

/// One session's view of a [`RuntimeStore`].
///
/// The view holds exactly two fields: the store and the session id. It has
/// no backend state, no latch, no binding mode and no discovery fallback. It
/// implements no store trait. Its session-scoped operations are inherent
/// methods generated from the store's operation list, with the same names,
/// minus the `session_id` parameter: `view.load_session_window(selector)`
/// calls `store.load_session_window(&self.session_id, selector)`.
///
/// A session-local request cannot override the view's identity. For an
/// operation whose request carries a session id, the forwarder compares that
/// id with the view's before it forwards, and a mismatch is
/// [`StoreError::ForeignSessionRequest`] with nothing sent to the store.
///
/// Catalog and deployment operations are not on the view; a caller that
/// needs them uses [`Self::store`] or holds the deployment store.
#[derive(Clone)]
pub struct SessionStore {
    store: Arc<dyn RuntimeStore>,
    session_id: SessionId,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl SessionStore {
    /// Validates the id ([`validate_session_id`]). It does not admit, read or
    /// bind: a view is a value.
    pub fn new(store: Arc<dyn RuntimeStore>, session_id: SessionId) -> Result<Self, StoreError> {
        validate_session_id(&session_id)?;
        Ok(Self { store, session_id })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn store(&self) -> &Arc<dyn RuntimeStore> {
        &self.store
    }

    /// The store's recorded fleet format.
    pub fn fleet_format(&self) -> FleetFormat {
        self.store.fleet_format()
    }

    fn check_request(&self, request: &(impl CarriesSession + ?Sized)) -> Result<(), StoreError> {
        request.check_session(&self.session_id)
    }
}

/// A request that names the session it acts on (ADR 0112 §1).
pub trait CarriesSession {
    /// Refuse the request unless every session id it carries is `session_id`.
    fn check_session(&self, session_id: &SessionId) -> Result<(), StoreError>;
}

fn check_carried(
    request_session_id: &SessionId,
    view_session_id: &SessionId,
) -> Result<(), StoreError> {
    if request_session_id == view_session_id {
        Ok(())
    } else {
        Err(StoreError::ForeignSessionRequest {
            view_session_id: view_session_id.clone(),
            request_session_id: request_session_id.clone(),
        })
    }
}

impl<T: CarriesSession + ?Sized> CarriesSession for &T {
    fn check_session(&self, session_id: &SessionId) -> Result<(), StoreError> {
        (**self).check_session(session_id)
    }
}

impl<T: CarriesSession> CarriesSession for [T] {
    fn check_session(&self, session_id: &SessionId) -> Result<(), StoreError> {
        self.iter()
            .try_for_each(|request| request.check_session(session_id))
    }
}

macro_rules! carries_session_field {
    ($($ty:ty => |$request:ident| $session:expr;)*) => {
        $(
            impl CarriesSession for $ty {
                fn check_session(&self, session_id: &SessionId) -> Result<(), StoreError> {
                    let $request = self;
                    check_carried($session, session_id)
                }
            }
        )*
    };
}

carries_session_field! {
    RuntimeCommit => |request| &request.session_id;
    DriveFence => |request| request.session();
    AdmitRootRequest => |request| request.fence.session();
    CheckpointAdmissionRequest => |request| request.fence.session();
    crate::TurnAddress => |request| &request.session_id;
    crate::TurnCancelRequest => |request| &request.address.session_id;
    crate::TurnCancelClosureAuthorization => |request| request.session_id();
    crate::PendingTurnInputBatch => |request| request.session_id();
    crate::PendingTurnInputDraft => |request| &request.session_id;
    crate::QueuedWorkBatchDraft => |request| &request.session_id;
    TurnParkWrite => |request| &request.session_id;
    SessionMeta => |request| &request.session_id;
    AttachmentIntent => |request| &request.session_id;
}

/// One view forwarder per session-scoped entry; nothing for a catalog entry.
macro_rules! view_forwarder {
    ($component:ident [catalog] $($rest:tt)*) => {};
    (
        $component:ident [session $($carried:ident)*]
        $(#[$meta:meta])*
        fn $name:ident(&self, $session_id:ident: $session_ty:ty $(, $arg:ident: $arg_ty:ty)*) -> $ret:ty;
    ) => {
        #[doc = concat!(
            "[`", stringify!($component), "::", stringify!($name),
            "`] for this view's session."
        )]
        $(#[$meta])*
        pub async fn $name(&self $(, $arg: $arg_ty)*) -> $ret {
            $(self.check_request(&$carried)?;)*
            self.store.$name(&self.session_id $(, $arg)*).await
        }
    };
    (
        $component:ident [carried $($carried:ident),+]
        $(#[$meta:meta])*
        fn $name:ident(&self $(, $arg:ident: $arg_ty:ty)*) -> $ret:ty;
    ) => {
        #[doc = concat!(
            "[`", stringify!($component), "::", stringify!($name),
            "`], refusing a request for another session."
        )]
        $(#[$meta])*
        pub async fn $name(&self $(, $arg: $arg_ty)*) -> $ret {
            $(self.check_request(&$carried)?;)+
            self.store.$name($($arg),*).await
        }
    };
}

macro_rules! emit_session_view {
    ($(
        $component:ident {
            $(
                $(#[$meta:meta])*
                [$($scope:tt)*] fn $name:ident(&self $(, $arg:ident: $arg_ty:ty)*) -> $ret:ty;
            )*
            $(provided:
                $(
                    $(#[$pmeta:meta])*
                    [$($pscope:tt)*] fn $pname:ident(&self $(, $parg:ident: $parg_ty:ty)*) -> $pret:ty;
                )*
            )?
        }
    )*) => {
        impl SessionStore {
            $(
                $(
                    view_forwarder! {
                        $component [$($scope)*] $(#[$meta])* fn $name(&self $(, $arg: $arg_ty)*) -> $ret;
                    }
                )*
                $($(
                    view_forwarder! {
                        $component [$($pscope)*] $(#[$pmeta])* fn $pname(&self $(, $parg: $parg_ty)*) -> $pret;
                    }
                )*)?
            )*
        }

        /// The operations the view forwards (test builds only).
        #[cfg(test)]
        pub(crate) const SESSION_VIEW_OPERATIONS: &[&str] = &[
            $(
                $(stringify!($name),)*
                $($(stringify!($pname),)*)?
            )*
        ];
    };
}

// `SESSION_VIEW_OPERATIONS` lists every entry; the lint filters out the
// catalog ones by their scope marker.
super::runtime_store_decorator::runtime_store_operations!(emit_session_view);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_forwards_every_session_scoped_operation() {
        let listed = super::super::runtime_store_decorator::RUNTIME_STORE_OPERATIONS;
        assert_eq!(listed.len(), SESSION_VIEW_OPERATIONS.len());
        for operation in listed {
            assert!(
                ["session", "carried", "catalog"]
                    .iter()
                    .any(|scope| operation.scope.starts_with(scope)),
                "`{}` has no session scope marker",
                operation.name
            );
            if operation.scope.starts_with("session") {
                assert!(
                    operation
                        .params
                        .first()
                        .is_some_and(|param| param.replace(' ', "") == "&SessionId"),
                    "`{}` is marked `session` but does not take the session id first",
                    operation.name
                );
            }
        }
    }

    #[test]
    fn a_request_for_another_session_is_refused() {
        let view = SessionId::from("view");
        let meta = SessionMeta {
            session_id: SessionId::from("other"),
            relation: crate::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            owning_process_id: None,
        };
        assert!(matches!(
            meta.check_session(&view),
            Err(StoreError::ForeignSessionRequest { view_session_id, request_session_id })
                if view_session_id == view && request_session_id.as_str() == "other"
        ));
        assert!(meta.check_session(&SessionId::from("other")).is_ok());
    }
}
