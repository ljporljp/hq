//! 行情数据层：负责取数与解析，全部是**阻塞**调用，供后台线程使用。
//!
//! 数据源选用新浪财经的公开行情接口（免费、无需 API Key、国内可直连）：
//!
//! | 用途       | 接口                                                                                    |
//! | ---------- | --------------------------------------------------------------------------------------- |
//! | 实时报价   | `hq.sinajs.cn` 的 `hf_<品种>`（一次请求取回全部品种）与 `fx_susdcny`（在岸人民币），GBK 编码 |
//! | 分时曲线   | `stock2.finance.sina.com.cn` 的 `GlobalFuturesService.getGlobalFuturesMinLine`          |
//! | 日 K 线    | 同上的 `getGlobalFuturesDailyKLine`                                                     |
//!
//! 后两个接口返回的是 JSONP（`var t=(...);`），需要先剥掉包装再按 JSON 解析。
//!
//! 支持的品种见 [`INSTRUMENTS`]，每个品种的分时/日 K URL 都是编译期常量
//! （见 [`instrument!`]）；实时报价则是一次批量请求取回全部品种（见 [`quote_url`]）。
//!
//! 每次成功的响应都会原样写进本地缓存（见 [`crate::cache`]）；请求失败或
//! 解析不出数据时，回退到缓存里足够新的那一份，界面至少还有上一份行情可看。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

use crate::cache::Cache;

/// 1 金衡盎司 = 31.1034768 克
pub const GRAMS_PER_TROY_OUNCE: f64 = 31.103_476_8;

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) hq/0.1";
const SINA_REFERER: &str = "https://finance.sina.com.cn/";

/// 一个可查询的外盘品种。
///
/// 所有字段都是编译期常量：URL 在 [`instrument!`] 宏里用 `concat!` 拼好，
/// 取数时直接拿来用，不再有 `{}` 占位符需要运行时替换。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instrument {
    /// 新浪品种代码，同时用作缓存文件名的一部分。
    pub symbol: &'static str,
    /// 中文名，用于界面展示。
    pub name: &'static str,
    /// 报价单位，如 `美元/盎司`、`美分/蒲式耳`。接口只给数值不给单位，这里按品种写死。
    pub unit: &'static str,
    /// 实时报价接口里的变量名，即 `var hq_str_hf_XAU="..."` 中的 `hq_str_hf_XAU`。
    pub quote_var: &'static str,
    /// 当日分时曲线 URL（JSONP）。
    pub intraday_url: &'static str,
    /// 日 K 线 URL（JSONP）。
    pub daily_url: &'static str,
}

/// 由品种代码、中文名和报价单位生成 [`Instrument`]，URL 在编译期拼接。
macro_rules! instrument {
    ($symbol:literal, $name:literal, $unit:literal) => {
        Instrument {
            symbol: $symbol,
            name: $name,
            unit: $unit,
            quote_var: concat!("hq_str_hf_", $symbol),
            intraday_url: concat!(
                "https://stock2.finance.sina.com.cn/futures/api/jsonp.php/var%20t=/",
                "GlobalFuturesService.getGlobalFuturesMinLine?symbol=",
                $symbol
            ),
            daily_url: concat!(
                "https://stock2.finance.sina.com.cn/futures/api/jsonp.php/var%20t=/",
                "GlobalFuturesService.getGlobalFuturesDailyKLine?symbol=",
                $symbol
            ),
        }
    };
}

/// 新浪外盘接口支持的全部品种，即 <https://finance.sina.com.cn/money/future/hf.html>
/// 在架的 30 个品种（2026-10-28 逐个实测：实时报价非空、分时接口有数据）。
///
/// 报价单位取自该页面的品种表（`oHF_1` 里的第二个元素）与各品种详情页的
/// "报价单位"一栏；表里几处明显是抄黄金的（`FCPO` 写成美元/盎司、`RSS3` 写成
/// "钱/公斤"），按交易所实际口径更正为令吉/吨、日元/公斤。
///
/// 该页面里还有一组已经停更、接口返回空的品种，没有收录：
/// `TRB`（日橡胶，日 K 止于 2019-04）、`NAS`（纳指期货）、`DJS`（道指期货）以及
/// IMM 外汇系列 `DXF`/`SF`/`CD`/`JY`/`BP`/`EC`（均止于 2019 年前后）。
macro_rules! instruments {
    ($($symbol:literal => $name:literal, $unit:literal),* $(,)?) => {
        pub const INSTRUMENTS: &[Instrument] = &[$(instrument!($symbol, $name, $unit)),*];
    };
}

instruments! {
    "XAU"  => "伦敦金（现货黄金）", "美元/盎司",
    "XAG"  => "伦敦银（现货白银）", "美元/盎司",
    "XPT"  => "纽约铂金", "美元/盎司",
    "XPD"  => "纽约钯金", "美元/盎司",
    "GC"   => "纽约黄金", "美元/盎司",
    "SI"   => "纽约白银", "美元/盎司",
    "HG"   => "美铜", "美分/磅",
    "CL"   => "纽约原油", "美元/桶",
    "OIL"  => "布伦特原油", "美元/桶",
    "NG"   => "美国天然气", "美元/mmbtu",
    "S"    => "美国大豆", "美分/蒲式耳",
    "W"    => "美国小麦", "美分/蒲式耳",
    "C"    => "美国玉米", "美分/蒲式耳",
    "BO"   => "美黄豆油", "美分/磅",
    "SM"   => "美黄豆粉", "美元/短吨",
    "CT"   => "美国棉花", "美分/磅",
    "RS"   => "美国原糖", "美分/磅",
    "LHC"  => "美瘦猪肉", "美分/磅",
    "BTC"  => "比特币期货", "美元",
    "ES"   => "标普500指数期货", "点",
    "NID"  => "伦镍", "美元/吨",
    "PBD"  => "伦铅", "美元/吨",
    "SND"  => "伦锡", "美元/吨",
    "ZSD"  => "伦锌", "美元/吨",
    "AHD"  => "伦铝", "美元/吨",
    "CAD"  => "伦铜", "美元/吨",
    "FEF"  => "新加坡铁矿石", "美元/公吨",
    "FCPO" => "马棕油", "令吉/吨",
    "RSS3" => "日橡胶", "日元/公斤",
    "EUA"  => "欧洲碳排放", "欧元/吨",
}

impl Instrument {
    /// 按代码查找品种。
    ///
    /// 界面现在是"全品种"视图，不再按代码挑一个；这个方法留给调用方与测试用。
    #[allow(dead_code)]
    pub fn find(symbol: &str) -> Option<Instrument> {
        INSTRUMENTS.iter().copied().find(|it| it.symbol == symbol)
    }

    /// 品种所属的大类，仅用于界面左侧的分组导航。
    ///
    /// 分类口径按国内商品盘面的习惯划分：贵金属与有色金属归"金属"，
    /// 粮油与肉糖归"粮食"，棉花 / 橡胶 / 铁矿石这类工业原料归"工业品"。
    pub fn category(self) -> Category {
        match self.symbol {
            "S" | "W" | "C" | "BO" | "SM" | "RS" | "LHC" | "FCPO" => Category::Grain,
            "CL" | "OIL" | "NG" => Category::Energy,
            "XAU" | "XAG" | "XPT" | "XPD" | "GC" | "SI" | "HG" | "CAD" | "AHD" | "ZSD" | "PBD"
            | "NID" | "SND" => Category::Metal,
            "ES" => Category::Index,
            "CT" | "RSS3" | "FEF" => Category::Industrial,
            // 比特币与碳排放配额既不是金属也不是农产品，统一归"其他"。
            _ => Category::Other,
        }
    }
}

/// 品种大类。"全部"不在这里，它是界面上的"不过滤"，用 `None` 表示。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Grain,
    Energy,
    Metal,
    Index,
    Industrial,
    Other,
}

impl Category {
    /// 除"全部"之外的六个分组，按界面展示顺序排列。
    pub const ALL: [Category; 6] = [
        Category::Grain,
        Category::Energy,
        Category::Metal,
        Category::Index,
        Category::Industrial,
        Category::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Grain => "粮食",
            Category::Energy => "能源",
            Category::Metal => "金属",
            Category::Index => "指数",
            Category::Industrial => "工业品",
            Category::Other => "其他",
        }
    }

    /// 该分组下的全部品种，顺序与 [`INSTRUMENTS`] 一致。
    pub fn instruments(self) -> impl Iterator<Item = Instrument> {
        INSTRUMENTS
            .iter()
            .copied()
            .filter(move |it| it.category() == self)
    }
}
/// 报价与汇率共用一次请求，缓存按整体存一份（30 个品种一次取回）。
const QUOTE_KEY: &str = "quotes-all";

/// 实时报价 URL：`hf_<品种>` 加汇率，新浪支持一次查询多个代码。
///
/// 代码是编译期已知的，拼一次存起来即可。
pub fn quote_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        let mut url = String::from("https://hq.sinajs.cn/rn=1&list=");
        for it in INSTRUMENTS {
            url.push_str("hf_");
            url.push_str(it.symbol);
            url.push(',');
        }
        url.push_str("fx_susdcny");
        url
    })
}

/// 分时曲线按品种分开缓存，只保留当天的意义，跨夜就别再拿旧数据冒充今天了。
fn intraday_key(symbol: &str) -> String {
    format!("intraday-{symbol}")
}

/// 日 K 自带日期，陈旧一份也只是少一根最新蜡烛。
fn daily_key(symbol: &str) -> String {
    format!("daily-{symbol}")
}

/// 断网时最多回退多久的缓存。
const QUOTE_MAX_AGE: Duration = Duration::from_secs(30 * 60);
const INTRADAY_MAX_AGE: Duration = Duration::from_secs(6 * 60 * 60);
const DAILY_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// 带来源标记的结果：`cached` 为真表示这次用的是本地缓存，不是网络。
#[derive(Clone, Debug)]
pub struct Fetched<T> {
    pub value: T,
    pub cached: bool,
}

impl<T> Fetched<T> {
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Fetched<U> {
        Fetched {
            value: f(self.value),
            cached: self.cached,
        }
    }
}

/// 一个时点的报价，单位见所属 [`Instrument::unit`]。
#[derive(Clone, Debug, Default)]
pub struct Quote {
    pub price: f64,
    pub prev_close: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub date: String,
    pub time: String,
}

impl Quote {
    /// 涨跌额；没有昨收数据时返回 `None`。
    pub fn change(&self) -> Option<f64> {
        (self.prev_close > 0.0 && self.price > 0.0).then(|| self.price - self.prev_close)
    }

    /// 涨跌幅（百分比）。
    pub fn change_pct(&self) -> Option<f64> {
        self.change()
            .map(|c| c / self.prev_close * 100.0)
            .filter(|_| self.prev_close > 0.0)
    }
}

/// 分时曲线上的一个点（分钟级）。
#[derive(Clone, Debug)]
pub struct MinutePoint {
    /// 交易日（`YYYY-MM-DD`）。接口只在首行给出日期，解析时补到每个点上，
    /// 存档要按它和 `time` 拼出完整时间戳。
    pub date: String,
    pub time: String,
    pub price: f64,
}

/// 一根日 K。
#[derive(Clone, Debug)]
pub struct Candle {
    pub date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

/// 一次批量取回的实时行情：全部品种的报价 + 人民币汇率（同一个响应里返回）。
#[derive(Clone, Debug, Default)]
pub struct Quotes {
    /// 只有拿到有效报价的品种才在里面（休市返回 0 的品种会被跳过）。
    pub quotes: HashMap<&'static str, Quote>,
    /// 拿不到汇率就为 `None`，界面退化为只显示原币价格。
    pub usdcny: Option<f64>,
}

/// 建一个复用的 HTTP agent：限制总超时，避免断网时后台线程卡死。
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .user_agent(USER_AGENT)
        .build()
        .into()
}

/// 先走网络，失败再回退到本地缓存。
///
/// 成功的响应会原样落盘（只存能解析成功的，免得把错误页也缓存下来）；
/// 网络失败时用 `max_age` 以内的缓存顶上，并在结果里标记 `cached`。
fn with_cache<T>(
    cache: &Cache,
    key: &str,
    max_age: Duration,
    fetch: impl FnOnce() -> Result<String, String>,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<Fetched<T>, String> {
    let fetch_err = match fetch() {
        Ok(text) => match parse(&text) {
            Ok(value) => {
                cache.write(key, &text);
                return Ok(Fetched {
                    value,
                    cached: false,
                });
            }
            Err(err) => err,
        },
        Err(err) => err,
    };

    match cache
        .read_fresh(key, max_age)
        .and_then(|text| parse(&text).ok())
    {
        Some(value) => Ok(Fetched {
            value,
            cached: true,
        }),
        None => Err(fetch_err),
    }
}

fn get_text(agent: &ureq::Agent, url: &str, referer: Option<&str>) -> Result<String, String> {
    let mut req = agent.get(url);
    if let Some(referer) = referer {
        req = req.header("Referer", referer);
    }
    let mut resp = req.call().map_err(|e| format!("请求失败: {e}"))?;
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(8 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| format!("读取响应失败: {e}"))?;
    // 新浪的 `hq.sinajs.cn` 用 GBK/GB18030 编码，且声明的 MIME 是
    // `application/javascript`，ureq 不会自动解码，必须自己转。
    Ok(decode_text(&bytes))
}

/// 把接口返回的字节转成 UTF-8 字符串。
///
/// 先按 UTF-8 试，失败再按 GB18030（GBK 的超集）解码，两种都不行就宽松替换。
fn decode_text(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    let (text, _, _) = encoding_rs::GB18030.decode(bytes);
    text.into_owned()
}

/// 拉取全部品种的实时报价与汇率。`hq.sinajs.cn` 不带 Referer 会返回 403。
///
/// 30 个品种拼成一次请求，2s 一轮刷新也只有一次往返。
pub fn fetch_quotes(agent: &ureq::Agent, cache: &Cache) -> Result<Fetched<Quotes>, String> {
    with_cache(
        cache,
        QUOTE_KEY,
        QUOTE_MAX_AGE,
        || get_text(agent, quote_url(), Some(SINA_REFERER)),
        parse_quotes,
    )
}

/// 拉取当日分时曲线。
pub fn fetch_intraday(
    agent: &ureq::Agent,
    cache: &Cache,
    instrument: Instrument,
) -> Result<Fetched<Vec<MinutePoint>>, String> {
    with_cache(
        cache,
        &intraday_key(instrument.symbol),
        INTRADAY_MAX_AGE,
        || get_text(agent, instrument.intraday_url, None),
        parse_intraday,
    )
}

/// 拉取日 K 线。接口会返回全量历史，调用方按需裁剪（界面只画最近几百根，
/// 存档则保留全量）。
pub fn fetch_daily(
    agent: &ureq::Agent,
    cache: &Cache,
    instrument: Instrument,
) -> Result<Fetched<Vec<Candle>>, String> {
    with_cache(
        cache,
        &daily_key(instrument.symbol),
        DAILY_MAX_AGE,
        || get_text(agent, instrument.daily_url, None),
        parse_daily,
    )
}

// ---------------------------------------------------------------- 缓存读取
//
// 供启动时同步调用：界面先在本地缓存上画出上一份行情，再等后台刷新。

/// 从缓存读一份仍在有效期内的批量报价。
pub fn cached_quotes(cache: &Cache) -> Option<Quotes> {
    let text = cache.read_fresh(QUOTE_KEY, QUOTE_MAX_AGE)?;
    parse_quotes(&text).ok()
}

/// 从缓存读分时曲线。
pub fn cached_intraday(cache: &Cache, instrument: Instrument) -> Option<Vec<MinutePoint>> {
    let text = cache.read_fresh(&intraday_key(instrument.symbol), INTRADAY_MAX_AGE)?;
    parse_intraday(&text).ok()
}

/// 从缓存读日 K，最近 `keep` 根。
pub fn cached_daily(cache: &Cache, instrument: Instrument, keep: usize) -> Option<Vec<Candle>> {
    let mut candles = cached_daily_all(cache, instrument)?;
    if candles.len() > keep {
        candles.drain(..candles.len() - keep);
    }
    Some(candles)
}

/// 从缓存读日 K 的**全量**历史。
///
/// 接口一次会返回十几年的数据，界面上只用最近几百根（[`cached_daily`]），
/// 但存档（[`crate::archive`]）要留全：历史一旦丢了就补不回来。
/// 这里不设有效期——日 K 自带日期，旧一份也只是少几根最新蜡烛。
pub fn cached_daily_all(cache: &Cache, instrument: Instrument) -> Option<Vec<Candle>> {
    let text = cache.read(&daily_key(instrument.symbol))?.0;
    parse_daily(&text).ok()
}

// ---------------------------------------------------------------- 解析

/// 把 `var hq_str_xxx="a,b,c";` 拆成字段数组。
///
/// 一次响应里有多行（每个品种一行、汇率一行），所以：
/// - 变量名后面必须紧跟 `=`，否则 `hq_str_hf_S` 会匹配到 `hq_str_hf_SI`；
/// - 结束引号从起始引号往后找，不能用 `rfind`，那会跳到最后一个品种上。
fn sina_quote_fields<'a>(text: &'a str, var: &str) -> Option<Vec<&'a str>> {
    let start = text
        .match_indices(var)
        .find_map(|(at, _)| text[at + var.len()..].starts_with('=').then_some(at))?;
    let rest = &text[start..];
    let open = rest.find('"')? + 1;
    let close = rest[open..].find('"')? + open;
    if close <= open {
        return None;
    }
    Some(rest[open..close].split(',').collect())
}

/// 解析批量报价响应。一个品种都解析不出时视为失败，好让调用方回退到缓存。
fn parse_quotes(text: &str) -> Result<Quotes, String> {
    let quotes: HashMap<&'static str, Quote> = INSTRUMENTS
        .iter()
        .filter_map(|it| parse_spot(text, *it).map(|q| (it.symbol, q)))
        .collect();
    if quotes.is_empty() {
        return Err("无法解析任何品种的实时报价".to_string());
    }
    Ok(Quotes {
        quotes,
        usdcny: parse_usdcny(text),
    })
}

fn parse_spot(text: &str, instrument: Instrument) -> Option<Quote> {
    // 字段：[0]最新价 [1]昨收 [2]买价 [3]卖价 [4]最高 [5]最低
    //       [6]时间 [7]昨结算 [8]今开 [12]日期 [13]名称
    let f = sina_quote_fields(text, instrument.quote_var)?;
    let at = |i: usize| f.get(i).and_then(|s| s.trim().parse::<f64>().ok());
    let price = at(0)?;
    if price <= 0.0 {
        // 休市时新浪返回 0，视为无有效报价。
        return None;
    }
    Some(Quote {
        price,
        // 部分品种（如纽约黄金、原油）的 [1] 昨收是空的，只有 [7] 昨结算，
        // 缺 [1] 时用它兜底，否则涨跌额会一直显示不出来。
        prev_close: at(1).filter(|v| *v > 0.0).or_else(|| at(7)).unwrap_or(0.0),
        open: at(8).unwrap_or(0.0),
        high: at(4).unwrap_or(0.0),
        low: at(5).unwrap_or(0.0),
        date: f.get(12).unwrap_or(&"").trim().to_string(),
        time: f.get(6).unwrap_or(&"").trim().to_string(),
    })
}

fn parse_usdcny(text: &str) -> Option<f64> {
    // 字段：[0]时间 [1]买价 [2]卖价 [3]昨收 ... [8]最新价
    let f = sina_quote_fields(text, "hq_str_fx_susdcny")?;
    let at = |i: usize| f.get(i).and_then(|s| s.trim().parse::<f64>().ok());
    let bid = at(1).filter(|r| *r > 1.0);
    let last = at(8).filter(|r| *r > 1.0);
    bid.or(last)
}

/// JSONP 包装：`/*<script>...*/\nvar t=(<json>);`
fn jsonp_payload(text: &str) -> Option<&str> {
    let open = text.find("({").or_else(|| text.find("(["))?;
    let close = text.rfind("})").or_else(|| text.rfind("])"))? + 1;
    (close > open).then(|| &text[open + 1..close])
}

fn parse_intraday(text: &str) -> Result<Vec<MinutePoint>, String> {
    let payload = jsonp_payload(text).ok_or("分时数据不是 JSONP 格式")?;
    // 接口字段名是 `minLine_1d`，serde 里用 rename 更啰嗦，这里手动取。
    let value: serde_json::Value =
        serde_json::from_str(payload).map_err(|e| format!("分时 JSON 解析失败: {e}"))?;
    let rows = value
        .get("minLine_1d")
        .and_then(|v| v.as_array())
        .ok_or("分时数据缺少 minLine_1d")?;

    let mut points = Vec::with_capacity(rows.len());
    // 日期口径：行尾的完整时间戳（`2026-10-28 21:00:00`）才是行情真正的时间，
    // 首行 [0] 那列是**交易日**，夜盘品种（马棕油、日橡胶）会比自然日晚一天，
    // 所以优先用完整时间戳，拿不到才退回首行的交易日 + 该行时间。
    let mut trade_date = String::new();
    for row in rows {
        let Some(cells) = row.as_array() else {
            continue;
        };
        // 首行 10 列：[0]交易日 [4]时间 [5]价格 [9]完整时间戳
        // 其后 6 列：[0]时间 [1]价格 [5]完整时间戳
        let (time, price, stamp) = if cells.len() >= 10 {
            if let Some(day) = cells.first().and_then(|v| v.as_str()) {
                if !day.is_empty() {
                    trade_date = day.to_string();
                }
            }
            (cells.get(4), cells.get(5), cells.last())
        } else {
            (cells.first(), cells.get(1), cells.last())
        };
        let (Some(time), Some(price)) = (time, price) else {
            continue;
        };
        let (Some(time), Ok(price)) = (
            time.as_str().map(str::to_string),
            price.as_str().unwrap_or_default().parse::<f64>(),
        ) else {
            continue;
        };
        // 完整时间戳拆成日期 + 时间；拆不出来就用行的时刻，日期回落到交易日。
        let (date, time) = stamp
            .and_then(|v| v.as_str())
            .and_then(|stamp| {
                let (date, rest) = stamp.split_once(' ')?;
                let time = rest.get(..5)?; // `HH:MM`
                (!date.is_empty() && time.len() == 5).then(|| (date.to_string(), time.to_string()))
            })
            .unwrap_or_else(|| (trade_date.clone(), time));
        if price > 0.0 {
            // 认不出日期也照样画图，只是存档时会被跳过（见 [`crate::archive`]）。
            points.push(MinutePoint { date, time, price });
        }
    }
    if points.is_empty() {
        return Err("分时数据为空（可能非交易时段）".to_string());
    }
    Ok(points)
}

#[derive(Deserialize)]
struct DailyRow {
    date: String,
    open: String,
    high: String,
    low: String,
    close: String,
}

fn parse_daily(text: &str) -> Result<Vec<Candle>, String> {
    let payload = jsonp_payload(text).ok_or("日 K 数据不是 JSONP 格式")?;
    let rows: Vec<DailyRow> =
        serde_json::from_str(payload).map_err(|e| format!("日 K JSON 解析失败: {e}"))?;
    let mut candles = Vec::with_capacity(rows.len());
    for row in rows {
        let num = |s: &str| s.trim().parse::<f64>().ok().filter(|v| *v > 0.0);
        let (Some(open), Some(high), Some(low), Some(close)) = (
            num(&row.open),
            num(&row.high),
            num(&row.low),
            num(&row.close),
        ) else {
            continue;
        };
        candles.push(Candle {
            date: row.date,
            open,
            high,
            low,
            close,
        });
    }
    if candles.is_empty() {
        return Err("日 K 数据为空".to_string());
    }
    Ok(candles)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试里固定用伦敦金作为样本品种。
    const XAU: Instrument = instrument!("XAU", "伦敦金（现货黄金）", "美元/盎司");

    const QUOTE_SAMPLE: &str = concat!(
        "var hq_str_hf_XAU=\"4046.66,4060.130,4046.66,4047.01,4070.74,4044.61,",
        "11:19:00,4060.13,4061.17,0,0,0,2026-10-28,伦敦金（现货黄金）\";\n",
        "var hq_str_fx_susdcny=\"10:02:01,6.6680000000,6.6681000000,6.6670000000,",
        "34.0000000000,6.6672000000,6.6701000000,6.6667000000,6.6680000000,在岸人民币\";\n",
    );

    #[test]
    fn decodes_gbk_response_as_utf8() {
        // 真实响应里名称是 GBK 编码（"伦敦金（现货黄金）"），直接当 UTF-8 读会失败。
        let (gbk, _, _) = encoding_rs::GB18030.encode(&QUOTE_SAMPLE);
        assert!(std::str::from_utf8(&gbk).is_err());
        let text = decode_text(&gbk);
        assert_eq!(text, QUOTE_SAMPLE);
        assert_eq!(parse_spot(&text, XAU).expect("quote").price, 4046.66);
        assert_eq!(parse_usdcny(&text).expect("rate"), 6.668);
    }

    #[test]
    fn keeps_utf8_response_as_is() {
        assert_eq!(decode_text(QUOTE_SAMPLE.as_bytes()), QUOTE_SAMPLE);
    }

    #[test]
    fn parses_spot_quote() {
        let quote = parse_spot(QUOTE_SAMPLE, XAU).expect("quote");
        assert_eq!(quote.price, 4046.66);
        assert_eq!(quote.prev_close, 4060.13);
        assert_eq!(quote.open, 4061.17);
        assert_eq!(quote.high, 4070.74);
        assert_eq!(quote.low, 4044.61);
        assert_eq!(quote.time, "11:19:00");
        assert_eq!(quote.date, "2026-10-28");
        // 4046.66 - 4060.13 = -13.47
        assert!((quote.change().unwrap() + 13.47).abs() < 1e-9);
    }

    #[test]
    fn parses_usdcny_rate() {
        let rate = parse_usdcny(QUOTE_SAMPLE).expect("rate");
        assert!((rate - 6.668).abs() < 1e-9);
    }

    #[test]
    fn rejects_zero_price_when_market_closed() {
        let closed = "var hq_str_hf_XAU=\"0.000,4060.130,0.000,0.000,0.000,0.000,\
                      00:00:00,4060.13,4061.17,0,0,0,2026-10-28,伦敦金\";";
        assert!(parse_spot(closed, XAU).is_none());
    }

    #[test]
    fn falls_back_to_settlement_when_prev_close_missing() {
        // COMEX 品种的真实响应：[1] 为空，[7] 是昨结算，[13] 才是中文名。
        let text = concat!(
            "var hq_str_hf_GC=\"4066.068,,4064.400,4064.600,4084.100,4040.500,",
            "17:15:06,4074.600,4073.600,0,2,1,2026-10-28,纽约黄金,0\";",
        );
        let gc = Instrument::find("GC").expect("GC");
        let quote = parse_spot(text, gc).expect("quote");
        assert_eq!(quote.price, 4066.068);
        assert_eq!(quote.prev_close, 4074.600);
        assert_eq!(quote.open, 4073.600);
        assert!(quote.change().expect("change") < 0.0);
    }

    #[test]
    fn parses_intraday_jsonp() {
        let body = concat!(
            "/*<script>location.href='//sina.com';</script>*/\n",
            "var t=(",
            r#"{"minLine_1d":[["2026-10-28","4060.130","LIFFE","","06:00","4062.270","0","0","4061.9","2026-10-28 06:00:00"],"#,
            r#"["06:01","4061.670","0","0","4061.9","2026-10-28 06:01:00"]]}"#,
            ");",
        );
        let points = parse_intraday(body).expect("intraday");
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].date, "2026-10-28");
        assert_eq!(points[0].time, "06:00");
        assert_eq!(points[0].price, 4062.270);
        assert_eq!(points[1].date, "2026-10-28");
        assert_eq!(points[1].time, "06:01");
        assert_eq!(points[1].price, 4061.670);
    }

    #[test]
    fn intraday_uses_session_time_over_trade_date() {
        // 马棕油的真实响应：首列是交易日 2026-10-29，但夜盘实际发生在 10-28 21:00。
        // 存档按完整时间戳走，才不会把 10-28 的行情记到 10-29 头上。
        let body = concat!(
            "/*<script>location.href='//sina.com';</script>*/\n",
            "var t=(",
            r#"{"minLine_1d":[["2026-10-29","4685.000","bmd","","21:00","4697.160","389","74560","4694.728","2026-10-28 21:00:00"],"#,
            r#"["21:01","4700.200","232","74560","4696.515","2026-10-28 21:01:00"]]}"#,
            ");",
        );
        let points = parse_intraday(body).expect("intraday");
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].date, "2026-10-28");
        assert_eq!(points[0].time, "21:00");
        assert_eq!(points[1].date, "2026-10-28");
        assert_eq!(points[1].time, "21:01");
    }

    #[test]
    fn parses_daily_jsonp() {
        let body = concat!(
            "/*<script>location.href='//sina.com';</script>*/\n",
            "var t=([",
            r#"{"date":"2026-10-27","open":"4030.800","high":"4079.400","low":"4001.600","close":"4073.100","volume":"0","position":"0","s":"0.000","settlement":"0"},"#,
            r#"{"date":"2026-10-28","open":"4073.100","high":"4084.100","low":"4058.200","close":"4060.600","volume":"0","position":"0","s":"0.000","settlement":"0"}"#,
            "]);",
        );
        let candles = parse_daily(body).expect("daily");
        assert_eq!(candles.len(), 2);
        assert_eq!(candles[1].date, "2026-10-28");
        assert_eq!(candles[1].high, 4084.100);
    }

    #[test]
    fn instruments_bind_symbols_into_urls() {
        let xau = Instrument::find("XAU").expect("XAU");
        assert_eq!(xau.name, "伦敦金（现货黄金）");
        assert_eq!(xau.quote_var, "hq_str_hf_XAU");
        assert_eq!(
            xau.intraday_url,
            "https://stock2.finance.sina.com.cn/futures/api/jsonp.php/var%20t=/GlobalFuturesService.getGlobalFuturesMinLine?symbol=XAU"
        );
        assert_eq!(
            xau.daily_url,
            "https://stock2.finance.sina.com.cn/futures/api/jsonp.php/var%20t=/GlobalFuturesService.getGlobalFuturesDailyKLine?symbol=XAU"
        );

        let cl = Instrument::find("CL").expect("CL");
        assert_eq!(cl.daily_url, "https://stock2.finance.sina.com.cn/futures/api/jsonp.php/var%20t=/GlobalFuturesService.getGlobalFuturesDailyKLine?symbol=CL");
    }

    #[test]
    fn batch_quote_url_lists_every_instrument() {
        let url = quote_url();
        for it in INSTRUMENTS {
            assert!(
                url.contains(&format!("hf_{},", it.symbol)),
                "{} 不在批量报价 URL 里",
                it.symbol
            );
        }
        assert!(url.ends_with(",fx_susdcny"));
    }

    #[test]
    fn parses_every_instrument_from_one_response() {
        // 组合样例：`S` / `SI` / `SND` 这种前缀重合的代码必须各归各家，
        // 而结束引号也不能跨行串到后面的品种上。
        let text = concat!(
            "var hq_str_hf_S=\"1247.275,,1247.250,1247.500,1247.500,1241.750,",
            "17:53:21,1243.000,1243.250,0,28,114,2026-10-28,美国大豆,0\";\n",
            "var hq_str_hf_SI=\"53.888,,53.895,53.905,54.595,53.765,",
            "17:53:22,54.513,54.485,0,1,3,2026-10-28,纽约白银,0\";\n",
            "var hq_str_hf_C=\"450.778,,450.500,450.750,451.000,449.000,",
            "17:53:05,448.750,449.000,0,359,84,2026-10-28,美国玉米,0\";\n",
            "var hq_str_fx_susdcny=\"17:52:01,6.6668000000,6.6669000000,6.6670000000,",
            "42.0000000000,6.6672000000,6.6701000000,6.6659000000,6.6669000000,在岸人民币\";\n",
        );
        let quotes = parse_quotes(text).expect("quotes");
        assert_eq!(quotes.quotes.len(), 3);
        assert_eq!(quotes.quotes["S"].price, 1247.275);
        assert_eq!(quotes.quotes["SI"].price, 53.888);
        assert_eq!(quotes.quotes["C"].price, 450.778);
        assert!((quotes.usdcny.expect("rate") - 6.6668).abs() < 1e-9);
    }

    #[test]
    fn rejects_batch_response_without_any_quote() {
        assert!(parse_quotes("var hq_str_fx_susdcny=\"...\";").is_err());
    }

    #[test]
    fn instrument_table_is_well_formed() {
        // 代码不能重复，否则缓存文件会互相覆盖。
        let mut seen = std::collections::HashSet::new();
        for it in INSTRUMENTS {
            assert!(seen.insert(it.symbol), "重复的品种代码: {}", it.symbol);
            assert!(!it.name.is_empty(), "{} 缺少中文名", it.symbol);
            assert!(!it.unit.is_empty(), "{} 缺少报价单位", it.symbol);
            // URL 必须真的绑上了这个品种，而不是留下模板占位符。
            assert!(it.daily_url.ends_with(&format!("symbol={}", it.symbol)));
            assert!(it.intraday_url.ends_with(&format!("symbol={}", it.symbol)));
        }
        assert_eq!(INSTRUMENTS.len(), 30);
        assert!(Instrument::find("NOPE").is_none());
    }

    #[test]
    fn every_instrument_belongs_to_exactly_one_category() {
        // 分组导航的计数直接来自 `Category::instruments()`，所以六个分组必须把
        // 全部品种刚好覆盖一遍，不能漏也不能重。
        let mut seen = std::collections::HashSet::new();
        for category in Category::ALL {
            for instrument in category.instruments() {
                assert_eq!(instrument.category(), category);
                assert!(
                    seen.insert(instrument.symbol),
                    "{} 被分到多个组",
                    instrument.symbol
                );
            }
        }
        assert_eq!(seen.len(), INSTRUMENTS.len());
    }

    #[test]
    fn categories_match_the_expected_markets() {
        let of = |symbol: &str| Instrument::find(symbol).expect(symbol).category();
        assert_eq!(of("S"), Category::Grain);
        assert_eq!(of("FCPO"), Category::Grain);
        assert_eq!(of("CL"), Category::Energy);
        assert_eq!(of("XAU"), Category::Metal);
        assert_eq!(of("NID"), Category::Metal);
        assert_eq!(of("ES"), Category::Index);
        assert_eq!(of("FEF"), Category::Industrial);
        assert_eq!(of("BTC"), Category::Other);
        assert_eq!(Category::ALL.len(), 6);
    }

    fn temp_cache(name: &str) -> crate::cache::Cache {
        let dir =
            std::env::temp_dir().join(format!("hq-feed-cache-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::cache::Cache::at(dir)
    }

    #[test]
    fn caches_successful_response() {
        let cache = temp_cache("success");
        let fetched = with_cache(
            &cache,
            "k",
            Duration::from_secs(60),
            || Ok(QUOTE_SAMPLE.to_string()),
            |text| parse_spot(text, XAU).ok_or_else(|| "parse".to_string()),
        )
        .expect("fetched");
        assert!(!fetched.cached);
        assert_eq!(fetched.value.price, 4046.66);
        assert_eq!(cache.read("k").expect("cached").0, QUOTE_SAMPLE);
        let _ = std::fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn falls_back_to_cache_when_network_fails() {
        let cache = temp_cache("fallback");
        let ok = || {
            with_cache(
                &cache,
                "k",
                Duration::from_secs(60),
                || Ok(QUOTE_SAMPLE.to_string()),
                |text| parse_spot(text, XAU).ok_or_else(|| "parse".to_string()),
            )
        };
        ok().expect("warm up");
        let fetched = with_cache(
            &cache,
            "k",
            Duration::from_secs(60),
            || Err::<String, _>("请求失败: 断网".to_string()),
            |text| parse_spot(text, XAU).ok_or_else(|| "parse".to_string()),
        )
        .expect("fallback");
        assert!(fetched.cached);
        assert_eq!(fetched.value.price, 4046.66);
        let _ = std::fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn does_not_cache_unparseable_response() {
        let cache = temp_cache("garbage");
        let err = with_cache(
            &cache,
            "k",
            Duration::from_secs(60),
            || Ok("<html>502 Bad Gateway</html>".to_string()),
            |text| parse_spot(text, XAU).ok_or_else(|| "无法解析实时报价".to_string()),
        )
        .expect_err("should fail");
        assert_eq!(err, "无法解析实时报价");
        // 错误页不该落盘，也不该覆盖已有的好数据。
        assert!(cache.read("k").is_none());
        let _ = std::fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn skips_stale_cache_when_network_fails() {
        let cache = temp_cache("stale");
        cache.write("k", QUOTE_SAMPLE);
        // 时限为 0 表示"缓存一律不信"，此时只能把网络错误抛出去。
        let err = with_cache(
            &cache,
            "k",
            Duration::from_secs(0),
            || Err::<String, _>("请求失败: 断网".to_string()),
            |text| parse_spot(text, XAU).ok_or_else(|| "parse".to_string()),
        )
        .expect_err("stale cache must not be used");
        assert!(err.contains("断网"));
        let _ = std::fs::remove_dir_all(cache.dir());
    }
}
