use {
    crate::{channel::Messages, richat::config::ConfigAppsRichat},
    futures::future::{FutureExt, TryFutureExt, try_join_all},
    richat_shared::transports::shm::ShmServer,
    std::future::Future,
    tokio_util::sync::CancellationToken,
};

#[derive(Debug)]
pub struct RichatServer;

impl RichatServer {
    pub async fn spawn(
        config: ConfigAppsRichat,
        messages: Messages,
        shutdown: CancellationToken,
    ) -> anyhow::Result<impl Future<Output = anyhow::Result<()>>> {
        let mut tasks = Vec::with_capacity(1);

        // Start Shm
        if let Some(config) = config.shm {
            tasks.push(
                ShmServer::spawn(config, messages.clone(), shutdown.clone())
                    .await?
                    .boxed(),
            );
        }

        Ok(try_join_all(tasks).map_ok(|_| ()).map_err(Into::into))
    }
}
