use {
    criterion::Criterion,
    richat_benches::fixtures::generate_transactions,
    richat_filter::message::{Message, MessageParserEncoding},
    richat_plugin_agave::protobuf::{ProtobufEncoder, ProtobufMessage},
    richat_shared::transports::shm::bloom256,
    solana_message::VersionedMessage,
    solana_pubkey::Pubkey,
    std::{borrow::Cow, hint::black_box, time::SystemTime},
};

/// Prepared transaction: bloom filter + encoded protobuf bytes.
struct PreparedTx {
    bloom: [u8; 32],
    encoded: Vec<u8>,
}

pub fn bench_bloom_prefilter(criterion: &mut Criterion) {
    let transactions = generate_transactions();
    let created_at = SystemTime::now();

    // Build bloom filters and encode transactions (same as plugin dispatch path)
    let prepared: Vec<PreparedTx> = transactions
        .iter()
        .map(|tx| {
            let (slot, replica) = tx.to_replica();

            // Build bloom filter from account keys (mirrors extract_shm_meta)
            let mut bloom = [0u8; 32];
            let account_keys = match &replica.transaction.message {
                VersionedMessage::Legacy(msg) => &msg.account_keys,
                VersionedMessage::V0(msg) => &msg.account_keys,
            };
            for key in account_keys {
                bloom256::insert(&mut bloom, key.as_ref().try_into().unwrap());
            }
            for key in &replica.transaction_status_meta.loaded_addresses.writable {
                bloom256::insert(&mut bloom, key.as_ref().try_into().unwrap());
            }
            for key in &replica.transaction_status_meta.loaded_addresses.readonly {
                bloom256::insert(&mut bloom, key.as_ref().try_into().unwrap());
            }

            // Encode as Raw protobuf
            let msg = ProtobufMessage::Transaction {
                slot,
                transaction: &replica,
            };
            let encoded = msg.encode_with_timestamp(ProtobufEncoder::Raw, created_at);

            PreparedTx { bloom, encoded }
        })
        .collect();

    // Filter key that does NOT exist in any transaction → 100% bloom skip
    let missing_key: [u8; 32] = [0xFF; 32];

    // Filter key from the first transaction → partial match
    let first_account_key: [u8; 32] = {
        let (_slot, replica) = transactions[0].to_replica();
        let keys = match &replica.transaction.message {
            VersionedMessage::Legacy(msg) => &msg.account_keys,
            VersionedMessage::V0(msg) => &msg.account_keys,
        };
        let pk: &Pubkey = &keys[0];
        pk.to_bytes()
    };

    let blooms: Vec<&[u8; 32]> = prepared.iter().map(|p| &p.bloom).collect();

    let mut group = criterion.benchmark_group("bloom_prefilter");

    // 1. Pure bloom check — hot path for non-matching transactions
    group.bench_function("bloom_check_only/miss", |b| {
        b.iter(|| {
            for bloom in &blooms {
                black_box(bloom256::may_contain(bloom, &missing_key));
            }
        })
    });

    group.bench_function("bloom_check_only/hit", |b| {
        b.iter(|| {
            for bloom in &blooms {
                black_box(bloom256::may_contain(bloom, &first_account_key));
            }
        })
    });

    // 2. Full parse baseline — parse protobuf + check account keys
    group.bench_function("full_parse_skip/miss", |b| {
        b.iter(|| {
            for p in &prepared {
                let msg = Message::parse(
                    Cow::Borrowed(p.encoded.as_slice()),
                    MessageParserEncoding::Limited,
                )
                .unwrap();
                if let Message::Transaction(tx) = &msg {
                    black_box(tx.account_keys().contains(&Pubkey::from(missing_key)));
                }
            }
        })
    });

    group.bench_function("full_parse_skip/hit", |b| {
        b.iter(|| {
            for p in &prepared {
                let msg = Message::parse(
                    Cow::Borrowed(p.encoded.as_slice()),
                    MessageParserEncoding::Limited,
                )
                .unwrap();
                if let Message::Transaction(tx) = &msg {
                    black_box(
                        tx.account_keys()
                            .contains(&Pubkey::from(first_account_key)),
                    );
                }
            }
        })
    });

    // 3. Bloom pre-filter → skip or parse
    group.bench_function("bloom_then_parse/miss", |b| {
        b.iter(|| {
            for p in &prepared {
                if bloom256::may_contain(&p.bloom, &missing_key) {
                    let msg = Message::parse(
                        Cow::Borrowed(p.encoded.as_slice()),
                        MessageParserEncoding::Limited,
                    )
                    .unwrap();
                    if let Message::Transaction(tx) = &msg {
                        black_box(tx.account_keys().contains(&Pubkey::from(missing_key)));
                    }
                } else {
                    black_box(false);
                }
            }
        })
    });

    group.bench_function("bloom_then_parse/hit", |b| {
        b.iter(|| {
            for p in &prepared {
                if bloom256::may_contain(&p.bloom, &first_account_key) {
                    let msg = Message::parse(
                        Cow::Borrowed(p.encoded.as_slice()),
                        MessageParserEncoding::Limited,
                    )
                    .unwrap();
                    if let Message::Transaction(tx) = &msg {
                        black_box(
                            tx.account_keys()
                                .contains(&Pubkey::from(first_account_key)),
                        );
                    }
                } else {
                    black_box(false);
                }
            }
        })
    });

    group.finish();
}
