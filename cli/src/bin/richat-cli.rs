use {
    clap::{Parser, Subcommand},
    richat_cli::pubsub::ArgsAppPubSub,
    std::sync::atomic::{AtomicU64, Ordering},
};

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Debug, Parser)]
#[clap(author, version, about = "Richat Cli Tool: pubsub")]
struct Args {
    #[command(subcommand)]
    action: ArgsAppSelect,
}

#[derive(Debug, Subcommand)]
enum ArgsAppSelect {
    /// Subscribe on updates over WebSocket (Solana PubSub)
    Pubsub(ArgsAppPubSub),
}

async fn main2() -> anyhow::Result<()> {
    anyhow::ensure!(
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .is_ok(),
        "failed to call CryptoProvider::install_default()"
    );

    richat_shared::tracing::setup(false)?;

    let args = Args::parse();
    match args.action {
        ArgsAppSelect::Pubsub(action) => action.run().await,
    }
}

fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .thread_name_fn(move || {
            static ATOMIC_ID: AtomicU64 = AtomicU64::new(0);
            let id = ATOMIC_ID.fetch_add(1, Ordering::Relaxed);
            format!("richatCli{id:02}")
        })
        .enable_all()
        .build()?
        .block_on(main2())
}
