use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;
/// One logical run, as an engine's control verbs address it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RunRef {
    pub session: SessionId,
    pub run: TurnId,
}
