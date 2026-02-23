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
        SlotStatus,
    },
    futures::future::BoxFuture,
    log::error,
    richat_metrics::{MaybeRecorder, gauge},
    richat_shared::transports::{grpc::GrpcServer, quic::QuicServer, shm::ShmDirectWriter},
    solana_clock::Slot,
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
    static ENCODE_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(64 * 1024));
}

#[derive(Debug)]
pub struct PluginInner {
    runtime: Runtime,
    messages: Option<Sender>,
    shm_direct: Option<Arc<ShmDirectWriter>>,
    encoder: ProtobufEncoder,
    shutdown: CancellationToken,
    tasks: Vec<(&'static str, PluginTask)>,
}

impl PluginInner {
    fn dispatch(&self, message: ProtobufMessage) {
        match (&self.shm_direct, &self.messages) {
            // SHM-only: encode into thread-local buffer, write directly
            (Some(shm), None) => {
                let slot = message.get_slot();
                ENCODE_BUF.with(|buf| {
                    let mut buf = buf.borrow_mut();
                    message.encode_into(self.encoder, &mut buf);
                    shm.write(&buf, slot);
                });
            }
            // Channel-only (no SHM): existing path
            (None, Some(sender)) => {
                sender.push(message, self.encoder);
            }
            // Combined: encode once, write to SHM, push pre-encoded to channel
            (Some(shm), Some(sender)) => {
                let slot = message.get_slot();
                ENCODE_BUF.with(|buf| {
                    let mut buf = buf.borrow_mut();
                    message.encode_into(self.encoder, &mut buf);
                    shm.write(&buf, slot);
                    sender.push_pre_encoded(message, buf.to_vec(), self.encoder);
                });
            }
            (None, None) => {}
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
                    let sender = messages.as_ref().expect("channel created for grpc");
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
                    let sender = messages.as_ref().expect("channel created for quic");
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
            tasks,
        })
    }
}

#[derive(Debug, Default)]
pub struct Plugin {
    inner: Option<PluginInner>,
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

            inner.runtime.shutdown_timeout(Duration::from_secs(10));
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
                    unreachable!("ReplicaAccountInfoVersions::V0_0_1 is not supported")
                }
                ReplicaAccountInfoVersions::V0_0_2(_info) => {
                    unreachable!("ReplicaAccountInfoVersions::V0_0_2 is not supported")
                }
                ReplicaAccountInfoVersions::V0_0_3(info) => info,
            };

            let inner = self.inner.as_ref().expect("initialized");
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
        status: &SlotStatus,
    ) -> PluginResult<()> {
        let inner = self.inner.as_ref().expect("initialized");
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
                unreachable!("ReplicaAccountInfoVersions::V0_0_1 is not supported")
            }
            ReplicaTransactionInfoVersions::V0_0_2(_info) => {
                unreachable!("ReplicaAccountInfoVersions::V0_0_2 is not supported")
            }
            ReplicaTransactionInfoVersions::V0_0_3(info) => info,
        };

        let inner = self.inner.as_ref().expect("initialized");
        inner.dispatch(ProtobufMessage::Transaction { slot, transaction });

        Ok(())
    }

    fn notify_entry(&self, entry: ReplicaEntryInfoVersions) -> PluginResult<()> {
        #[allow(clippy::infallible_destructuring_match)]
        let entry = match entry {
            ReplicaEntryInfoVersions::V0_0_1(_entry) => {
                unreachable!("ReplicaEntryInfoVersions::V0_0_1 is not supported")
            }
            ReplicaEntryInfoVersions::V0_0_2(entry) => entry,
        };

        let inner = self.inner.as_ref().expect("initialized");
        inner.dispatch(ProtobufMessage::Entry { entry });

        Ok(())
    }

    fn notify_block_metadata(&self, blockinfo: ReplicaBlockInfoVersions<'_>) -> PluginResult<()> {
        let blockinfo = match blockinfo {
            ReplicaBlockInfoVersions::V0_0_1(_info) => {
                unreachable!("ReplicaBlockInfoVersions::V0_0_1 is not supported")
            }
            ReplicaBlockInfoVersions::V0_0_2(_info) => {
                unreachable!("ReplicaBlockInfoVersions::V0_0_2 is not supported")
            }
            ReplicaBlockInfoVersions::V0_0_3(_info) => {
                unreachable!("ReplicaBlockInfoVersions::V0_0_3 is not supported")
            }
            ReplicaBlockInfoVersions::V0_0_4(info) => info,
        };

        let inner = self.inner.as_ref().expect("initialized");
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
