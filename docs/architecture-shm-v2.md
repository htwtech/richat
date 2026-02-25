# Richat: Как работает связка Плагин → SHM → Клиент

## Общая картина

```
┌─────────────────────┐
│   Solana Validator   │  ← блокчейн-нода, генерирует данные
│   (agave)            │
└────────┬────────────┘
         │  Geyser Plugin Interface
         │  (колбэки: аккаунт изменился, транзакция пришла, слот обновился...)
         ▼
┌─────────────────────────────────────────────────────┐
│              richat-plugin-agave                     │
│                                                     │
│  ProtobufMessage → encode → dispatch()              │
│       │                                             │
│       ├─── SHM-only ──→ ShmDirectWriter ──→ файл    │
│       │                  (прямая запись)    /dev/shm │
│       │                                             │
│       ├─── Channel-only → Sender ──→ gRPC / QUIC   │
│       │                  (broadcast)                │
│       │                                             │
│       └─── Combined ───→ SHM + Sender (одна         │
│                          кодировка на оба пути)     │
└─────────────────────────────────────────────────────┘
         │                           │
         ▼                           ▼
┌─────────────────┐        ┌─────────────────┐
│  SHM файл       │        │  gRPC / QUIC    │
│  /dev/shm/richat│        │  сервер         │
└────────┬────────┘        └────────┬────────┘
         │                          │
         ▼                          ▼
┌─────────────────┐        ┌─────────────────┐
│  richat-server   │        │  Удалённые      │
│  (SHM reader)    │        │  клиенты        │
│  + пре-фильтр    │        │  (gRPC/QUIC)    │
└─────────────────┘        └─────────────────┘
```

---

## 1. Откуда берутся данные

Solana validator (agave) — это нода блокчейна. Она обрабатывает транзакции, обновляет аккаунты, создаёт блоки. Для каждого события она вызывает **колбэки Geyser Plugin Interface**:

| Событие | Колбэк | Что внутри |
|---|---|---|
| Аккаунт изменился | `update_account()` | pubkey, owner, lamports, data, слот |
| Пришла транзакция | `notify_transaction()` | подпись, аккаунты, статус, голосование/нет |
| Слот обновился | `update_slot_status()` | номер слота, статус (processed/confirmed/finalized) |
| Entry (часть блока) | `notify_entry()` | слот, индекс, хэш, кол-во транзакций |
| Метаданные блока | `notify_block_metadata()` | слот, blockhash, rewards, высота |

Каждое из этих событий — это **колбэк из Solana напрямую в наш плагин**. Они приходят из разных потоков валидатора.

---

## 2. Плагин: что делает с данными

Плагин (`plugin-agave/src/plugin.rs`) получает колбэк и делает три вещи:

### Шаг 1: Создаёт ProtobufMessage

Из сырых данных Solana создаётся enum:
```
ProtobufMessage::Account { slot, account }
ProtobufMessage::Transaction { slot, transaction }
ProtobufMessage::Slot { slot, parent, status }
ProtobufMessage::Entry { entry }
ProtobufMessage::BlockMeta { blockinfo }
```

### Шаг 2: Кодирует в protobuf-байты

Есть два кодировщика:
- **Raw** — ручная кодировка, быстрее (используются конкретные номера полей protobuf)
- **Prost** — через библиотеку prost (стандартный protobuf)

Оба дают одинаковый результат, Raw просто быстрее.

Кодирование происходит в **thread-local буфер** `ENCODE_BUF` — это `Vec<u8>`, который выделяется один раз (начальный размер 64 КБ) и потом переиспользуется. Каждый вызов делает `buf.clear()` (сбрасывает длину, но ёмкость остаётся) и кодирует в тот же буфер. Итог: **ноль аллокаций после прогрева**.

### Шаг 3: Dispatch — рассылка по транспортам

Метод `dispatch()` — центральная точка. Он работает в одном из трёх режимов:

#### Режим 1: SHM-only (только общая память)

```
encode_into(buf)          ← закодировать в thread-local буфер
extract_shm_meta(message) ← извлечь метаданные для V2 индекса
shm.write(buf, slot, meta)← записать в ring buffer
```

Используется когда в конфиге есть SHM, но нет gRPC/QUIC.
Самый быстрый путь — 0 аллокаций, 1 mutex lock, 0 каналов.

#### Режим 2: Channel-only (только gRPC/QUIC)

```
sender.push(message, encoder) ← кодировать и положить в broadcast канал
```

Из канала данные забирают gRPC и QUIC серверы и рассылают клиентам.

#### Режим 3: Combined (SHM + gRPC/QUIC)

```
encode_into(buf)                        ← закодировать ОДИН раз
shm.write(buf, slot, meta)              ← записать в SHM
sender.push_pre_encoded(msg, buf.clone())← отправить в канал те же байты
```

Ключевая оптимизация — **кодирование происходит только один раз**, результат идёт и в SHM, и в канал.

---

## 3. SHM: структура файла в общей памяти

Файл `/dev/shm/richat` — это обычный файл, замапленный в память через `mmap`. Он разделён на три зоны:

```
┌──────────────────────────────────────────────────────────────────┐
│                        SHM файл (~15 ГБ)                         │
│                                                                  │
│  ┌──────────┐ ┌───────────────────────────┐ ┌─────────────────┐ │
│  │  Header   │ │      Index Ring            │ │   Data Ring      │ │
│  │  64 байта │ │  N × 128 байт (V2)        │ │   N байт         │ │
│  └──────────┘ └───────────────────────────┘ └─────────────────┘ │
│                                                                  │
│  offset 0     offset 64      offset 64 + N×128                  │
└──────────────────────────────────────────────────────────────────┘
```

### 3.1. Header (64 байта)

| Поле | Размер | Описание |
|---|---|---|
| `magic` | 8 | Сигнатура `"RICHATSH"` — для проверки что это наш файл |
| `version` | 8 | 1 (V1) или 2 (V2, с метаданными) |
| `idx_capacity` | 8 | Кол-во слотов индекса (степень 2, напр. 2 097 152) |
| `data_capacity` | 8 | Размер data ring в байтах (степень 2, напр. 15 ГиБ) |
| `tail` | 8 | Атомарный счётчик: сколько сообщений записано (монотонно растёт) |
| `data_tail` | 8 | Атомарный: текущая позиция записи в data ring |
| `closed` | 8 | Атомарный флаг: 0 = работает, 1 = закрыт |
| `index_entry_size` | 8 | 32 (V1) или 128 (V2) |

### 3.2. Index Ring — кольцевой буфер индексов

Это массив из `idx_capacity` записей. Каждая запись (V2) = 128 байт:

```
┌─────────────────────────────────────────────────────────────┐
│                  ShmIndexEntryV2 (128 байт)                 │
├──────────────┬──────────────────────────────────────────────┤
│ Базовая часть│  seq (8)  - позиция коммита (-1 = пусто)    │
│ (32 байта)   │  data_off (8) - смещение в data ring        │
│              │  data_len (4) - длина данных                 │
│              │  msg_type (1) - тип сообщения                │
│              │  flags (1)    - битовые флаги                │
│              │  _pad (2)                                    │
│              │  slot (8)     - номер слота Solana           │
├──────────────┼──────────────────────────────────────────────┤
│ Метаданные   │  meta[0..96] - зависят от типа сообщения    │
│ (96 байт)    │  (см. таблицу ниже)                         │
└──────────────┴──────────────────────────────────────────────┘
```

**msg_type** — какое сообщение:
| Значение | Тип |
|---|---|
| 0 | Slot |
| 1 | Account |
| 2 | Transaction |
| 3 | Entry |
| 4 | BlockMeta |

**flags** — битовые флаги:
| Тип | Бит 0 | Бит 1 | Бит 2 |
|---|---|---|---|
| Transaction | is_vote | failed | — |
| Account | executable | has_txn_sig | is_startup |

**meta[96]** — метаданные, зависящие от типа:

```
Account:
  meta[0..32]   = pubkey       (32 байта — адрес аккаунта)
  meta[32..64]  = owner        (32 байта — владелец аккаунта)
  meta[64..72]  = lamports     (8 байт — баланс)
  meta[72..96]  = padding

Transaction:
  meta[0..64]   = signature    (64 байта — подпись транзакции)
  meta[64..96]  = bloom filter (32 байта — блум-фильтр аккаунтов)

Slot:
  meta[0..8]    = parent slot  (8 байт — родительский слот)
  meta[8..96]   = padding

Entry:
  meta[0..8]    = index        (8 байт — индекс entry в блоке)
  meta[8..16]   = exec_tx_count (8 байт — кол-во исполненных транзакций)
  meta[16..96]  = padding

BlockMeta:
  meta[0..96]   = padding      (метаданные не нужны)
```

### 3.3. Data Ring — кольцевой буфер данных

Здесь лежат **сами protobuf-байты** сообщений. Это кольцо: когда достигаем конца, пишем с начала (с переносом).

```
data_off = 0                                        data_capacity
   ▼                                                      ▼
   ┌──────┬──────┬──────┬──────┬ ... ─────┬──────┬────────┐
   │ msg0 │ msg1 │ msg2 │ msg3 │          │ msgN │ пусто  │
   └──────┴──────┴──────┴──────┴──────────┴──────┴────────┘

   При переполнении: данные оборачиваются с начала
   Старые данные перезаписываются
```

### 3.4. Как пишется одно сообщение

```
1. Взять мьютекс (единственный mutex на все записи)
2. Скопировать protobuf-байты в data ring по адресу data_pos % data_capacity
   (с обработкой переноса если достигли конца)
3. Заполнить V2 index entry:
   - data_off, data_len, msg_type, flags, slot, meta[96]
4. Обновить header.data_tail (Relaxed ordering)
5. КОММИТ: entry.seq = текущая позиция (Release ordering)
   ↑ Это момент когда читатель УВИДИТ запись
6. Увеличить позицию, обновить header.tail (Release ordering)
7. Отпустить мьютекс
```

Ключевой момент — **шаг 5**: пока `seq` не обновлён, читатель не видит запись. Release ordering гарантирует, что все предыдущие записи (данные, метаданные) видны читателю к моменту, когда он прочитает `seq`.

---

## 4. Bloom-фильтр: как он работает

### Зачем нужен

Типичный сценарий: клиент хочет получать только транзакции, которые затрагивают конкретный аккаунт (например, кошелёк пользователя). Без bloom-фильтра нужно:
1. Прочитать ~КБ protobuf-данных из data ring
2. Распарсить protobuf (дорого: ~90 µs на пачку)
3. Проверить каждый аккаунт в транзакции

С bloom-фильтром:
1. Прочитать 32 байта из index entry → **~65 нс**
2. Проверить bloom → если "нет" — **пропустить**, не читая данные вообще

### Как устроен

Bloom-фильтр — 256 бит (32 байта). Для каждого аккаунта в транзакции:

```
Аккаунт = 32 байта pubkey

Вычисляем 5 хэшей (k=5):
  h0 = u32_from_le_bytes(key[0..4])  % 256  → бит номер h0
  h1 = u32_from_le_bytes(key[4..8])  % 256  → бит номер h1
  h2 = u32_from_le_bytes(key[8..12]) % 256  → бит номер h2
  h3 = u32_from_le_bytes(key[12..16])% 256  → бит номер h3
  h4 = u32_from_le_bytes(key[16..20])% 256  → бит номер h4

Вставка: установить все 5 бит в 1
Проверка: если ВСЕ 5 бит = 1 → "возможно есть" (may_contain)
           если ХОТЯ БЫ ОДИН = 0 → "точно нет"
```

**Свойства:**
- **Ложных отрицаний НЕТ** — если аккаунт есть в транзакции, bloom всегда ответит "да"
- **Ложные срабатывания ВОЗМОЖНЫ** — bloom может сказать "может быть", хотя аккаунта нет. Тогда мы парсим protobuf и проверяем точно.
- На практике для транзакций с 5-30 аккаунтами false positive rate низкий.

### Что попадает в bloom

При записи транзакции плагин вставляет в bloom ВСЕ аккаунты:
- Статические аккаунты из тела транзакции (`account_keys`)
- Динамические аккаунты из address lookup tables (`loaded_addresses.writable` + `readonly`)

---

## 5. Клиент: как читает из SHM

### 5.1. Подключение

Клиент (`client/src/shm.rs`) делает:
1. Открывает файл `/dev/shm/richat` (read-only)
2. Делает `mmap` (read-only) — теперь видит ту же память что и писатель
3. Проверяет `magic` = "RICHATSH"
4. Читает `version`, `idx_capacity`, `data_capacity`, `index_entry_size`
5. Вычисляет смещения: где начинается index ring, где data ring

### 5.2. Цикл чтения (V2 с пре-фильтром)

```
next_pos = начальная позиция (обычно header.tail - некоторый offset)

loop:
  1. Загрузить header.tail (Acquire ordering)
  2. Если next_pos >= tail → данных нет, ждём (адаптивный backoff)
  3. Если header.closed == 1 → писатель закрылся, выход

  4. Вычислить idx = next_pos % idx_capacity
  5. Прочитать index entry[idx]:
     - Проверить seq == next_pos (Acquire) → запись готова?
     - Если seq != next_pos → мы отстали (lag), переподписка

  6. Извлечь метаданные:  msg_type, flags, slot, meta[96]

  ──── ПРЕ-ФИЛЬТР (до чтения данных!) ────

  7. Вызвать pre_filter(meta):
     - Тип Account и disable_accounts? → ПРОПУСТИТЬ
     - Тип Transaction и exclude_votes и is_vote? → ПРОПУСТИТЬ
     - Тип Transaction и есть фильтр по аккаунту?
       → bloom256::may_contain(bloom, target_key) == false? → ПРОПУСТИТЬ
     - ... другие фильтры

  8. Если пре-фильтр отклонил → next_pos++, goto 1
     ДАННЫЕ ВООБЩЕ НЕ ЧИТАЮТСЯ ИЗ DATA RING!

  ──── ЧТЕНИЕ ДАННЫХ ────

  9. Если пре-фильтр принял:
     - Скопировать data_len байт из data ring[data_off % data_capacity]
     - Перепроверить seq (не перезаписали ли?) → если перезаписали → lag

  10. Распарсить protobuf: Message::parse(data, Limited)
  11. Обработать сообщение (отправить клиенту, обработать фильтр и т.д.)
  12. next_pos++, goto 1
```

### 5.3. Адаптивный backoff (когда данных нет)

Когда читатель догнал писателя (`next_pos >= tail`), он не должен бесконечно крутить CPU:

```
Фаза 1 (spin):    1-64 раза pause instruction (~5 мкс)
Фаза 2 (yield):   4 раза thread::yield_now()
Фаза 3 (sleep):   sleep(1 мкс)

Когда данные появляются → сброс на фазу 1
```

### 5.4. Обнаружение отставания (lag)

Если читатель слишком медленный и писатель перезаписал слот в индексе:
```
seq в index entry стал != next_pos
```
Это значит писатель обогнал на полный круг. Читатель обнаруживает это и **переподписывается** — начинает с актуальной позиции.

---

## 6. Пре-фильтр: какие фильтры доступны

Конфигурация (`richat/src/config.rs`):

```json
{
  "sources": [{
    "shm": {
      "path": "/dev/shm/richat",
      "pre_filter": {
        "disable_accounts": false,
        "disable_transactions": false,
        "disable_entries": true,
        "exclude_votes": true,
        "exclude_failed": false,
        "account_pubkeys": ["PubkeyAAA..."],
        "account_owners": ["PubkeyBBB..."],
        "transaction_accounts": ["PubkeyCCC..."]
      }
    }
  }]
}
```

Что проверяется ДО чтения данных (по V2 метаданным):

| Фильтр | Тип сообщения | Что проверяет | Где в мета |
|---|---|---|---|
| `disable_accounts` | Account | Пропускать все аккаунты | msg_type |
| `disable_transactions` | Transaction | Пропускать все транзакции | msg_type |
| `disable_entries` | Entry | Пропускать все entries | msg_type |
| `exclude_votes` | Transaction | Пропускать голосования | flags & 0x01 |
| `exclude_failed` | Transaction | Пропускать неудачные | flags & 0x02 |
| `account_pubkeys` | Account | Только указанные pubkey | meta[0..32] |
| `account_owners` | Account | Только указанные owner | meta[32..64] |
| `transaction_accounts` | Transaction | Bloom-проверка аккаунта | meta[64..96] |

---

## 7. Результаты бенчмарка

Наш бенчмарк показал реальную разницу:

```
bloom_check_only/miss:      65 нс    ← чистая проверка блума (пачка ~14 транзакций)
bloom_check_only/hit:       72 нс
full_parse_skip/miss:    89 000 нс   ← полный парсинг protobuf + проверка
full_parse_skip/hit:     90 000 нс
bloom_then_parse/miss:    4 300 нс   ← bloom → skip (без чтения данных)
bloom_then_parse/hit:       836 нс   ← bloom match → парсим только нужные
```

**Вывод:** bloom пре-фильтр ускоряет фильтрацию транзакций в **~1370x** на уровне чистой проверки и **~20-107x** на уровне потока с учётом false positives.

---

## 8. Полная схема потока данных

```
  Solana Validator
        │
        │  колбэки (update_account, notify_transaction, ...)
        ▼
  ┌─────────────────────────────────────────────────────┐
  │  richat-plugin-agave                                │
  │                                                     │
  │  1. Создать ProtobufMessage из данных колбэка       │
  │  2. extract_shm_meta() → V2 метаданные:            │
  │     • msg_type, flags, slot                         │
  │     • Account: pubkey + owner + lamports            │
  │     • Transaction: signature + bloom filter         │
  │  3. encode_into(ENCODE_BUF) → protobuf-байты        │
  │  4. ShmDirectWriter::write():                       │
  │     • mutex lock                                    │
  │     • copy data → data ring                         │
  │     • fill index entry (128 байт V2)                │
  │     • seq.store(Release) ← КОММИТ                   │
  │     • mutex unlock                                  │
  └──────────────────┬──────────────────────────────────┘
                     │
          ┌──────────┴──────────┐
          ▼                     ▼
   /dev/shm/richat       Sender (broadcast)
   [Header 64B]          ┌─── gRPC Server
   [Index  N×128B]       └─── QUIC Server
   [Data   ~15GB]
          │
          │  mmap (read-only)
          ▼
  ┌─────────────────────────────────────────────────────┐
  │  richat-server (или другой SHM клиент)              │
  │                                                     │
  │  Reader loop:                                       │
  │    1. tail = header.tail.load(Acquire)              │
  │    2. idx_entry = index[pos % capacity]             │
  │    3. Прочитать msg_type, flags, meta               │
  │    4. ПРЕ-ФИЛЬТР:                                   │
  │       ├─ disable_accounts? → skip                   │
  │       ├─ exclude_votes? → skip                      │
  │       ├─ bloom::may_contain(bloom, key)?            │
  │       │   └─ false → SKIP (0 байт данных прочитано!)│
  │       │   └─ true → нужен парсинг                   │
  │       └─ account_pubkey match? → meta[0..32]        │
  │    5. Если принято: copy data из data ring           │
  │    6. Message::parse(data, Limited) → полный парсинг │
  │    7. Отправить клиенту через gRPC/pubsub            │
  └─────────────────────────────────────────────────────┘
```

Главное преимущество V2 с bloom: для **не-интересных** транзакций (а таких ~99% при фильтре по одному аккаунту) мы не читаем килобайты protobuf-данных из data ring вообще — достаточно проверить 32 байта bloom-фильтра прямо в index entry.
