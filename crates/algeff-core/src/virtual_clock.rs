//! 逻辑时钟 VirtualClock（pdr.md §5.2 / §12.1，可选 feature `virtual-clock`）。
//!
//! A2 拥有本文件：接入 Sleep/GetTime 的确定性时间重放。

use std::time::Duration;

/// 逻辑时间 = 偏移量。确定性重放时由运行时推进，不依赖墙钟。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VirtualClock {
    offset: Duration,
}

impl VirtualClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn now(&self) -> Duration {
        self.offset
    }

    pub fn advance(&mut self, d: Duration) {
        // 审计 R1 契约-F8 修复：Duration 加法溢出（debug/release 均无条件
        // panic）是用户可控崩溃面（连续 Sleep 的 duration 之和超上限）——
        // 饱和加法，溢出封顶而非 panic。
        self.offset = self.offset.saturating_add(d);
    }

    pub fn set(&mut self, t: Duration) {
        self.offset = t;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_and_set() {
        let mut c = VirtualClock::new();
        assert_eq!(c.now(), Duration::ZERO);
        c.advance(Duration::from_secs(5));
        assert_eq!(c.now(), Duration::from_secs(5));
        c.set(Duration::from_secs(1));
        assert_eq!(c.now(), Duration::from_secs(1));
    }

    /// 审计 R1 契约-F8 修复：Duration 加法溢出（debug/release 均无条件 panic）
    /// 是用户可控崩溃面（virtual-clock 下连续 Sleep 的 duration 之和超上限）
    /// —— advance 改为饱和加法，溢出封顶而非 panic。
    #[test]
    fn advance_saturates_on_overflow() {
        let mut c = VirtualClock::new();
        c.advance(Duration::MAX);
        c.advance(Duration::from_secs(1));
        assert_eq!(c.now(), Duration::MAX, "溢出应饱和封顶而非 panic");
    }
}
