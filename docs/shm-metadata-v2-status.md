# SHM Metadata V2 — Статус проекта

Ветка: `shm-metadata-v2`
Последний коммит: `24393c7 SHM V2 metadata index + zero-parse pre-filtering`

---

## Что сделано

### 1. SHM V2 формат индекса (shared/src/transports/shm.rs)

Расширили index entry с 32B (V1) до 128B (V2). Добавили inline-метаданные:

- `ShmIndexEntryV2` (128 байт): seq, data_off, data_len, **msg_type, flags, slot, meta[96]**
- `ShmWriteMeta` — структура для передачи метаданных при записи
- `bloom256` модуль — 256-bit bloom filter (k=5) для account keys транзакций
- `VERSION_V2 = 2`, `INDEX_ENTRY_SIZE_V2 = 128`
- `ShmDirectWriter::new()` теперь создаёт V2 файл
- `ShmDirectWriter::write()` принимает `&ShmWriteMeta` и заполняет V2 entry
- Header: добавлено поле `index_entry_size` (32 или 128)
- Accessor-методы на `ShmIndexEntryV2`: `account_pubkey()`, `account_owner()`, `account_lamports()`, `tx_signature()`, `tx_account_bloom()`, `slot_parent()`, `entry_index()`, `entry_exec_tx_count()`
- Тесты: V2 read/write (account, transaction), bloom insert/contains/no-false-negatives/empty

### 2. Плагин: extract_shm_meta (plugin-agave/src/plugin.rs)

Метод `PluginInner::extract_shm_meta()` извлекает метаданные из `ProtobufMessage`:

| Тип | meta[0..32] | meta[32..64] | meta[64..96] |
|-----|------------|-------------|-------------|
| Account | pubkey | owner | lamports (8B) |
| Transaction | signature (64B) | bloom filter (32B) |
| Slot | parent (8B) | — | — |
| Entry | index (8B) | exec_tx_count (8B) | — |
| BlockMeta | — | — | — |

Bloom filter для транзакций строится из всех аккаунтов: static keys + loaded writable + loaded readonly.

### 3. Бенчмарк bloom-prefilter (benches/benches/bloom-prefilter/)

**Новые файлы:**
- `benches/benches/bloom-prefilter/main.rs`
- `benches/benches/bloom-prefilter/transaction.rs`

**Добавлено в benches/Cargo.toml:**
- `richat-shared = { workspace = true }` (dev-dependency)
- `[[bench]] name = "bloom-prefilter" harness = false`

**Результаты (tds2, cargo bench):**

```
bloom_prefilter/bloom_check_only/miss:      65 ns
bloom_prefilter/bloom_check_only/hit:       72 ns
bloom_prefilter/full_parse_skip/miss:    89 µs
bloom_prefilter/full_parse_skip/hit:     90 µs
bloom_prefilter/bloom_then_parse/miss:  4.3 µs
bloom_prefilter/bloom_then_parse/hit:   836 ns
```

Bloom pre-filter: **~1370× быстрее** чистой проверки vs full parse, **~20× на потоке** с учётом false positives.

### 4. Документация (docs/)

- `docs/architecture-shm-v2.md` — Полное описание архитектуры (RU): плагин → SHM → клиент, структура файла, bloom filter, пре-фильтр, адаптивный backoff
- `docs/grpc-subscription-load-ranking.md` — Ранжирование gRPC подписок по нагрузке на ноду (RU), рекомендации по тарификации

---

## Что НЕ сделано / следующие шаги

### Код — нужен reader-side (клиент)

1. **`client/src/shm.rs` — `subscribe_filter_map_v2()`**
   Метод существует, но нужно убедиться что он использует V2 метаданные для пре-фильтра. Проверить что `try_read_next_v2()` читает `ShmIndexEntryV2` и вызывает `pre_filter` до чтения данных.

2. **`richat/src/source.rs` — `ShmPreFilter`**
   Структура для пре-фильтрации на стороне reader. Нужно проверить что:
   - `transaction_accounts` фильтр использует `bloom256::may_contain()`
   - `account_pubkeys` / `account_owners` проверяются по meta[0..32] / meta[32..64]
   - `exclude_votes` / `exclude_failed` проверяют flags

3. **`richat/src/config.rs` — `ConfigShmPreFilter`**
   Конфиг для SHM пре-фильтра. Нужно проверить что все поля маппятся на reader-side фильтрацию.

### Тестирование

4. **Интеграционное тестирование на tds2**
   - Запустить плагин с SHM V2 → richat-server с пре-фильтром → gRPC клиент
   - Проверить что данные проходят корректно
   - Проверить что фильтрация работает (bloom skip + exact match)

5. **Тест обратной совместимости**
   - Убедиться что V1 reader не падает на V2 файле (должен проверять version)
   - Или явно отклонять V2 если reader не поддерживает

### Оптимизация

6. **Bloom false positive rate**
   - Для транзакций с >30 аккаунтами bloom может быть перенасыщен
   - Рассмотреть увеличение до 512-bit если FP rate > 5%
   - Написать бенч/тест для оценки FP rate на реальных данных

7. **Конфиг gRPC лимитов для продакшена**
   - Настроить `filter_limits` с `any: false` для accounts/transactions/blocks
   - Ограничить `blocks.max: 0` или `blocks.account_include_any: false`
   - Настроить `account_max`, `owner_max`, `account_include_max`

### Документация

8. **README update** — добавить описание V2 формата и пре-фильтра
9. **Конфиг примеры** — добавить примеры конфигов для разных тарифных планов

---

## Файлы затронутые в этой ветке

### Изменённые (tracked, uncommitted)
```
M  benches/Cargo.toml                    — добавлен richat-shared dep + bloom-prefilter bench target
M  plugin-agave/src/plugin.rs            — extract_shm_meta(), dispatch() с V2 meta
M  richat/src/config.rs                  — ConfigShmPreFilter с transaction_accounts
M  richat/src/source.rs                  — ShmPreFilter bloom check
M  shared/src/transports/shm.rs          — ShmIndexEntryV2, bloom256, ShmWriteMeta, VERSION_V2
```

### Новые (untracked)
```
?? benches/benches/bloom-prefilter/main.rs
?? benches/benches/bloom-prefilter/transaction.rs
?? docs/architecture-shm-v2.md
?? docs/grpc-subscription-load-ranking.md
?? docs/shm-metadata-v2-status.md
```

### Статус сборки
- `cargo check --bench bloom-prefilter` — OK на tds2
- `cargo bench --bench bloom-prefilter` — OK на tds2, результаты выше
- `cargo check` / `cargo test` / `cargo build --release` — нужно перепроверить (последняя проверка была до бенчмарка)

---

## Ключевые решения принятые

1. **V2 entry = 128 байт** (не 64, не 256) — баланс между размером индекса и количеством inline-метаданных
2. **Bloom 256-bit, k=5** — компактный (32 байта), быстрый (~5 нс), достаточно для 5-30 аккаунтов
3. **Хэш = u32 из raw bytes pubkey** — не криптографический, но быстрый и с хорошим распределением для pubkey
4. **meta[96] layout по типу** — фиксированная интерпретация, без тегов (тип определяется по msg_type)
5. **Пре-фильтр ДО чтения данных** — главная оптимизация: для 99% транзакций при узком фильтре данные вообще не читаются
6. **ShmDirectWriter всегда V2** — V1 остаётся только для ShmServer (async path, legacy)
