# Полное ревью `plugin-agave` и `richat`
Дата: 2026-03-08
Область: `d:\Proj\richat\plugin-agave`, `d:\Proj\richat\richat`

## Краткий итог
- Выявлены критичные риски по памяти/задержкам в gRPC delivery path.
- Найдена потенциальная логическая ошибка в `blockhash`-валидности.
- Выявлены security-долги по дефолтным лимитам/аутентификации.
- Подготовлен приоритизированный план действий (P0→P3).

---

## 1) Критичные проблемы (исправлять в первую очередь)

### 1.1 Неограниченный рост очереди сообщений у gRPC-подписчика
**Симптом:** очередь клиента (`messages`) растет без жесткого live-ограничения по байтам.  
**Почему это критично:** при медленных клиентах возможен рост RAM и резкий рост latency доставки.

**Код:**
- `richat/src/grpc/server.rs:373`
- `richat/src/grpc/server.rs:411`
- `richat/src/grpc/server.rs:896`
- `richat/src/grpc/server.rs:911`

**Деталь:**
- Ограничение `messages_len_max` используется только для объема, который worker собрал за тик (`encoded_len`), но не ограничивает уже накопленную очередь клиента.
- `is_full()`/`is_full_replay()` в live path практически не применяются для hard-stop.

---

### 1.2 Неограниченная очередь в storage serializer path
**Симптом:** `kanal::unbounded()` для записи в storage.  
**Почему это критично:** при slowdown RocksDB или всплесках трафика очередь может расти без границ.

**Код:**
- `richat/src/storage.rs:151`
- `richat/src/storage.rs:516`

---

### 1.3 Потенциально неверная cleanup-логика blockhash validity
**Симптом:** retain-предикат в `BlockMetaStorage` выглядит инвертированным.  
**Почему это критично:** может приводить к некорректным ответам `is_blockhash_valid`.

**Код:**
- `richat/src/grpc/block_meta.rs:84`

---

## 2) Высокий приоритет (перфоманс и надежность доставки)

### 2.1 Off-by-one/visibility в sync recv path
**Симптом:** `try_recv` использует `if head < tail`, а часть потребителей стартует с `tail + 1`.  
**Риск:** лишняя задержка на последнем сообщении до появления следующего.

**Код:**
- `richat/src/channel.rs:951`
- `richat/src/grpc/server.rs:263`

---

### 2.2 Busy-spin по умолчанию в gRPC workers
**Симптом:** `ticks_without_messages_max` по умолчанию `None -> usize::MAX`.  
**Риск:** пустые или малонагруженные контуры могут зря крутить CPU.

**Код:**
- `richat/src/grpc/config.rs:35`
- `richat/src/grpc/server.rs:154`
- `richat/src/grpc/server.rs:427`

---

### 2.3 Слабые security-дефолты
**Симптом:** при пустом `x_tokens` аутентификация отключена; лимиты фильтров по умолчанию широкие (`usize::MAX`, `any=true`).  
**Риск:** легко организовать дорогие подписки/DoS на публичном контуре.

**Код:**
- `richat/src/grpc/server.rs:168`
- `filter/src/config.rs:170`
- `filter/src/config.rs:304`
- `filter/src/config.rs:414`

---

## 3) Средний приоритет (качество фильтрации, техдолг, эксплуатация)

### 3.1 SHM pre-filter для `transaction_accounts` дает false-positive как финальный результат
**Симптом:** используется только Bloom pre-check (`may_contain`) без точной пост-проверки перед выдачей.  
**Эффект:** клиент может получать лишние транзакции (логическая неточность фильтра).

**Код:**
- `richat/src/source.rs:183`
- `richat/src/source.rs:431`

---

### 3.2 Lock contention в `plugin-agave` channel path
**Симптом:** state lock удерживается вокруг дополнительной логики backfill по слотам и доп. encode.  
**Эффект:** under load увеличивает tail latency в dispatch.

**Код:**
- `plugin-agave/src/channel.rs:87`
- `plugin-agave/src/channel.rs:121`
- `plugin-agave/src/channel.rs:124`

---

### 3.3 Потенциально очень большой memory footprint в PubSub defaults
**Симптом:** высокие лимиты кэша сообщений + `VecDeque::with_capacity(max_count + 1)`.  
**Эффект:** высокий риск избыточного memory reservation/pressure.

**Код:**
- `richat/src/pubsub/config.rs:62`
- `richat/src/pubsub/notification.rs:92`

---

### 3.4 Высокая кардинальность метрик по `x_subscription_id`
**Симптом:** label берется напрямую из клиентского заголовка.  
**Эффект:** explode cardinality в Prometheus (RAM/CPU в мониторинге).

**Код:**
- `richat/src/grpc/server.rs:446`
- `richat/src/pubsub/server.rs:131`

---

### 3.5 Техдолг: утечка строк имен источников при reload
**Симптом:** `name.to_owned().leak()` для intern источников.  
**Эффект:** bounded, но накопительный leak при большом количестве уникальных имен.

**Код:**
- `richat/src/source.rs:363`

---

### 3.6 Safety debt: `unsafe impl Sync for PluginTask`
**Симптом:** ручной `unsafe impl Sync` без явного safety-contract в коде.  
**Эффект:** повышенный риск регрессий при рефакторинге.

**Код:**
- `plugin-agave/src/plugin.rs:58`

---

## 4) Влияние на задержки доставки gRPC-подписчикам

Основные источники задержек:
1. Накопление per-client очереди без жесткого дропа/дисконнекта (latency растет вместе с backlog).
2. Дополнительная сериализация/encode на каждое совпадение фильтра в worker path.
3. Contention и scheduling effects при множестве клиентов с тяжелыми фильтрами.
4. Replay path может конкурировать за CPU/lock, увеличивая tail latency live-клиентов.

---

## 5) Технический долг (сводка)

1. Отсутствие единой стратегии backpressure и drop-policy в gRPC live path.
2. Часть критичных путей зависит от `expect/unwrap` и паникообразного поведения.
3. Пограничные логические условия (head/tail, cleanup) не защищены явными regression-тестами.
4. Security posture сильно зависит от ручной настройки конфигурации, а не безопасных дефолтов.

---

## 6) Долги безопасности (сводка)

1. Возможная публичная работа без auth при `x_tokens = []`.
2. Слишком permissive default limits для фильтров подписок.
3. Потенциальный DoS через дорогие блок/аккаунт-подписки и cardinality-атаки на метрики.
4. Нет явного hard quota на потребление ресурсов на клиента в live push path.

---

## 7) Полный план действий

## P0 (немедленно)
1. Внедрить **жесткий byte-cap** per gRPC client queue в live path:
   - при превышении: controlled disconnect (`DataLoss/ResourceExhausted`) или drop-policy;
   - добавить метрики: queue bytes, queue drops, disconnects by reason.
2. Перевести storage ingestion queue с `unbounded` на bounded + явный backpressure.
3. Исправить cleanup predicate в `BlockMetaStorage` и покрыть тестами `is_blockhash_valid`.
4. Исправить head/tail boundary в sync recv и проверить отсутствие дополнительной idle-задержки.

## P1 (в ближайший спринт)
1. Исправить idle behavior workers (убрать дефолтный spin, включить wait/backoff по умолчанию).
2. Ужесточить security-дефолты:
   - mandatory auth для non-loopback;
   - безопасные default filter limits;
   - лимит clients/subscriptions.
3. Ограничить cardinality метрик:
   - нормализовать/хешировать `x_subscription_id`;
   - rate-limit/limit labels.

## P2 (производительность и корректность фильтрации)
1. Для SHM `transaction_accounts` сделать двухступенчатую фильтрацию:
   - Bloom pre-check (быстрый skip),
   - точная проверка account set после parse перед отправкой.
2. Переработать `plugin-agave` channel lock section:
   - вынести тяжелую логику и encode из критической секции.
3. Оптимизировать replay/live isolation, чтобы replay-клиенты меньше влияли на live tail latency.

## P3 (техдолг/поддерживаемость)
1. Убрать `unsafe impl Sync for PluginTask` или документировать инварианты + добавить тесты.
2. Убрать `String::leak` для source names, заменить на bounded interner/owned map.
3. Пройтись по `expect/unwrap` в runtime-коде и заменить на контролируемые ошибки.
4. Добавить regression/perf тесты:
   - lag/queue growth;
   - burst subscribers;
   - replay + live concurrency;
   - blockhash validity lifecycle.

---

## 8) Рекомендуемый порядок внедрения
1. P0.1 + P0.2 (очереди и backpressure).
2. P0.3 + P0.4 (корректность blockhash и boundary).
3. P1 security hardening.
4. P2 latency/perf оптимизации.
5. P3 cleanup и долговая программа.

---

## 9) Ограничения проверки

- В текущем окружении не удалось запустить `cargo check` (`cargo` отсутствует в PATH), поэтому выводы основаны на детальном статическом ревью кода.
