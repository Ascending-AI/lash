use super::*;
use std::{collections::VecDeque, time::Instant};
use tokio::sync::watch;

/// The independent tool event window of an in-process replay store.
#[derive(Debug)]
pub struct ToolLiveMemoryWindow {
    state: Arc<Mutex<HashMap<String, Window>>>,
    retention: ToolLiveRetention,
}
#[derive(Debug)]
struct Window {
    incarnation: String,
    position: u64,
    bytes: usize,
    events: VecDeque<(u64, Instant, usize, ToolLiveRecord)>,
    bell: watch::Sender<u64>,
    touched: Instant,
}
impl Window {
    fn new() -> Self {
        Self {
            incarnation: uuid::Uuid::new_v4().to_string(),
            position: 0,
            bytes: 0,
            events: VecDeque::new(),
            bell: watch::channel(0).0,
            touched: Instant::now(),
        }
    }
    fn trim(&mut self, policy: ToolLiveRetention) {
        while self
            .events
            .front()
            .is_some_and(|(_, at, _, _)| at.elapsed() > policy.age)
            || self.events.len() > policy.events
            || self.bytes > policy.bytes
        {
            if let Some((_, _, bytes, _)) = self.events.pop_front() {
                self.bytes -= bytes;
            } else {
                break;
            }
        }
    }
    fn read(
        &mut self,
        owner: &ExecutionOwner,
        after: Option<&ToolLiveCursor>,
        retention: ToolLiveRetention,
    ) -> Vec<ToolLiveItem> {
        self.trim(retention);
        self.touched = Instant::now();
        let floor = self
            .events
            .front()
            .map_or(self.position, |(position, ..)| position - 1);
        let cursor = |position| ToolLiveCursor {
            owner: owner.clone(),
            incarnation: self.incarnation.clone(),
            position,
        };
        let mut items = Vec::new();
        let position = match after {
            Some(after)
                if after.incarnation != self.incarnation || after.position > self.position =>
            {
                items.push(ToolLiveItem::Gap {
                    cursor: cursor(floor),
                    reason: ToolLiveGapReason::Unavailable,
                });
                floor
            }
            Some(after) if after.position < floor => {
                items.push(ToolLiveItem::Gap {
                    cursor: cursor(floor),
                    reason: ToolLiveGapReason::Trimmed,
                });
                floor
            }
            Some(after) => after.position,
            None => {
                if floor != 0 {
                    items.push(ToolLiveItem::Gap {
                        cursor: cursor(floor),
                        reason: ToolLiveGapReason::Trimmed,
                    });
                }
                floor
            }
        };
        items.extend(
            self.events
                .iter()
                .filter(|(at, ..)| *at > position)
                .map(|(at, _, _, record)| record.clone().into_item(cursor(*at))),
        );
        items
    }
}
impl Default for ToolLiveMemoryWindow {
    fn default() -> Self {
        Self::new(ToolLiveRetention::default())
    }
}
impl ToolLiveMemoryWindow {
    pub fn new(retention: ToolLiveRetention) -> Self {
        Self {
            state: Arc::default(),
            retention,
        }
    }
    fn key(owner: &ExecutionOwner) -> Result<String, ToolLiveError> {
        serde_json::to_string(owner).map_err(ToolLiveError::corrupt)
    }
    fn ensure<'a>(&self, state: &'a mut HashMap<String, Window>, key: String) -> &'a mut Window {
        state.retain(|_, window| {
            window.touched.elapsed() <= self.retention.age || window.bell.receiver_count() != 0
        });
        if !state.contains_key(&key)
            && state.len() >= self.retention.owners
            && let Some(oldest) = state
                .iter()
                .min_by_key(|(_, window)| window.touched)
                .map(|(key, _)| key.clone())
        {
            state.remove(&oldest);
        }
        state.entry(key).or_insert_with(Window::new)
    }
}
#[async_trait::async_trait]
impl ToolLiveReplay for ToolLiveMemoryWindow {
    async fn publish(
        &self,
        owner: &ExecutionOwner,
        records: Vec<ToolLiveRecord>,
    ) -> Result<(), ToolLiveError> {
        let key = Self::key(owner)?;
        let mut state = self.state.lock_recover();
        let window = self.ensure(&mut state, key);
        for record in records {
            let bytes = serde_json::to_vec(&record)
                .map_err(ToolLiveError::corrupt)?
                .len();
            window.position += 1;
            window.bytes += bytes;
            window
                .events
                .push_back((window.position, Instant::now(), bytes, record));
        }
        window.touched = Instant::now();
        window.trim(self.retention);
        window.bell.send_replace(window.position);
        Ok(())
    }
    async fn subscribe(
        &self,
        owner: &ExecutionOwner,
        after: Option<ToolLiveCursor>,
    ) -> Result<ToolLiveSubscription, ToolLiveError> {
        if after.as_ref().is_some_and(|cursor| &cursor.owner != owner) {
            return Err(ToolLiveError::WrongOwner);
        }
        let key = Self::key(owner)?;
        let (receiver, initial) = {
            let mut state = self.state.lock_recover();
            let window = self.ensure(&mut state, key.clone());
            let receiver = window.bell.subscribe();
            let initial = window.read(owner, after.as_ref(), self.retention);
            (receiver, initial)
        };
        let state = self.state.clone();
        let retention = self.retention;
        let owner = owner.clone();
        Ok(futures_util::stream::unfold(
            (receiver, VecDeque::from(initial), after, false),
            move |(mut receiver, mut queue, mut after, ended)| {
                let state = state.clone();
                let owner = owner.clone();
                let key = key.clone();
                async move {
                    if ended {
                        return None;
                    }
                    loop {
                        if let Some(item) = queue.pop_front() {
                            after = Some(item.cursor().clone());
                            return Some((Ok(item), (receiver, queue, after, false)));
                        }
                        if receiver.changed().await.is_err() {
                            return Some((
                                Err(ToolLiveError::NotificationLost),
                                (receiver, queue, after, true),
                            ));
                        }
                        let mut state = state.lock_recover();
                        let Some(window) = state.get_mut(&key) else {
                            return Some((
                                Err(ToolLiveError::NotificationLost),
                                (receiver, queue, after, true),
                            ));
                        };
                        queue.extend(window.read(&owner, after.as_ref(), retention));
                    }
                }
            },
        )
        .boxed())
    }
}
