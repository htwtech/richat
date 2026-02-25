use {
    crate::{
        channel::Sender,
        config::Config,
        metrics,
        protobuf::{ProtobufEncoder, ProtobufMessage},
        version::VERSION,
    },
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        GeyserPlugin, GeyserPluginError, ReplicaAccountInfoVersions, ReplicaBlockInfoVersions,
        ReplicaEntryInfoVersions, ReplicaTransactionInfoVersions, Result as PluginResult,
        SlotStatus as GeyserSlotStatus,
    },
    futures::future::BoxFuture,
    log::error,
    richat_metrics::{MaybeRecorder, gauge},
    richat_shared::transports::{
        grpc::GrpcServer,
        quic::QuicServer,
        shm::{
            ShmDirectWriter, ShmWriteMeta, bloom256,
            FLAG_ACC_EXECUTABLE, FLAG_ACC_HAS_TXN_SIG,
            FLAG_TX_FAILED, FLAG_TX_IS_VOTE,
            MSG_TYPE_ACCOUNT, MSG_TYPE_BLOCK_META, MSG_TYPE_ENTRY, MSG_TYPE_SLOT,
            MSG_TYPE_TRANSACTION,
        },
    },
    solana_clock::Slot,
    solana_message::VersionedMessage,
    std::{cell::RefCell, fmt, sync::Arc, time::Duration},
    tokio::{runtime::Runtime, task::JoinError},
    tokio_util::sync::CancellationToken,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginNotification {
    Slot,
    Account,
    Transaction,
    Entry,
    BlockMeta,
}

impl From<&ProtobufMessage<'_>> for PluginNotification {
    fn from(value: &ProtobufMessage<'_>) -> Self {
        match value {
            ProtobufMessage::Account { .. } => Self::Account,
            ProtobufMessage::Slot { .. } => Self::Slot,
            ProtobufMessage::Transaction { .. } => Self::Transaction,
            ProtobufMessage::Entry { .. } => Self::Entry,
            ProtobufMessage::BlockMeta { .. } => Self::BlockMeta,
        }
    }
}

struct PluginTask(BoxFuture<'static, Result<(), JoinError>>);

unsafe impl Sync for PluginTask {}

impl fmt::Debug for PluginTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginTask").finish()
    }
}

thread_local! {
    // 64 KiB initial capacity; grows as needed. After warmup, the buffer
    // stabilizes at the size of the largest message seen, avoiding further allocations.
    static ENCODE_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(64 * 1024));
}

#[derive(Debug)]
pub struct PluginInner {
    runtime: Runtime,
    messages: Option<Sender>,
    shm_direct: Option<Arc<ShmDirectWriter>>,
    encoder: ProtobufEncoder,
    shutdown: CancellationToken,
    shutdown_timeout_secs: u64,
    tasks: Vec<(&'static str, PluginTask)>,
}

impl PluginInner {
    fn dispatch(&self, message: ProtobufMessage) {
        match (&self.shm_direct, &self.messages) {
            // SHM-only: encode into thread-local buffer, write directly with V2 metadata
            (Some(shm), None) => {
                let slot = message.get_slot();
                let write_meta = Self::extract_shm_meta(&message);
                ENCODE_BUF.with(|buf| {
                    let mut buf = buf.borrow_mut();
                    message.encode_into(self.encoder, &mut buf);
                    shm.write(&buf, slot, &write_meta);
                });
            }
            // Channel-only (no SHM): existing path
            (None, Some(sender)) => {
                sender.push(message, self.encoder);
            }
            // Combined: encode once, write to SHM with V2 metadata, push pre-encoded to channel
            (Some(shm), Some(sender)) => {
                let slot = message.get_slot();
                let write_meta = Self::extract_shm_meta(&message);
                ENCODE_BUF.with(|buf| {
                    let mut buf = buf.borrow_mut();
                    message.encode_into(self.encoder, &mut buf);
                    shm.write(&buf, slot, &write_meta);
                    sender.push_pre_encoded(message, buf.to_vec(), self.encoder);
                });
            }
            (None, None) => {}
        }
    }

    /// Extract metadata from a ProtobufMessage for the V2 SHM index entry.
    fn extract_shm_meta(message: &ProtobufMessage) -> ShmWriteMeta {
        let mut wm = ShmWriteMeta::default();
        match message {
            ProtobufMessage::Account { account, .. } => {
                wm.msg_type = MSG_TYPE_ACCOUNT;
                if account.executable {
                    wm.flags |= FLAG_ACC_EXECUTABLE;
                }
                if account.txn.is_some() {
                    wm.flags |= FLAG_ACC_HAS_TXN_SIG;
                }
                // pubkey → meta[0..32]
                let pk_len = account.pubkey.len().min(32);
                wm.meta[..pk_len].copy_from_slice(&account.pubkey[..pk_len]);
                // owner → meta[32..64]
                let ow_len = account.owner.len().min(32);
                wm.meta[32..32 + ow_len].copy_from_slice(&account.owner[..ow_len]);
                // lamports → meta[64..72]
                wm.meta[64..72].copy_from_slice(&account.lamports.to_le_bytes());
            }
            ProtobufMessage::Transaction { transaction, .. } => {
                wm.msg_type = MSG_TYPE_TRANSACTION;
                if transaction.is_vote {
                    wm.flags |= FLAG_TX_IS_VOTE;
                }
                if transaction.transaction_status_meta.status.is_err() {
                    wm.flags |= FLAG_TX_FAILED;
                }
                // signature → meta[0..64]
                let sig = transaction.signature.as_ref();
                let sig_len = sig.len().min(64);
                wm.meta[..sig_len].copy_from_slice(&sig[..sig_len]);
                // account bloom filter → meta[64..96]
                let mut bloom = [0u8; 32];
                let account_keys = match &transaction.transaction.message {
                    VersionedMessage::Legacy(msg) => &msg.account_keys,
                    VersionedMessage::V0(msg) => &msg.account_keys,
                };
                for key in account_keys {
                    bloom256::insert(&mut bloom, key.as_ref().try_into().unwrap());
                }
                for key in &transaction.transaction_status_meta.loaded_addresses.writable {
                    bloom256::insert(&mut bloom, key.as_ref().try_into().unwrap());
                }
                for key in &transaction.transaction_status_meta.loaded_addresses.readonly {
                    bloom256::insert(&mut bloom, key.as_ref().try_into().unwrap());
                }
                wm.meta[64..96].copy_from_slice(&bloom);
            }
            ProtobufMessage::Slot {
                status, parent, ..
            } => {
                wm.msg_type = MSG_TYPE_SLOT;
                wm.flags = Self::slot_status_to_u8(status);
                if let Some(p) = parent {
                    wm.meta[0..8].copy_from_slice(&p.to_le_bytes());
                }
            }
            ProtobufMessage::Entry { entry } => {
                wm.msg_type = MSG_TYPE_ENTRY;
                wm.meta[0..8].copy_from_slice(&(entry.index as u64).to_le_bytes());
                wm.meta[8..16]
                    .copy_from_slice(&entry.executed_transaction_count.to_le_bytes());
            }
            ProtobufMessage::BlockMeta { .. } => {
                wm.msg_type = MSG_TYPE_BLOCK_META;
            }
        }
        wm
    }

    fn slot_status_to_u8(status: &GeyserSlotStatus) -> u8 {
        match status {
            GeyserSlotStatus::Processed => 0,
            GeyserSlotStatus::Confirmed => 1,
            GeyserSlotStatus::Rooted => 2,
            GeyserSlotStatus::FirstShredReceived => 3,
            GeyserSlotStatus::Completed => 4,
            GeyserSlotStatus::CreatedBank => 5,
            GeyserSlotStatus::Dead(_) => 6,
        }
    }

    fn new(config: Config) -> PluginResult<Self> {
        let (metrics_recorder, metrics_handle) = if config.metrics.is_some() {
            let recorder = metrics::setup();
            let handle = recorder.handle();
            (Arc::new(recorder.into()), Some(handle))
        } else {
            (Arc::new(MaybeRecorder::Noop), None)
        };

        // Create Tokio runtime
        let runtime = config
            .tokio
            .build_runtime("richatPlugin")
            .map_err(|error| GeyserPluginError::Custom(Box::new(error)))?;

        // Determine if we need the channel (for gRPC/QUIC subscribers)
        let needs_channel = config.grpc.is_some() || config.quic.is_some();

        // Create messages store only if needed
        let messages = if needs_channel {
            Some(Sender::new(config.channel, Arc::clone(&metrics_recorder)))
        } else {
            None
        };

        // Create SHM direct writer if configured
        let shm_direct = if config.shm.is_some() {
            Some(Arc::new(
                ShmDirectWriter::new(config.shm.as_ref().unwrap())
                    .map_err(|error| GeyserPluginError::Custom(Box::new(error)))?,
            ))
        } else {
            None
        };

        // Spawn servers
        let (messages, shutdown, tasks) = runtime
            .block_on(async move {
                let shutdown = CancellationToken::new();
                let mut tasks = Vec::with_capacity(4);

                // Start gRPC
                if let Some(config) = config.grpc {
                    let sender = messages.as_ref()
                        .ok_or_else(|| anyhow::anyhow!("channel required for gRPC but was not created"))?;
                    let connections_inc = gauge!(&metrics_recorder, metrics::CONNECTIONS_TOTAL, "transport" => "grpc");
                    let connections_dec = connections_inc.clone();
                    tasks.push((
                        "gRPC Server",
                        PluginTask(Box::pin(
                            GrpcServer::spawn(
                                config,
                                sender.clone(),
                                move || connections_inc.increment(1), // on_conn_new_cb
                                move || connections_dec.decrement(1), // on_conn_drop_cb
                                VERSION,
                                shutdown.clone(),
                            )
                            .await?,
                        )),
                    ));
                }

                // Start Quic
                if let Some(config) = config.quic {
                    let sender = messages.as_ref()
                        .ok_or_else(|| anyhow::anyhow!("channel required for QUIC but was not created"))?;
                    let connections_inc = gauge!(&metrics_recorder, metrics::CONNECTIONS_TOTAL, "transport" => "quic");
                    let connections_dec = connections_inc.clone();
                    tasks.push((
                        "Quic Server",
                        PluginTask(Box::pin(
                            QuicServer::spawn(
                                config,
                                sender.clone(),
                                move || connections_inc.increment(1), // on_conn_new_cb
                                move || connections_dec.decrement(1), // on_conn_drop_cb
                                VERSION,
                                shutdown.clone(),
                            )
                            .await?,
                        )),
                    ));
                }

                // Start Shm via ShmServer only when NOT using ShmDirectWriter
                // (i.e. this path is no longer taken since we use ShmDirectWriter)
                // Kept for reference but the shm config is now handled above.

                // Start prometheus server
                if let (Some(config), Some(metrics_handle)) = (config.metrics, metrics_handle) {
                    tasks.push((
                        "Prometheus Server",
                        PluginTask(Box::pin(
                            metrics::spawn_server(config, metrics_handle, shutdown.clone().cancelled_owned()).await?,
                        )),
                    ));
                }

                Ok::<_, anyhow::Error>((messages, shutdown, tasks))
            })
            .map_err(|error| GeyserPluginError::Custom(format!("{error:?}").into()))?;

        Ok(Self {
            runtime,
            messages,
            shm_direct,
            encoder: config.channel.encoder,
            shutdown,
            shutdown_timeout_secs: config.shutdown_timeout_secs,
            tasks,
        })
    }
}

#[derive(Debug, Default)]
pub struct Plugin {
    inner: Option<PluginInner>,
}

impl Plugin {
    fn get_inner(&self) -> PluginResult<&PluginInner> {
        self.inner
            .as_ref()
            .ok_or_else(|| GeyserPluginError::Custom("plugin not initialized".into()))
    }
}

impl GeyserPlugin for Plugin {
    fn name(&self) -> &'static str {
        concat!(env!("CARGO_PKG_NAME"), "-", env!("CARGO_PKG_VERSION"))
    }

    fn on_load(&mut self, config_file: &str, _is_reload: bool) -> PluginResult<()> {
        solana_logger::setup_with_default("info");
        let config = Config::load_from_file(config_file).inspect_err(|error| {
            error!("failed to load config: {error:?}");
        })?;

        // Setup logger from the config
        solana_logger::setup_with_default(&config.logs.level);

        // Create inner
        self.inner = Some(PluginInner::new(config).inspect_err(|error| {
            error!("failed to load plugin from the config: {error:?}");
        })?);

        Ok(())
    }

    fn on_unload(&mut self) {
        if let Some(inner) = self.inner.take() {
            if let Some(ref messages) = inner.messages {
                messages.close();
            }
            if let Some(ref shm) = inner.shm_direct {
                shm.close();
            }

            inner.shutdown.cancel();
            inner.runtime.block_on(async {
                for (name, task) in inner.tasks {
                    if let Err(error) = task.0.await {
                        error!("failed to join `{name}` task: {error:?}");
                    }
                }
            });

            if let Some(ref shm) = inner.shm_direct {
                shm.remove_file();
            }

            inner.runtime.shutdown_timeout(Duration::from_secs(inner.shutdown_timeout_secs));
        }
    }

    fn update_account(
        &self,
        account: ReplicaAccountInfoVersions,
        slot: u64,
        is_startup: bool,
    ) -> PluginResult<()> {
        if !is_startup {
            let account = match account {
                ReplicaAccountInfoVersions::V0_0_1(_info) => {
                    return Err(GeyserPluginError::Custom(
                        "ReplicaAccountInfoVersions::V0_0_1 is not supported".into(),
                    ));
                }
                ReplicaAccountInfoVersions::V0_0_2(_info) => {
                    return Err(GeyserPluginError::Custom(
                        "ReplicaAccountInfoVersions::V0_0_2 is not supported".into(),
                    ));
                }
                ReplicaAccountInfoVersions::V0_0_3(info) => info,
            };

            let inner = self.get_inner()?;
            inner.dispatch(ProtobufMessage::Account { slot, account });
        }

        Ok(())
    }

    fn notify_end_of_startup(&self) -> PluginResult<()> {
        Ok(())
    }

    fn update_slot_status(
        &self,
        slot: Slot,
        parent: Option<u64>,
        status: &GeyserSlotStatus,
    ) -> PluginResult<()> {
        let inner = self.get_inner()?;
        inner.dispatch(ProtobufMessage::Slot {
            slot,
            parent,
            status,
        });

        Ok(())
    }

    fn notify_transaction(
        &self,
        transaction: ReplicaTransactionInfoVersions<'_>,
        slot: u64,
    ) -> PluginResult<()> {
        let transaction = match transaction {
            ReplicaTransactionInfoVersions::V0_0_1(_info) => {
                return Err(GeyserPluginError::Custom(
                    "ReplicaTransactionInfoVersions::V0_0_1 is not supported".into(),
                ));
            }
            ReplicaTransactionInfoVersions::V0_0_2(_info) => {
                return Err(GeyserPluginError::Custom(
                    "ReplicaTransactionInfoVersions::V0_0_2 is not supported".into(),
                ));
            }
            ReplicaTransactionInfoVersions::V0_0_3(info) => info,
        };

        let inner = self.get_inner()?;
        inner.dispatch(ProtobufMessage::Transaction { slot, transaction });

        Ok(())
    }

    fn notify_entry(&self, entry: ReplicaEntryInfoVersions) -> PluginResult<()> {
        #[allow(clippy::infallible_destructuring_match)]
        let entry = match entry {
            ReplicaEntryInfoVersions::V0_0_1(_entry) => {
                return Err(GeyserPluginError::Custom(
                    "ReplicaEntryInfoVersions::V0_0_1 is not supported".into(),
                ));
            }
            ReplicaEntryInfoVersions::V0_0_2(entry) => entry,
        };

        let inner = self.get_inner()?;
        inner.dispatch(ProtobufMessage::Entry { entry });

        Ok(())
    }

    fn notify_block_metadata(&self, blockinfo: ReplicaBlockInfoVersions<'_>) -> PluginResult<()> {
        let blockinfo = match blockinfo {
            ReplicaBlockInfoVersions::V0_0_1(_info) => {
                return Err(GeyserPluginError::Custom(
                    "ReplicaBlockInfoVersions::V0_0_1 is not supported".into(),
                ));
            }
            ReplicaBlockInfoVersions::V0_0_2(_info) => {
                return Err(GeyserPluginError::Custom(
                    "ReplicaBlockInfoVersions::V0_0_2 is not supported".into(),
                ));
            }
            ReplicaBlockInfoVersions::V0_0_3(_info) => {
                return Err(GeyserPluginError::Custom(
                    "ReplicaBlockInfoVersions::V0_0_3 is not supported".into(),
                ));
            }
            ReplicaBlockInfoVersions::V0_0_4(info) => info,
        };

        let inner = self.get_inner()?;
        inner.dispatch(ProtobufMessage::BlockMeta { blockinfo });

        Ok(())
    }

    fn account_data_notifications_enabled(&self) -> bool {
        true
    }

    fn account_data_snapshot_notifications_enabled(&self) -> bool {
        false
    }

    fn transaction_notifications_enabled(&self) -> bool {
        true
    }

    fn entry_notifications_enabled(&self) -> bool {
        true
    }
}

#[cfg(feature = "plugin")]
#[unsafe(no_mangle)]
#[allow(improper_ctypes_definitions)]
/// # Safety
///
/// This function returns the Plugin pointer as trait GeyserPlugin.
pub unsafe extern "C" fn _create_plugin() -> *mut dyn GeyserPlugin {
    #[cfg(feature = "rustls-install-default-provider")]
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to call CryptoProvider::install_default()");

    let plugin = Plugin::default();
    let plugin: Box<dyn GeyserPlugin> = Box::new(plugin);
    Box::into_raw(plugin)
}
