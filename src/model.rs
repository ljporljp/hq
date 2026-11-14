//! 视图状态：持有全部品种的最近一次快照，并驱动后台轮询。
//!
//! 线程模型：网络请求全部放在 `background_executor` 上跑（`feed` 里是阻塞的 ureq
//! 调用），主线程只接收结果、`cx.notify()` 重绘。UI 永远不会被网络卡住。
//!
//! 一屏要画 [`feed::INSTRUMENTS`] 里的全部品种，所以：
//! - 实时报价由新浪的批量接口一次取回（30 个品种一次往返）；
//! - 分时 / 日 K 每个品种一个接口，由后台任务用若干线程并发拉取，
//!   每轮只补"缺的或过期的"那些。
//!
//! 缓存：启动时同步读一次本地缓存（纯文件 IO，很快），先画出上一份行情；
//! 之后每次请求成功都会覆盖缓存，请求失败则回退到缓存（见 [`crate::cache`]）。

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gpui_kit::{AppContext as _, Context, SharedString, Task, WeakEntity};

use crate::archive::Archive;
use crate::cache::Cache;
use crate::components::{ChartPoint, Series};
use crate::feed::{
    self, Candle, Category, Instrument, MinutePoint, Quote, GRAMS_PER_TROY_OUNCE, INSTRUMENTS,
};

/// 实时报价轮询间隔。新浪的现货报价本身约 1s 更新一次，5s 足够且不给对端压力。
const QUOTE_INTERVAL: Duration = Duration::from_secs(5);
/// 分时曲线不需要跟着秒价刷，一分钟一次即可。
const INTRADAY_TTL: Duration = Duration::from_secs(60);
/// 日 K 一天只变一次，但要照顾盘中最新一根，10 分钟刷一次。
const DAILY_TTL: Duration = Duration::from_secs(600);
/// 30 个品种的序列并发拉取，同时打满对端不礼貌，6 路足够。
const SERIES_WORKERS: usize = 6;

/// 图表时间范围。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Range {
    Intraday,
    Week,
    Month,
    Year,
}

impl Range {
    pub const ALL: [Range; 4] = [Range::Intraday, Range::Week, Range::Month, Range::Year];

    pub fn label(self) -> &'static str {
        match self {
            Range::Intraday => "分时",
            Range::Week => "近 7 日",
            Range::Month => "近 30 日",
            Range::Year => "近 1 年",
        }
    }

    /// 日 K 需要保留的根数；分时走另一个接口。
    fn daily_keep(self) -> Option<usize> {
        match self {
            Range::Intraday => None,
            Range::Week => Some(7),
            Range::Month => Some(30),
            Range::Year => Some(250),
        }
    }

    fn ttl(self) -> Duration {
        if self.daily_keep().is_some() {
            DAILY_TTL
        } else {
            INTRADAY_TTL
        }
    }
}

/// 后台线程里拿到的原始序列：全是 `String` / `f64`，不跨线程搬 UI 类型。
enum RawSeries {
    Intraday(Vec<MinutePoint>),
    Daily(Vec<Candle>),
}

impl RawSeries {
    /// 回到主线程后再转成绘图用的 [`Series`]。
    fn into_series(self) -> Series {
        match self {
            RawSeries::Intraday(points) => Series::Intraday(
                points
                    .into_iter()
                    .map(|p| ChartPoint {
                        label: SharedString::from(p.time),
                        value: p.price,
                    })
                    .collect(),
            ),
            RawSeries::Daily(candles) => Series::Daily(candles),
        }
    }
}

/// 某个品种已加载的序列，连同它对应的范围和加载时间。
struct Loaded {
    series: Series,
    range: Range,
    /// `None` 表示这份数据来自本地缓存、或还没成功过：下一轮立刻重试。
    loaded_at: Option<Instant>,
}

/// 后台任务要做什么。
pub struct Job {
    agent: ureq::Agent,
    cache: Cache,
    archive: Option<Arc<Archive>>,
    range: Range,
    /// 这一轮需要（重新）拉序列的品种。
    instruments: Vec<Instrument>,
}

/// 后台任务的结果。
pub struct Fetched {
    quotes: Option<Result<feed::Fetched<feed::Quotes>, String>>,
    series: Vec<(Instrument, Result<feed::Fetched<RawSeries>, String>)>,
}

impl Job {
    fn run(self) -> Fetched {
        let quotes = feed::fetch_quotes(&self.agent, &self.cache);
        // 只存网络取到的：缓存回退的那份上一轮已经存过了。
        if let (Ok(fetched), Some(archive)) = (&quotes, self.archive.as_deref()) {
            if !fetched.cached {
                archive.archive_quotes(&fetched.value);
            }
        }
        let series = self.fetch_series();
        Fetched {
            quotes: Some(quotes),
            series,
        }
    }

    /// 并发拉取若干品种的序列，最多 [`SERIES_WORKERS`] 路。
    fn fetch_series(&self) -> Vec<(Instrument, Result<feed::Fetched<RawSeries>, String>)> {
        if self.instruments.is_empty() {
            return Vec::new();
        }
        let workers = SERIES_WORKERS.min(self.instruments.len());
        // 按品种数量均分成 `workers` 段，每段一个线程顺序拉。
        let chunk = self.instruments.len().div_ceil(workers);
        let range = self.range;
        let agent = &self.agent;
        let cache = &self.cache;
        let archive = self.archive.as_deref();

        thread::scope(|scope| {
            let handles: Vec<_> = self
                .instruments
                .chunks(chunk)
                .map(|chunk| {
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .map(|it| (*it, fetch_series(agent, cache, archive, *it, range)))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();

            handles
                .into_iter()
                .flat_map(|handle| handle.join().unwrap_or_default())
                .collect()
        })
    }
}

fn fetch_series(
    agent: &ureq::Agent,
    cache: &Cache,
    archive: Option<&Archive>,
    instrument: Instrument,
    range: Range,
) -> Result<feed::Fetched<RawSeries>, String> {
    match range.daily_keep() {
        Some(keep) => feed::fetch_daily(agent, cache, instrument).map(|fetched| {
            // 存档要全量历史；界面只画最近 `keep` 根，所以先存档再裁剪。
            if let (Some(archive), false) = (archive, fetched.cached) {
                archive.archive_daily(instrument, &fetched.value);
            }
            fetched.map(|mut candles| {
                if candles.len() > keep {
                    candles.drain(..candles.len() - keep);
                }
                RawSeries::Daily(candles)
            })
        }),
        None => feed::fetch_intraday(agent, cache, instrument).map(|fetched| {
            if let (Some(archive), false) = (archive, fetched.cached) {
                archive.archive_intraday(instrument, &fetched.value);
            }
            fetched.map(RawSeries::Intraday)
        }),
    }
}

pub struct MarketView {
    range: Range,
    /// 左侧导航选中的品类；`None` 表示"全部"。
    category: Option<Category>,
    /// 顶部展示的品种，按 [`INSTRUMENTS`] 的下标存。
    selected: Option<usize>,
    quotes: HashMap<&'static str, Quote>,
    /// USD/CNY，拿不到就退化为只显示原币价格。
    usdcny: Option<f64>,
    /// 按品种代码索引的序列。
    series: HashMap<&'static str, Loaded>,
    /// 当前展示的数据里含有本地缓存回退的部分（断网 / 对端异常）。
    stale: bool,
    cache: Cache,
    archive: Option<Arc<Archive>>,
    /// 存档最近一次写盘失败的原因，显示在页脚里。
    archive_error: Option<String>,
    error: Option<String>,
    updated_at: Option<SharedString>,
    _task: Task<()>,
}

impl MarketView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let agent = feed::agent();
        let cache = Cache::new();
        // 存档打不开（目录无权限、文件被占用）不该拖垮看板，只是不落盘。
        let archive = Archive::open().map(Arc::new);
        // 日 K 的历史只在用户切到日线视图时才会走网络，先在后台把缓存里已有的
        // 那份全量历史补进存档，免得存档从空表开始。
        if let Some(archive) = archive.clone() {
            let cache = cache.clone();
            thread::spawn(move || archive.backfill_daily(&cache));
        }

        // 先用本地缓存铺一遍：冷启动/断网时界面上也有东西看，后台刷新随后覆盖。
        let (quotes, usdcny) = match feed::cached_quotes(&cache) {
            Some(snapshot) => (snapshot.quotes, snapshot.usdcny),
            None => (HashMap::new(), None),
        };
        let mut series = HashMap::new();
        for instrument in INSTRUMENTS {
            // 缓存里的分时曲线只在同一天有意义，`cached_series` 已按时限筛过。
            if let Some(loaded) = cached_series(&cache, *instrument, Range::Intraday) {
                series.insert(instrument.symbol, loaded);
            }
        }

        let selected = default_selection(&quotes, &series);

        let task = cx.spawn({
            let agent = agent.clone();
            let cache = cache.clone();
            let archive = archive.clone();
            async move |this: WeakEntity<Self>, cx: &mut gpui_kit::AsyncApp| {
                loop {
                    // 每轮都重新问实体要做什么：实体没了就退出循环。
                    let Ok(job) = this.update(cx, |view, _| {
                        view.next_job(&agent, &cache, archive.clone())
                    }) else {
                        break;
                    };

                    // 阻塞的网络请求必须挪到后台线程。
                    let fetched = cx.background_spawn(async move { job.run() }).await;

                    if this.update(cx, |view, cx| view.apply(fetched, cx)).is_err() {
                        break;
                    }

                    cx.background_executor().timer(QUOTE_INTERVAL).await;
                }
            }
        });

        Self {
            range: Range::Intraday,
            // 默认"全部"：进来先看到整个市场。
            category: None,
            selected,
            quotes,
            usdcny,
            series,
            stale: false,
            cache,
            archive,
            archive_error: None,
            error: None,
            updated_at: None,
            _task: task,
        }
    }

    fn next_job(
        &self,
        agent: &ureq::Agent,
        cache: &Cache,
        archive: Option<Arc<Archive>>,
    ) -> Job {
        let wanted: Vec<Instrument> = INSTRUMENTS
            .iter()
            .copied()
            .filter(|instrument| match self.series.get(instrument.symbol) {
                Some(loaded) => {
                    let expired = loaded
                        .loaded_at
                        .is_none_or(|at| at.elapsed() >= self.range.ttl());
                    loaded.range != self.range || expired
                }
                None => true,
            })
            .collect();

        Job {
            agent: agent.clone(),
            cache: cache.clone(),
            archive,
            range: self.range,
            instruments: wanted,
        }
    }

    fn apply(&mut self, fetched: Fetched, cx: &mut Context<Self>) {
        let mut errors = Vec::new();
        // 这一轮是否完全来自网络；只要有一项回退到缓存就标记为陈旧。
        let mut cached = false;

        if let Some(result) = fetched.quotes {
            match result {
                Ok(feed::Fetched {
                    value,
                    cached: from_cache,
                }) => {
                    cached |= from_cache;
                    // 休市的品种返回 0 价，`parse_quotes` 已经不收；这里整份替换，
                    // 缺的品种在界面上显示"暂无报价"而不是上一轮的旧价。
                    self.quotes = value.quotes;
                    if value.usdcny.is_some() {
                        self.usdcny = value.usdcny;
                    }
                    // 缓存的报价不刷新"最后更新"，否则会把旧价格显示成刚取到的。
                    if !from_cache {
                        self.updated_at =
                            Some(chrono::Local::now().format("%H:%M:%S").to_string().into());
                    }
                }
                // 断网且没有可用缓存时，保留上一笔有效报价，只把错误显示出来。
                Err(err) => errors.push(err),
            }
        }

        for (instrument, result) in fetched.series {
            match result {
                Ok(feed::Fetched {
                    value,
                    cached: from_cache,
                }) => {
                    cached |= from_cache;
                    self.series.insert(
                        instrument.symbol,
                        Loaded {
                            series: value.into_series(),
                            range: self.range,
                            // 缓存回退不算"刚拉到"：loaded_at 留空，下一轮（5 秒后）就重试，
                            // 而不是等满 TTL 才去网络再试。
                            loaded_at: (!from_cache).then(Instant::now),
                        },
                    );
                }
                // 单个品种失败（比如非交易时段接口返回空）不该淹没其它品种的错误。
                Err(err) => errors.push(format!("{}: {err}", instrument.name)),
            }
        }

        self.stale = cached;
        // 存档失败只提示，不影响行情展示。
        self.archive_error = self.archive.as_deref().and_then(Archive::last_error);
        self.error = if errors.is_empty() {
            None
        } else {
            Some(errors.join("；"))
        };
        // 选中的品种可能在休市后不再有报价；保持用户的选择，只有确实没选过才补一个。
        if self.selected.is_none() {
            self.selected = default_selection(&self.quotes, &self.series);
        }
        cx.notify();
    }

    pub fn select_instrument(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < INSTRUMENTS.len() && self.selected != Some(index) {
            self.selected = Some(index);
            cx.notify();
        }
    }

    /// 切换左侧分组。`None` 表示"全部"。
    ///
    /// 换组后如果原来选中的品种不在新组里，就把顶部换成新组的第一个品种，
    /// 否则顶部会一直停在一个当前看不见的品种上。
    pub fn select_category(&mut self, category: Option<Category>, cx: &mut Context<Self>) {
        if self.category == category {
            return;
        }
        self.category = category;
        let still_visible = self
            .selected
            .and_then(|index| INSTRUMENTS.get(index))
            .is_some_and(|it| category.is_none_or(|c| c == it.category()));
        if !still_visible {
            let next = INSTRUMENTS
                .iter()
                .position(|it| category.is_none_or(|c| c == it.category()));
            self.selected = next;
        }
        cx.notify();
    }

    pub fn category(&self) -> Option<Category> {
        self.category
    }

    /// 该品种是否属于当前分组；"全部"时恒为真。
    pub fn is_visible(&self, instrument: Instrument) -> bool {
        self.category
            .is_none_or(|category| category == instrument.category())
    }

    pub fn select_range(&mut self, range: Range, cx: &mut Context<Self>) {
        if self.range != range {
            self.range = range;
            // 先看本地缓存里有没有这个范围，有就直接画出来；没有则显示加载态。
            // 后台循环随即会拉最新数据覆盖。
            self.series.clear();
            for instrument in INSTRUMENTS {
                if let Some(loaded) = cached_series(&self.cache, *instrument, range) {
                    self.series.insert(instrument.symbol, loaded);
                }
            }
            cx.notify();
        }
    }

    pub fn refresh_now(&mut self, cx: &mut Context<Self>) {
        for loaded in self.series.values_mut() {
            loaded.loaded_at = None;
        }
        cx.notify();
    }

    pub fn range(&self) -> Range {
        self.range
    }

    /// 顶部展示的品种；一个可画的品种都没有时为 `None`。
    pub fn selected_instrument(&self) -> Option<Instrument> {
        self.selected.map(|index| INSTRUMENTS[index])
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub fn selected_quote(&self) -> Option<&Quote> {
        self.selected_instrument()
            .and_then(|it| self.quotes.get(it.symbol))
    }

    pub fn selected_series(&self) -> Option<&Series> {
        self.selected_instrument()
            .and_then(|it| self.series.get(it.symbol))
            .map(|loaded| &loaded.series)
    }

    pub fn quote_of(&self, instrument: Instrument) -> Option<&Quote> {
        self.quotes.get(instrument.symbol)
    }

    pub fn series_of(&self, instrument: Instrument) -> Option<&Series> {
        self.series
            .get(instrument.symbol)
            .map(|loaded| &loaded.series)
    }

    pub fn usdcny(&self) -> Option<f64> {
        self.usdcny
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 存档写盘失败的原因（磁盘满、文件被占用等）。
    pub fn archive_error(&self) -> Option<&str> {
        self.archive_error.as_deref()
    }

    /// 当前展示的数据里有本地缓存回退的成分（不是刚从网络取到的）。
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    pub fn updated_at(&self) -> Option<&SharedString> {
        self.updated_at.as_ref()
    }

    /// 已拿到当前范围序列的品种数 / 总数。
    pub fn loaded_count(&self) -> (usize, usize) {
        let ready = INSTRUMENTS
            .iter()
            .filter(|it| {
                self.series
                    .get(it.symbol)
                    .is_some_and(|loaded| loaded.range == self.range)
            })
            .count();
        (ready, INSTRUMENTS.len())
    }

    /// 还有品种没拿到当前范围的序列。
    pub fn is_loading(&self) -> bool {
        let (ready, total) = self.loaded_count();
        ready < total
    }
}

/// 默认选中的品种：优先第一个有报价的，其次第一个有数据的。
fn default_selection(
    quotes: &HashMap<&'static str, Quote>,
    series: &HashMap<&'static str, Loaded>,
) -> Option<usize> {
    INSTRUMENTS
        .iter()
        .position(|it| quotes.contains_key(it.symbol))
        .or_else(|| {
            INSTRUMENTS.iter().position(|it| {
                series
                    .get(it.symbol)
                    .is_some_and(|loaded| !loaded.series.is_empty())
            })
        })
}

/// 从本地缓存还原某个范围的序列；缓存缺失或过期返回 `None`。
fn cached_series(cache: &Cache, instrument: Instrument, range: Range) -> Option<Loaded> {
    let series = match range.daily_keep() {
        Some(keep) => feed::cached_daily(cache, instrument, keep).map(Series::Daily),
        None => feed::cached_intraday(cache, instrument).map(|points| {
            Series::Intraday(
                points
                    .into_iter()
                    .map(|p| ChartPoint {
                        label: SharedString::from(p.time),
                        value: p.price,
                    })
                    .collect(),
            )
        }),
    }?;
    Some(Loaded {
        series,
        range,
        // 缓存不算"刚加载过"，下一轮立刻重新拉。
        loaded_at: None,
    })
}

/// 美元/盎司 → 人民币/克。
pub fn cny_per_gram(usd_per_oz: f64, usdcny: f64) -> f64 {
    usd_per_oz / GRAMS_PER_TROY_OUNCE * usdcny
}
