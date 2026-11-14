//! 行情存档：把每次成功拉到的数据落两份盘。
//!
//! - **FlowDB**（嵌入式 LSM 时序引擎）：按 `quote:<品种>` / `intraday:<品种>` /
//!   `daily:<品种>` 分组，记录的 `ts` 是行情自身的时间（微秒），值是该时点的 JSON。
//!   适合按品种前缀 + 时间范围扫描，画历史曲线或做离线分析。
//! - **SQLite**（`hq.sqlite3`）：同样的数据拆成四张表，主键带品种，重复写同一条
//!   只会覆盖，适合直接用 SQL 查。
//!
//! 两者都只收**网络取到**的数据：缓存回退的那份上一轮已经存过了，再写一遍只是
//! 白白放大存储。
//!
//! 存档是旁路：磁盘满、文件被占用都不应该影响看板本身，所以写失败只记下来
//! 供界面提示（[`Archive::last_error`]），不向外抛错。
//!
//! # 时间口径
//!
//! 接口给的日期/时间没有时区信息（交易所当地时间），两边都按 **UTC** 解释，
//! 仅用于排序与去重，不做跨时区换算。两个库的单位不一样，各有各的理由：
//!
//! - **FlowDB** 存微秒：引擎的块级时间桶索引（`time_bucket_secs`）按微秒折算，
//!   这是它的原生精度。
//! - **SQLite** 存 **epoch 秒**：SQLite 自带的 `datetime(ts, 'unixepoch')` /
//!   `date(ts, 'unixepoch')` 只认秒，微秒会直接返回 `NULL`，第三方工具也默认
//!   按秒（或毫秒/纳秒）解释。行情数据本身只到秒级，没有精度损失。
//! - **日 K 不存时间戳，存 `date TEXT`（`YYYY-MM-DD`，接口原样）**：一根日 K 的
//!   身份是"哪个交易日"，不是某个时刻。ISO 日期字符串可直接比较、排序、建索引，
//!   也免得给交易日强加一个 UTC 午夜的时间点。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use flowdb::{Config as FlowConfig, Engine, Record};
use rusqlite::{params, Connection};

use crate::cache::Cache;
use crate::feed::{Candle, Instrument, MinutePoint, Quote, Quotes, INSTRUMENTS};

/// 存档目录：`%LOCALAPPDATA%\hq\archive`，环境变量缺失时退到临时目录。
fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|dir| dir.join("hq").join("archive"))
        .unwrap_or_else(|| std::env::temp_dir().join("hq-archive"))
}

pub struct Archive {
    inner: Arc<Inner>,
}

struct Inner {
    engine: Engine,
    /// rusqlite 的 `Connection` 不是 `Sync`，而存档是在后台线程里调用的，
    /// 所以外面套一层互斥锁。
    db: Mutex<Connection>,
    /// 最近一次写盘失败的原因，成功时清空。
    error: Mutex<Option<String>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // 把内存表里的记录刷进 SST / WAL 再退出，避免最后几笔行情只留在进程里。
        let _ = self.engine.close();
    }
}

impl Archive {
    /// 打开默认目录下的存档；打不开就返回 `None`，看板照常跑。
    pub fn open() -> Option<Self> {
        Self::at(&data_dir()).ok()
    }

    /// 打开指定目录（测试用）。
    pub fn at(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建存档目录失败: {e}"))?;

        let engine = Engine::open(FlowConfig {
            data_dir: dir.join("flowdb"),
            // 桌面看板的数据量很小，按默认的 64MB memtable / 128MB block cache
            // 配下来太占内存，收一收。
            memtable_size_mb: 4,
            block_size: 4 * 1024,
            flush_interval_ms: 500,
            block_cache_capacity_mb: 16,
            index_memory_budget_mb: 32,
            // 每 5 秒一批写入，逐批 fsync 没必要；后台线程会定期同步 WAL。
            wal_sync_mode: flowdb::SyncMode::IntervalMs(1_000),
            ..FlowConfig::default()
        })
        .map_err(|e| format!("打开 FlowDB 失败: {e}"))?;

        let mut db = Connection::open(dir.join("hq.sqlite3"))
            .map_err(|e| format!("打开 SQLite 失败: {e}"))?;
        init_schema(&mut db).map_err(|e| format!("初始化 SQLite 表失败: {e}"))?;

        Ok(Self {
            inner: Arc::new(Inner {
                engine,
                db: Mutex::new(db),
                error: Mutex::new(None),
            }),
        })
    }

    /// 存档所在目录（FlowDB 的 `flowdb/` 子目录与 `hq.sqlite3` 都在它下面）。
    #[allow(dead_code)]
    pub fn dir() -> PathBuf {
        data_dir()
    }

    /// 最近一次写盘失败的原因。
    pub fn last_error(&self) -> Option<String> {
        self.inner.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 存档一轮批量报价（含汇率）。
    pub fn archive_quotes(&self, quotes: &Quotes) {
        let now = now_micros();
        let mut records = Vec::with_capacity(quotes.quotes.len() + 1);
        let mut rows = Vec::with_capacity(quotes.quotes.len());
        for (symbol, quote) in &quotes.quotes {
            let ts = stamp_micros(&quote.date, &quote.time).unwrap_or(now);
            // FlowDB 收微秒，SQLite 收秒（见模块文档的时间口径）。
            rows.push(QuoteRow {
                symbol,
                ts: ts / MICROS_PER_SEC,
                quote: quote.clone(),
            });
            records.push(Record::new(
                format!("quote:{symbol}"),
                ts,
                json_bytes(&quote_json(quote)),
            ));
        }
        if let Some(rate) = quotes.usdcny {
            records.push(Record::new(
                "rate:usdcny",
                now,
                json_bytes(&serde_json::json!({ "rate": rate })),
            ));
        }

        let rate = quotes.usdcny;
        let fetched_at = now / MICROS_PER_SEC;
        self.commit(records, move |db| {
            let tx = db.transaction()?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO quotes
                       (symbol, ts, fetched_at, price, prev_close, open, high, low, quote_date, quote_time)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                )?;
                for row in &rows {
                    stmt.execute(params![
                        row.symbol,
                        row.ts,
                        fetched_at,
                        row.quote.price,
                        row.quote.prev_close,
                        row.quote.open,
                        row.quote.high,
                        row.quote.low,
                        row.quote.date,
                        row.quote.time,
                    ])?;
                }
            }
            if let Some(rate) = rate {
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO rates (ts, pair, rate) VALUES (?1, 'USDCNY', ?2)",
                )?
                .execute(params![fetched_at, rate])?;
            }
            tx.commit()
        });
    }

    /// 存档某个品种的当日分时曲线。
    pub fn archive_intraday(&self, instrument: Instrument, points: &[MinutePoint]) {
        let mut records = Vec::with_capacity(points.len());
        let mut rows = Vec::with_capacity(points.len());
        for point in points {
            let Some(ts) = stamp_micros(&point.date, &point.time) else {
                continue;
            };
            if ts <= 0 {
                continue;
            }
            records.push(Record::new(
                format!("intraday:{}", instrument.symbol),
                ts,
                json_bytes(&serde_json::json!({ "price": point.price })),
            ));
            rows.push((instrument.symbol, ts / MICROS_PER_SEC, point.price));
        }

        self.commit(records, move |db| {
            let tx = db.transaction()?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO intraday (symbol, ts, price) VALUES (?1, ?2, ?3)",
                )?;
                for row in &rows {
                    stmt.execute(params![row.0, row.1, row.2])?;
                }
            }
            tx.commit()
        });
    }

    /// 存档某个品种的日 K 序列。
    ///
    /// FlowDB 侧仍按微秒存（引擎的时间桶索引要求），SQLite 侧按 `date`（`YYYY-MM-DD`）
    /// 存——日 K 的身份是交易日而不是时刻，日期字符串比较/排序/建索引都直接可用。
    pub fn archive_daily(&self, instrument: Instrument, candles: &[Candle]) {
        let mut records = Vec::with_capacity(candles.len());
        let mut rows = Vec::with_capacity(candles.len());
        for candle in candles {
            let date = candle.date.trim();
            let Some(ts) = date_micros(date) else {
                continue;
            };
            records.push(Record::new(
                format!("daily:{}", instrument.symbol),
                ts,
                json_bytes(&serde_json::json!({
                    "date": date,
                    "open": candle.open,
                    "high": candle.high,
                    "low": candle.low,
                    "close": candle.close,
                })),
            ));
            rows.push((instrument.symbol, date, candle));
        }

        self.commit(records, move |db| {
            let tx = db.transaction()?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO daily (symbol, date, open, high, low, close)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                for (symbol, date, candle) in &rows {
                    stmt.execute(params![
                        symbol,
                        date,
                        candle.open,
                        candle.high,
                        candle.low,
                        candle.close
                    ])?;
                }
            }
            tx.commit()
        });
    }

    /// 把本地缓存里已有的日 K 全量历史补进存档。
    ///
    /// 日 K 接口一次给十几年数据，但界面只在切到"近 7 日 / 30 日 / 1 年"时才去拉；
    /// 默认的分时视图下这些历史永远不会经过网络，所以启动时先从缓存补一遍，
    /// 免得存档从空表开始。缓存里没有的品种自然跳过，等用户切范围时再补。
    ///
    /// 补过的品种按最后一根蜡烛判断：存档里已经有了就不重写，否则每次启动都
    /// 把十几年的历史重灌一遍，白白放大写入。
    pub fn backfill_daily(&self, cache: &Cache) {
        for instrument in INSTRUMENTS {
            let Some(candles) = crate::feed::cached_daily_all(cache, *instrument) else {
                continue;
            };
            let Some(last) = candles.last().map(|c| c.date.trim()) else {
                continue;
            };
            if self.latest_daily_date(instrument.symbol).as_deref() >= Some(last) {
                continue;
            }
            self.archive_daily(*instrument, &candles);
        }
    }

    /// 某个品种在存档里的最后一根日 K 日期（`YYYY-MM-DD`），没有则 `None`。
    fn latest_daily_date(&self, symbol: &str) -> Option<String> {
        self.lock_db()
            .query_row(
                "SELECT MAX(date) FROM daily WHERE symbol = ?1",
                params![symbol],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
    }

    /// 一份数据同时写进 FlowDB 和 SQLite；任一侧失败都只记录原因。
    fn commit(
        &self,
        records: Vec<Record>,
        sqlite: impl FnOnce(&mut Connection) -> rusqlite::Result<()>,
    ) {
        if records.is_empty() {
            return;
        }
        let flow_err = self
            .inner
            .engine
            .write_batch(&records)
            .err()
            .map(|e| format!("FlowDB: {e}"));
        let sqlite_err = {
            let mut db = self.lock_db();
            sqlite(&mut db).err().map(|e| format!("SQLite: {e}"))
        };
        *self.inner.error.lock().unwrap_or_else(|e| e.into_inner()) = flow_err.or(sqlite_err);
    }

    fn lock_db(&self) -> MutexGuard<'_, Connection> {
        self.inner.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    fn engine(&self) -> &Engine {
        &self.inner.engine
    }
}

struct QuoteRow<'a> {
    symbol: &'a str,
    /// epoch 秒：SQLite 的 `datetime(ts, 'unixepoch')` 只认秒。
    ts: i64,
    quote: Quote,
}

fn quote_json(quote: &Quote) -> serde_json::Value {
    serde_json::json!({
        "price": quote.price,
        "prev_close": quote.prev_close,
        "open": quote.open,
        "high": quote.high,
        "low": quote.low,
        "date": quote.date,
        "time": quote.time,
    })
}

fn json_bytes(value: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

/// FlowDB 存微秒，SQLite 存秒；换算用它。
const MICROS_PER_SEC: i64 = 1_000_000;

/// 建表并设置常用的 pragma；重复调用无副作用，同时负责把旧库升级到当前口径。
fn init_schema(db: &mut Connection) -> rusqlite::Result<()> {
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "NORMAL")?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS quotes (
             symbol     TEXT    NOT NULL,
             ts         INTEGER NOT NULL, -- 行情时间（epoch 秒，UTC）
             fetched_at INTEGER NOT NULL, -- 本机取到的时间（epoch 秒）
             price      REAL    NOT NULL,
             prev_close REAL,
             open       REAL,
             high       REAL,
             low        REAL,
             quote_date TEXT,
             quote_time TEXT,
             PRIMARY KEY (symbol, ts)
         );
         CREATE TABLE IF NOT EXISTS intraday (
             symbol TEXT    NOT NULL,
             ts     INTEGER NOT NULL, -- 分钟线时间（epoch 秒，UTC）
             price  REAL    NOT NULL,
             PRIMARY KEY (symbol, ts)
         );
         CREATE TABLE IF NOT EXISTS daily (
             symbol TEXT NOT NULL,
             date   TEXT NOT NULL, -- 交易日（YYYY-MM-DD，交易所口径）
             open   REAL,
             high   REAL,
             low    REAL,
             close  REAL,
             PRIMARY KEY (symbol, date)
         );
         CREATE TABLE IF NOT EXISTS rates (
             ts   INTEGER NOT NULL, -- epoch 秒，UTC
             pair TEXT    NOT NULL,
             rate REAL    NOT NULL,
             PRIMARY KEY (pair, ts)
         );",
    )?;
    migrate(db)
}

/// 把早期版本写的旧口径数据升级到当前口径。
///
/// 旧版本 SQLite 侧直接存了 FlowDB 的微秒时间戳，`daily` 甚至还带 `ts`。SQLite 的
/// `datetime(ts, 'unixepoch')` 只认秒，存微秒等于把这份数据变成"只有本程序看得懂"，
/// 所以这里就地换算：秒表的三张表把 `ts`/`fetched_at` 除以 10^6，`daily` 整表按
/// `date` 重建。整段放在一个事务里，中途失败会整体回滚，`user_version` 没推进就
/// 下次重来。
fn migrate(db: &mut Connection) -> rusqlite::Result<()> {
    /// 旧库标记。没写过 user_version 的老库是 0。
    const VERSION: i64 = 1;
    if db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? >= VERSION {
        return Ok(());
    }

    let tx = db.transaction()?;
    // 微秒值约 1.7e15，秒值约 1.7e9；用 1e12 这条线区分两代数据。
    // 空表、或者已经是秒的数据都不会被误伤。
    const MICRO_THRESHOLD: i64 = 1_000_000_000_000;

    for table in ["quotes", "intraday", "rates"] {
        tx.execute(
            &format!("UPDATE {table} SET ts = ts / {MICROS_PER_SEC} WHERE ts >= ?1"),
            params![MICRO_THRESHOLD],
        )?;
    }
    tx.execute(
        &format!("UPDATE quotes SET fetched_at = fetched_at / {MICROS_PER_SEC} WHERE fetched_at >= ?1"),
        params![MICRO_THRESHOLD],
    )?;

    // `daily` 的旧表主键是 (symbol, ts)，新版换成 (symbol, date)。ts 恰好是该交易日的
    // UTC 午夜，换算回日期是无损的，所以整表搬迁而不是丢弃。
    let has_ts = tx
        .prepare("SELECT 1 FROM pragma_table_info('daily') WHERE name = 'ts'")?
        .exists([])?;
    if has_ts {
        tx.execute_batch(&format!(
            "CREATE TABLE daily_new (
                 symbol TEXT NOT NULL,
                 date   TEXT NOT NULL, -- 交易日（YYYY-MM-DD，交易所口径）
                 open   REAL,
                 high   REAL,
                 low    REAL,
                 close  REAL,
                 PRIMARY KEY (symbol, date)
             );
             INSERT OR REPLACE INTO daily_new (symbol, date, open, high, low, close)
             SELECT symbol,
                    date(CASE WHEN ts >= {MICRO_THRESHOLD} THEN ts / {MICROS_PER_SEC} ELSE ts END,
                         'unixepoch'),
                    open, high, low, close
               FROM daily
              WHERE date(CASE WHEN ts >= {MICRO_THRESHOLD} THEN ts / {MICROS_PER_SEC} ELSE ts END,
                         'unixepoch') IS NOT NULL;
             DROP TABLE daily;
             ALTER TABLE daily_new RENAME TO daily;"
        ))?;
    }

    tx.pragma_update(None, "user_version", VERSION)?;
    tx.commit()
}

fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// `2026-10-28` → 该日 00:00 的微秒时间戳。
fn date_micros(date: &str) -> Option<i64> {
    let date = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_micros())
}

/// `2026-10-28` + `23:09:00`（或 `23:09`）→ 微秒时间戳。
fn stamp_micros(date: &str, time: &str) -> Option<i64> {
    let date = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").ok()?;
    let time = NaiveTime::parse_from_str(time.trim(), "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(time.trim(), "%H:%M"))
        .ok()?;
    Some(NaiveDateTime::new(date, time).and_utc().timestamp_micros())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hq-archive-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample_quote() -> Quote {
        Quote {
            price: 4046.66,
            prev_close: 4060.13,
            open: 4061.17,
            high: 4070.74,
            low: 4044.61,
            date: "2026-10-28".into(),
            time: "11:19:00".into(),
        }
    }

    #[test]
    fn archives_quotes_to_both_stores() {
        let dir = temp_dir("quotes");
        let archive = Archive::at(&dir).expect("open archive");
        let mut quotes = HashMap::new();
        quotes.insert("XAU", sample_quote());
        archive.archive_quotes(&Quotes {
            quotes,
            usdcny: Some(6.668),
        });
        assert_eq!(archive.last_error(), None);

        let expected_ts = stamp_micros("2026-10-28", "11:19:00").expect("ts");

        // FlowDB：同一个进程内按品种前缀扫，能拿到这条记录。
        // 注意不能在 archive 还持有时另开一个 Engine 指向同一目录——同一份数据
        // 被两个实例打开会冲突，这里直接用 archive 持有的那个。
        let found = archive
            .engine()
            .query(flowdb::Query::prefix("quote:XAU"))
            .expect("query");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].ts, expected_ts);

        // SQLite：主键 (symbol, ts) 能查到同样的价格。SQLite 侧存 epoch 秒，
        // 且能被 `datetime()` 直接解释（这正是换成秒的原因）。
        let db = Connection::open(dir.join("hq.sqlite3")).expect("sqlite");
        let (price, fetched, readable): (f64, i64, String) = db
            .query_row(
                "SELECT price, fetched_at, datetime(ts, 'unixepoch')
                   FROM quotes WHERE symbol = 'XAU' AND ts = ?1",
                params![expected_ts / MICROS_PER_SEC],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("row");
        assert_eq!(price, 4046.66);
        assert!(fetched > 0);
        assert_eq!(readable, "2026-10-28 11:19:00");
        // 秒级时间戳不可能再是微秒量级，防止又退回旧口径。
        assert!(fetched < 10_000_000_000, "fetched_at 应是秒: {fetched}");
        let rate: f64 = db
            .query_row("SELECT rate FROM rates WHERE pair = 'USDCNY'", [], |row| {
                row.get(0)
            })
            .expect("rate");
        assert!((rate - 6.668).abs() < 1e-9);

        drop(archive);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flowdb_records_survive_reopen() {
        let dir = temp_dir("reopen");
        let expected_ts = stamp_micros("2026-10-28", "11:19:00").expect("ts");
        {
            let archive = Archive::at(&dir).expect("open archive");
            let mut quotes = HashMap::new();
            quotes.insert("XAU", sample_quote());
            archive.archive_quotes(&Quotes {
                quotes,
                usdcny: None,
            });
            assert_eq!(archive.last_error(), None);
            // `Inner::drop` 会把 memtable 刷进 WAL，这里模拟关掉进程再打开。
        }

        let engine = Engine::open(FlowConfig {
            data_dir: dir.join("flowdb"),
            auto_background: false,
            ..FlowConfig::default()
        })
        .expect("reopen");
        let found = engine
            .query(flowdb::Query::prefix_time_range(
                "quote:XAU",
                expected_ts,
                expected_ts + 1,
            ))
            .expect("query");
        assert_eq!(found.len(), 1, "重新打开后应能读到已存档的报价");
        let value: serde_json::Value =
            serde_json::from_slice(&found[0].value).expect("json value");
        assert_eq!(value["price"], 4046.66);
        let _ = engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archives_intraday_and_daily() {
        let dir = temp_dir("series");
        let archive = Archive::at(&dir).expect("open archive");
        let xau = Instrument::find("XAU").expect("XAU");

        archive.archive_intraday(
            xau,
            &[
                MinutePoint {
                    date: "2026-10-28".into(),
                    time: "06:00".into(),
                    price: 4062.27,
                },
                MinutePoint {
                    date: "2026-10-28".into(),
                    time: "06:01".into(),
                    price: 4061.67,
                },
            ],
        );
        archive.archive_daily(
            xau,
            &[Candle {
                date: "2026-10-27".into(),
                open: 4030.8,
                high: 4079.4,
                low: 4001.6,
                close: 4073.1,
            }],
        );
        assert_eq!(archive.last_error(), None);

        let db = Connection::open(dir.join("hq.sqlite3")).expect("sqlite");
        let points: i64 = db
            .query_row("SELECT COUNT(*) FROM intraday WHERE symbol = 'XAU'", [], |row| {
                row.get(0)
            })
            .expect("intraday count");
        assert_eq!(points, 2);
        let close: f64 = db
            .query_row(
                "SELECT close FROM daily WHERE symbol = 'XAU' AND date = '2026-10-27'",
                [],
                |row| row.get(0),
            )
            .expect("daily row");
        assert_eq!(close, 4073.1);
        // 分钟线在 SQLite 侧也是秒，`datetime()` 能直接读。
        let readable: String = db
            .query_row(
                "SELECT datetime(ts, 'unixepoch') FROM intraday
                  WHERE symbol = 'XAU' ORDER BY ts LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("intraday row");
        assert_eq!(readable, "2026-10-28 06:00:00");

        drop(archive);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backfills_daily_history_from_cache() {
        let dir = temp_dir("backfill");
        let cache_dir = temp_dir("backfill-cache");
        let cache = Cache::at(&cache_dir);
        // 日 K 缓存是接口原文，接口返回的是全量历史：这里放两根，存档应都收下。
        cache.write(
            "daily-XAU",
            concat!(
                "/*<script>location.href='//sina.com';</script>*/\nvar t=([",
                r#"{"date":"2026-10-20","open":"4000.0","high":"4010.0","low":"3990.0","close":"4005.0"},"#,
                r#"{"date":"2026-10-27","open":"4030.8","high":"4079.4","low":"4001.6","close":"4073.1"}"#,
                "]);",
            ),
        );

        let archive = Archive::at(&dir).expect("open archive");
        archive.backfill_daily(&cache);
        assert_eq!(archive.last_error(), None);

        let db = Connection::open(dir.join("hq.sqlite3")).expect("sqlite");
        let rows: i64 = db
            .query_row("SELECT COUNT(*) FROM daily WHERE symbol = 'XAU'", [], |row| {
                row.get(0)
            })
            .expect("daily count");
        assert_eq!(rows, 2, "缓存里的全量日 K 都应补进存档");

        // 再补一遍：最后一根已经在存档里了，不该重复写。
        archive.backfill_daily(&cache);
        let rows: i64 = db
            .query_row("SELECT COUNT(*) FROM daily WHERE symbol = 'XAU'", [], |row| {
                row.get(0)
            })
            .expect("daily count");
        assert_eq!(rows, 2, "已补齐的品种不应重复回填");

        drop(archive);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn rejects_unparseable_timestamps() {
        assert!(stamp_micros("", "11:19:00").is_none());
        assert!(stamp_micros("2026-10-28", "").is_none());
        assert!(date_micros("not-a-date").is_none());
        assert!(stamp_micros("2026-10-28", "11:19").is_some());
    }

    #[test]
    fn migrates_legacy_microsecond_rows() {
        let dir = temp_dir("migrate");
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("hq.sqlite3");

        // 旧版口径：秒表里存的是微秒，`daily` 还有 `ts` 列、没有 `date`。
        let legacy_ts = stamp_micros("2026-10-28", "11:19:00").expect("ts");
        {
            let db = Connection::open(&path).expect("legacy db");
            db.execute_batch(
                "CREATE TABLE quotes (symbol TEXT NOT NULL, ts INTEGER NOT NULL,
                     fetched_at INTEGER NOT NULL, price REAL NOT NULL, prev_close REAL,
                     open REAL, high REAL, low REAL, quote_date TEXT, quote_time TEXT,
                     PRIMARY KEY (symbol, ts));
                 CREATE TABLE intraday (symbol TEXT NOT NULL, ts INTEGER NOT NULL,
                     price REAL NOT NULL, PRIMARY KEY (symbol, ts));
                 CREATE TABLE daily (symbol TEXT NOT NULL, ts INTEGER NOT NULL, open REAL,
                     high REAL, low REAL, close REAL, PRIMARY KEY (symbol, ts));
                 CREATE TABLE rates (ts INTEGER NOT NULL, pair TEXT NOT NULL,
                     rate REAL NOT NULL, PRIMARY KEY (pair, ts));",
            )
            .expect("legacy schema");
            db.execute(
                "INSERT INTO quotes VALUES ('XAU', ?1, ?2, 4046.66, 4060.13, 4061.17,
                   4070.74, 4044.61, '2026-10-28', '11:19:00')",
                params![legacy_ts, legacy_ts],
            )
            .expect("legacy quote");
            db.execute(
                "INSERT INTO daily VALUES ('XAU', ?1, 1.0, 2.0, 0.5, 1.5)",
                params![date_micros("2026-10-27").expect("day")],
            )
            .expect("legacy candle");
        }

        // 打开存档即触发迁移。
        let archive = Archive::at(&dir).expect("open archive");
        drop(archive);

        let db = Connection::open(&path).expect("migrated db");
        let (ts, fetched, readable): (i64, i64, String) = db
            .query_row(
                "SELECT ts, fetched_at, datetime(ts, 'unixepoch') FROM quotes",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("migrated quote");
        assert_eq!(ts, legacy_ts / MICROS_PER_SEC);
        assert_eq!(fetched, legacy_ts / MICROS_PER_SEC);
        assert_eq!(readable, "2026-10-28 11:19:00");

        // `daily` 换成了 (symbol, date) 主键，旧蜡烛按 ts 换算成日期保留下来。
        let columns: Vec<String> = db
            .prepare("SELECT name FROM pragma_table_info('daily')")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("columns")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        assert!(columns.contains(&"date".to_string()), "{columns:?}");
        assert!(!columns.contains(&"ts".to_string()), "{columns:?}");
        let (date, close): (String, f64) = db
            .query_row("SELECT date, close FROM daily WHERE symbol = 'XAU'", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("migrated candle");
        assert_eq!(date, "2026-10-27", "旧日 K 应按交易日换算保留");
        assert_eq!(close, 1.5);
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user_version");
        assert_eq!(version, 1);

        // 再打开一次不会重复迁移（秒值不该被再除一次）。
        let archive = Archive::at(&dir).expect("reopen");
        drop(archive);
        let db = Connection::open(&path).expect("db");
        let ts: i64 = db
            .query_row("SELECT ts FROM quotes", [], |row| row.get(0))
            .expect("ts after reopen");
        assert_eq!(ts, legacy_ts / MICROS_PER_SEC, "迁移必须幂等");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
