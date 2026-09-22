use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// 上游可重试错误的统一退避参数(对齐通用网关重试与 grok retry)
pub const RETRY_BASE_DELAY: Duration = Duration::from_millis(300);
pub const RETRY_MAX_DELAY: Duration = Duration::from_millis(1500);
pub const RETRY_TOTAL_BUDGET: Duration = Duration::from_secs(3);

/// 52x 错误最大退避时间(对齐 grok MAX_RETRY_BACKOFF,防 Cloudflare 120s 挂死)
pub const CF_EDGE_MAX_RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// +/-20% jitter 抖动(对齐 grok jitter_backoff),打散并发客户端重试风暴
pub fn jitter_backoff(base: Duration) -> Duration {
    use std::hash::{Hash, Hasher};
    static JITTER_SEQ: AtomicU64 = AtomicU64::new(0);

    let base_ms = base.as_millis() as u64;
    if base_ms == 0 {
        return base;
    }
    let jitter_range = base_ms / 5;
    let mut hasher = std::hash::DefaultHasher::new();
    JITTER_SEQ.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    let jitter = if jitter_range > 0 {
        hasher.finish() % (jitter_range * 2 + 1)
    } else {
        0
    };
    Duration::from_millis(base_ms.saturating_sub(jitter_range) + jitter)
}

/// Retry-After 头解析:仅支持秒数形式(HTTP-date 忽略,按退避公式兜底)
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs = raw.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(secs))
}

/// 第 attempt 次失败(0 起)后的等待时长:
/// - 52x/5xx 等边缘错误: Retry-After 钳位到 30s 并加 +/-20% jitter 抖动
/// - 其余/网络错误: base * 2^attempt 指数退避 + jitter
/// - 总预算耗尽返回 None 不再重试。
///
/// 429 不进本函数:调用方对齐 codex 传输层 retry_429: false,快速失败交客户端退避。
pub fn compute_retry_delay(
    attempt: u32,
    started_at: std::time::Instant,
    headers: &reqwest::header::HeaderMap,
    status: Option<reqwest::StatusCode>,
) -> Option<Duration> {
    let elapsed = started_at.elapsed();
    if elapsed >= RETRY_TOTAL_BUDGET {
        return None;
    }

    let is_cf_52x = status
        .map(|s| {
            let c = s.as_u16();
            (520..=529).contains(&c)
        })
        .unwrap_or(false);

    let mut delay = if let Some(retry_after) = parse_retry_after(headers) {
        if is_cf_52x {
            // Cloudflare 52x 往往下发 60-120s，钳位到 30s + 抖动
            jitter_backoff(retry_after.min(CF_EDGE_MAX_RETRY_BACKOFF))
        } else {
            jitter_backoff(retry_after.min(RETRY_MAX_DELAY))
        }
    } else {
        let backoff = RETRY_BASE_DELAY.saturating_mul(1u32 << attempt.min(4));
        jitter_backoff(backoff.min(RETRY_MAX_DELAY))
    };

    // 头给出的等待也不得突破总预算(截断而非放弃,末次机会照试)
    if elapsed + delay > RETRY_TOTAL_BUDGET {
        delay = RETRY_TOTAL_BUDGET - elapsed;
    }
    Some(delay)
}
