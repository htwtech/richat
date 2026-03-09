use {
    crate::{
        channel::GlobalReplayFromSlot,
        config::{
            ConfigChannelSource, ConfigChannelSourceGeneral, ConfigChannelSourceReconnect,
            ConfigShmPreFilter,
        },
    },
    anyhow::Context as _,
    futures::{
        future::try_join_all,
        stream::{BoxStream, Stream, StreamExt, try_unfold},
    },
    richat_client::shm::{ShmConnectError, ShmEntryMeta, ShmSubscription},
    richat_filter::message::{Message, MessageParseError, MessageParserEncoding},
    std::{
        collections::HashSet,
        fmt,
        pin::Pin,
        sync::{LazyLock, Mutex},
        task::{Context, Poll},
    },
    thiserror::Error,
    tokio::time::{Duration, sleep},
    tracing::{error, info},
};

#[derive(Debug, Error)]
enum ConnectError {
    #[error(transparent)]
    Shm(ShmConnectError),
}

#[derive(Debug, Error)]
enum SubscribeError {
    #[error(transparent)]
    Connect(#[from] ConnectError),
}

#[derive(Debug, Error)]
pub enum ReceiveError {
    #[error(transparent)]
    Parse(#[from] MessageParseError),
    #[error("replay from the requested slot is not available from any source")]
    ReplayFailed,
}

/// Fast pre-filter using V2 SHM index metadata.
/// Decides whether to skip a message before reading data from the ring buffer.
struct ShmPreFilter {
    disable_accounts: bool,
    disable_transactions: bool,
    disable_entries: bool,
    exclude_votes: bool,
    exclude_failed: bool,
    account_pubkeys: HashSet<[u8; 32]>,
    account_owners: HashSet<[u8; 32]>,
    transaction_accounts: HashSet<[u8; 32]>,
}

impl ShmPreFilter {
    fn from_config(config: Option<ConfigShmPreFilter>) -> Self {
        let config = config.unwrap_or_default();
        Self {
            disable_accounts: config.disable_accounts,
            disable_transactions: config.disable_transactions,
            disable_entries: config.disable_entries,
            exclude_votes: config.exclude_votes,
            exclude_failed: config.exclude_failed,
            account_pubkeys: config
                .account_pubkeys
                .iter()
                .map(|pk| pk.to_bytes())
                .collect(),
            account_owners: config
                .account_owners
                .iter()
                .map(|pk| pk.to_bytes())
                .collect(),
            transaction_accounts: config
                .transaction_accounts
                .iter()
                .map(|pk| pk.to_bytes())
                .collect(),
        }
    }

    /// Returns true if the message should be accepted (i.e. NOT skipped).
    fn should_accept(&self, meta: &ShmEntryMeta) -> bool {
        use richat_shared::transports::shm::{
            FLAG_TX_FAILED, FLAG_TX_IS_VOTE, MSG_TYPE_ACCOUNT, MSG_TYPE_BLOCK_META,
            MSG_TYPE_ENTRY, MSG_TYPE_SLOT, MSG_TYPE_TRANSACTION,
        };

        match meta.msg_type {
            MSG_TYPE_ACCOUNT => {
                if self.disable_accounts {
                    return false;
                }
                if !self.account_pubkeys.is_empty() {
                    let pubkey: &[u8; 32] = meta.meta[0..32].try_into().unwrap();
                    if !self.account_pubkeys.contains(pubkey) {
                        return false;
                    }
                }
                if !self.account_owners.is_empty() {
                    let owner: &[u8; 32] = meta.meta[32..64].try_into().unwrap();
                    if !self.account_owners.contains(owner) {
                        return false;
                    }
                }
                true
            }
            MSG_TYPE_TRANSACTION => {
                if self.disable_transactions {
                    return false;
                }
                if self.exclude_votes && (meta.flags & FLAG_TX_IS_VOTE != 0) {
                    return false;
                }
                if self.exclude_failed && (meta.flags & FLAG_TX_FAILED != 0) {
                    return false;
                }
                if !self.transaction_accounts.is_empty() {
                    let bloom: &[u8; 32] = meta.meta[64..96].try_into().unwrap();
                    if !self.transaction_accounts.iter().any(|key| {
                        richat_shared::transports::shm::bloom256::may_contain(bloom, key)
                    }) {
                        return false;
                    }
                }
                true
            }
            MSG_TYPE_ENTRY => !self.disable_entries,
            MSG_TYPE_SLOT | MSG_TYPE_BLOCK_META => true,
            _ => true,
        }
    }

    /// Returns true if this filter does nothing (all defaults, no filtering).
    fn is_noop(&self) -> bool {
        !self.disable_accounts
            && !self.disable_transactions
            && !self.disable_entries
            && !self.exclude_votes
            && !self.exclude_failed
            && self.account_pubkeys.is_empty()
            && self.account_owners.is_empty()
            && self.transaction_accounts.is_empty()
    }
}

#[derive(Debug)]
struct Backoff {
    current_interval: Duration,
    initial_interval: Duration,
    max_interval: Duration,
    multiplier: f64,
}

impl Backoff {
    const fn new(config: ConfigChannelSourceReconnect) -> Self {
        Self {
            current_interval: config.initial_interval,
            initial_interval: config.initial_interval,
            max_interval: config.max_interval,
            multiplier: config.multiplier,
        }
    }

    async fn sleep(&mut self) {
        sleep(self.current_interval).await;
        self.current_interval = self
            .current_interval
            .mul_f64(self.multiplier)
            .min(self.max_interval);
    }

    const fn reset(&mut self) {
        self.current_interval = self.initial_interval;
    }
}

type SubscriptionMessage = Result<(&'static str, Message), ReceiveError>;

pub type PreparedReloadResult = anyhow::Result<(Vec<&'static str>, Vec<Subscription>)>;

pub struct Subscription {
    name: &'static str,
    config: ConfigChannelSource,
    stream: BoxStream<'static, SubscriptionMessage>,
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscription")
            .field("name", &self.name)
            .finish()
    }
}

impl Subscription {
    async fn new(
        source_config: ConfigChannelSource,
        global_replay_from_slot: GlobalReplayFromSlot,
    ) -> anyhow::Result<Self> {
        let name = Self::get_static_name(&source_config.general.name);
        let mut config = source_config.general.clone();

        let stream = if let Some(reconnect) = config.reconnect.take() {
            let backoff = Backoff::new(reconnect);
            try_unfold(
                (
                    backoff,
                    source_config.clone(),
                    config,
                    global_replay_from_slot,
                    None,
                ),
                move |mut state: (
                    Backoff,
                    ConfigChannelSource,
                    ConfigChannelSourceGeneral,
                    GlobalReplayFromSlot,
                    Option<kanal::AsyncReceiver<SubscriptionMessage>>,
                )| async move {
                    loop {
                        if let Some(stream) = state.4.as_mut() {
                            match stream.recv().await {
                                Ok(Ok((name, message))) => {
                                    return Ok(Some(((name, message), state)));
                                }
                                Ok(Err(ReceiveError::ReplayFailed)) => {
                                    if state.3.report_replay_failed(name) {
                                        return Err(ReceiveError::ReplayFailed);
                                    }
                                    error!(name, "failed to replay, waiting for other sources");
                                }
                                Ok(Err(error)) => {
                                    error!(name, ?error, "failed to receive")
                                }
                                Err(_) => {
                                    error!(name, "stream is finished")
                                }
                            }
                            state.4 = None;
                            state.0.sleep().await;
                        } else {
                            match Subscription::subscribe(
                                name,
                                &state.1,
                                state.2.parser,
                                state.2.channel_size,
                            )
                            .await
                            {
                                Ok(stream) => {
                                    state.4 = Some(stream);
                                    state.0.reset();
                                }
                                Err(error) => {
                                    error!(name, ?error, "failed to connect");
                                    state.0.sleep().await;
                                }
                            }
                        }
                    }
                },
            )
            .boxed()
        } else {
            let rx = Self::subscribe(
                name,
                &source_config,
                config.parser,
                config.channel_size,
            )
            .await?;
            futures::stream::unfold(
                rx,
                |rx| async move { rx.recv().await.ok().map(|msg| (msg, rx)) },
            )
            .boxed()
        };

        Ok(Self {
            name,
            config: source_config,
            stream,
        })
    }

    fn get_static_name(name: &str) -> &'static str {
        static NAMES: LazyLock<Mutex<HashSet<&'static str>>> =
            LazyLock::new(|| Mutex::new(HashSet::new()));
        let mut set = NAMES.lock().expect("poisoned");
        if let Some(&name) = set.get(name) {
            name
        } else {
            let name: &'static str = name.to_owned().leak();
            set.insert(name);
            name
        }
    }

    async fn subscribe(
        name: &'static str,
        source: &ConfigChannelSource,
        parser: MessageParserEncoding,
        channel_size: usize,
    ) -> Result<kanal::AsyncReceiver<SubscriptionMessage>, SubscribeError> {
        let sub = ShmSubscription::open(&source.config).map_err(ConnectError::Shm)?;
        let pf = ShmPreFilter::from_config(source.pre_filter.clone());
        let has_pre_filter = !pf.is_noop();
        info!(name, has_pre_filter, "attached to shared memory");
        let rx = sub.subscribe_filter_map_v2(
            channel_size,
            move |meta| pf.should_accept(meta),
            move |_meta, data| {
                match Message::parse(data.into(), parser) {
                    Ok(message) => Some(Ok((name, message))),
                    Err(MessageParseError::InvalidUpdateMessage("Ping")) => None,
                    Err(error) => Some(Err(error.into())),
                }
            },
        );
        info!(name, "subscribed");
        Ok(rx.to_async())
    }
}

pub struct Subscriptions {
    global_replay_from_slot: GlobalReplayFromSlot,
    streams: Vec<Subscription>,
    last_polled: usize,
}

impl fmt::Debug for Subscriptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscriptions")
            .field("streams", &self.streams.len())
            .field("last_polled", &self.last_polled)
            .finish()
    }
}

impl Subscriptions {
    pub async fn new(
        sources: Vec<ConfigChannelSource>,
        global_replay_from_slot: GlobalReplayFromSlot,
    ) -> anyhow::Result<Self> {
        let streams = Self::create_subscriptions(sources, &global_replay_from_slot).await?;

        Ok(Self {
            global_replay_from_slot,
            streams,
            last_polled: 0,
        })
    }

    async fn create_subscriptions(
        sources: impl IntoIterator<Item = ConfigChannelSource>,
        global_replay_from_slot: &GlobalReplayFromSlot,
    ) -> anyhow::Result<Vec<Subscription>> {
        try_join_all(sources.into_iter().map(|config| {
            let global_replay_from_slot = global_replay_from_slot.clone();
            async move {
                Subscription::new(config, global_replay_from_slot)
                    .await
                    .context("failed to subscribe")
            }
        }))
        .await
    }

    pub fn get_last_polled_name(&self) -> &'static str {
        self.streams[self.last_polled].name
    }

    /// Prepare reload by computing changes and creating subscriptions.
    /// Returns a future that resolves to names to remove and new streams to add.
    pub fn prepare_reload(
        &self,
        new_sources: Vec<ConfigChannelSource>,
    ) -> impl Future<Output = PreparedReloadResult> + use<> {
        // collect streams for removal
        let to_remove: Vec<&'static str> = self
            .streams
            .iter()
            .filter(|stream| {
                let matching = new_sources
                    .iter()
                    .find(|new_source_config| stream.name == new_source_config.name());
                match matching {
                    None => true,
                    Some(new_source_config) if &stream.config != new_source_config => true,
                    Some(_) => false,
                }
            })
            .map(|stream| stream.name)
            .collect();

        // collect sources to add
        let sources_to_add: Vec<ConfigChannelSource> = new_sources
            .into_iter()
            .filter(|new_source_config| {
                let name = Subscription::get_static_name(new_source_config.name());
                match self.streams.iter().find(|s| s.name == name) {
                    Some(stream) => &stream.config != new_source_config,
                    None => true,
                }
            })
            .collect();

        let global_replay_from_slot = self.global_replay_from_slot.clone();
        async move {
            Self::create_subscriptions(sources_to_add, &global_replay_from_slot)
                .await
                .map(|new_streams| (to_remove, new_streams))
        }
    }

    /// Apply the result of `prepare_reload` to update subscriptions.
    pub fn apply_reload(&mut self, to_remove: Vec<&'static str>, new_streams: Vec<Subscription>) {
        for name in to_remove {
            info!(name, "removing subscription");
            self.streams.retain(|stream| stream.name != name);
        }

        for stream in new_streams {
            info!(name = stream.name, "adding subscription");
            self.streams.push(stream);
        }

        self.global_replay_from_slot
            .update_sources(self.streams.len());

        self.last_polled = 0;
    }
}

impl Stream for Subscriptions {
    type Item = SubscriptionMessage;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let init_index = self.last_polled;
        loop {
            self.last_polled = (self.last_polled + 1) % self.streams.len();
            let index = self.last_polled;

            let result = self.streams[index].stream.poll_next_unpin(cx);
            if let Poll::Ready(value) = result {
                return Poll::Ready(value);
            }

            if index == init_index {
                return Poll::Pending;
            }
        }
    }
}
