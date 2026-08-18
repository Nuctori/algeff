//! 效应开销代数（运行时可审计路线，D-104）。
//!
//! 设计定位：Rust 是"普通类型系统"（无原生线性/依赖类型），编译期
//! coeffect 类型索引（∀(n:Nat) 类型索引）不可行，故开销走**运行时记录
//! 可审计**路线——与义务效应（UndoStack 已是"已发生副作用轨迹"）同源累加。
//! 这对应开销代数化设计文档 v0.1 审计结论的落点 (a)；落点 (b) 编译期类型层
//! 留作 v0.2 研究（本期不实现）。
//!
//! 代数结构（文档 §3/§4）：三原语 read / write / occupy 正交，每维是闭区间
//! `[min, max]`（保守估计不确定性与峰值）；
//! - 顺序组合 = 逐分量区间加法（[l1,u1]+[l2,u2] = [l1+l2, u1+u2]）
//! - 并行组合 = read/write 累加、occupy_peak 取 max（保守竞争假设见 §10，本期
//!   运行时路径仅做顺序累加，Fork 合并见 `UndoStack::append` 调用方）
//! - 条件分支 / `?` 短路 = 走哪条算哪条（运行时自然成立）
//!
//! 不兜底硬件（文档 §2.3）：开销是**逻辑资源单元**（syscall 数、字节量、fd
//! 占用），不是物理时间/磁盘延迟/CPU 周期。"3" 的度量由 `for_op` 明确定义为
//! 各原语的语义计数（文档 B1 修复）。

use crate::action::DataOp;

/// 单一原语的开销维度（闭区间，逻辑资源单元）。
///
/// 命名 `min`/`max` 直接对应文档 §4.1 的 `[l, u]`；`min` 是乐观下界，
/// `max` 是保守上界（最坏估计）。纯计算效应（无 DataOp）开销为 `[0,0]`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Grade {
    pub min: u64,
    pub max: u64,
}

impl Grade {
    /// 零元（[0,0]，单位元，文档 §4.3 零元律）。
    pub const ZERO: Grade = Grade { min: 0, max: 0 };

    /// 单点区间 [n,n]（参数化确定的开销，如 read{n}）。
    pub const fn point(n: u64) -> Grade {
        Grade { min: n, max: n }
    }

    /// 上限取 max（保守上界，参数化长度）。
    pub fn with_max(self, max: u64) -> Grade {
        Grade {
            min: self.min,
            max: self.max.max(max),
        }
    }

    /// 顺序组合：逐分量区间加法（文档 §4.1）。
    pub fn plus(self, other: Grade) -> Grade {
        Grade {
            min: self.min.saturating_add(other.min),
            max: self.max.saturating_add(other.max),
        }
    }

    /// 并行峰值（保守竞争假设取 max，文档 §10 倾向）：占用维度用。
    pub fn peak(self, other: Grade) -> Grade {
        Grade {
            min: self.min.max(other.min),
            max: self.max.max(other.max),
        }
    }
}

/// 三原语开销向量（文档 §3/§7.1 `EffectCost = {read, write, occupy}`）。
///
/// 量纲隔离（文档 §2.1/§4）：三维独立、不可跨维直接运算。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EffectCost {
    pub read: Grade,
    pub write: Grade,
    pub occupy: Grade,
}

impl EffectCost {
    /// 零开销（纯计算 / 未执行）。
    pub const ZERO: EffectCost = EffectCost {
        read: Grade::ZERO,
        write: Grade::ZERO,
        occupy: Grade::ZERO,
    };

    /// 顺序组合：逐分量区间加法（文档 §7.2 顺序组合规则）。
    pub fn plus(&self, other: &EffectCost) -> EffectCost {
        EffectCost {
            read: self.read.plus(other.read),
            write: self.write.plus(other.write),
            occupy: self.occupy.plus(other.occupy),
        }
    }

    /// 净占用（[net, net]）——occupy 的净变化（文档 §3.1 有向 occupy）。
    pub fn occupy_net(&self) -> u64 {
        self.occupy.max
    }

    /// 峰值占用（[peak, peak]）——occupy 的上界（文档 §6.2 Scope Peak）。
    pub fn occupy_peak(&self) -> u64 {
        self.occupy.max
    }

    /// 从 `DataOp` 静态派生开销（文档 §5.1 Unix 系统调用覆盖）。
    ///
    /// 度量定义（文档 B1 修复——"3" 的语义）：
    /// - `read`：观察外部状态（读/查询/输入/信号接收）——`read{1}` 计一次
    ///   观察效应；带长度的读以字节量为 `max`（保守上界）。
    /// - `write`：修改外部状态（写/更新/输出/信号投递/创建句柄）——`write{1}`
    ///   计一次修改效应；带数据的写以字节量为 `max`。
    /// - `occupy`：资源生命周期变化（open/socket/malloc→+1，close/free→-1）；
    ///   负占用以 0 下界（净占用不得低于 0，文档 §3.1）。
    ///
    /// 注意：本函数**只刻画静态上界**（effect 自带 cost sketch），真实开销
    /// 由运行时 `UndoStack` 累加实测（文档 §1 承诺一致：可静态推导=上界）。
    pub fn for_op(op: &DataOp) -> EffectCost {
        use DataOp::*;
        match op {
            // ── 纯观察（read 维度，无占用）──
            Read { len, .. } => EffectCost {
                read: Grade::point(1).with_max(*len as u64),
                ..Default::default()
            },
            Stat { .. } | ReadDir { .. } | GetTime | TcpAccept { .. } => EffectCost {
                read: Grade::point(1),
                ..Default::default()
            },
            // 带长度的接收：read 维度以字节量为 max（保守上界，与 Read 一致，文档 B1）。
            TcpRead { len, .. } => EffectCost {
                read: Grade::point(1).with_max(*len as u64),
                ..Default::default()
            },
            // 带长度的接收：read 维度以字节量为 max（保守上界）。
            UdpRecvFrom { len, .. } => EffectCost {
                read: Grade::point(1).with_max(*len as u64),
                ..Default::default()
            },
            // ── 纯修改（write 维度，无占用）──
            Write { data, .. } => EffectCost {
                write: Grade::point(1).with_max(data.len() as u64),
                ..Default::default()
            },
            TcpWrite { data, .. } | UdpSendTo { data, .. } => EffectCost {
                write: Grade::point(1).with_max(data.len() as u64),
                ..Default::default()
            },
            TcpShutdown { .. } => EffectCost {
                write: Grade::point(1),
                ..Default::default()
            },
            Seek { .. }
            | Truncate { .. }
            | Rename { .. }
            | Mkdir { .. }
            | Chmod { .. }
            | Chown { .. }
            | SendFile { .. } => EffectCost {
                write: Grade::point(1),
                ..Default::default()
            },
            // ── 创建句柄（write + occupy{+1}，文档 §5.1）──
            Open { .. }
            | TcpBind { .. }
            | TcpConnect { .. }
            | UdpBind { .. }
            | PipeOpen { .. }
            | Spawn { .. } => EffectCost {
                write: Grade::point(1),
                occupy: Grade::point(1),
                ..Default::default()
            },
            // ── 释放句柄（write 维度，净占用不降为负）──
            Close { .. } | Dup { .. } | Dup2 { .. } | Rmdir { .. } | Unlink { .. } => EffectCost {
                write: Grade::point(1),
                ..Default::default()
            },
            // ── 不可逆投递/信号（write 维度，无占用）──
            Kill { .. }
            | Wait { .. }
            | SendSignal { .. }
            | MutexLock { .. }
            | MutexUnlock { .. } => EffectCost {
                write: Grade::point(1),
                ..Default::default()
            },
            // ── 内存映射：read（观察）+ occupy{+size}（文档 §5.1 mmap）──
            Mmap { len, .. } => EffectCost {
                read: Grade::point(1),
                occupy: Grade::point(*len as u64),
                ..Default::default()
            },
            Munmap { len, .. } => EffectCost {
                write: Grade::point(1),
                occupy: Grade::point(*len as u64),
                ..Default::default()
            },
        }
    }
}

/// 成本预算（文档 §7.3 生命周期/泄漏检查的最小工业落地）。
///
/// `MAX_IO_LEN`（runtime.rs:27）本就是开销边界（字节量上限），本期将其泛化形态
/// 显式命名：`CostBudget` 是各维度的拒绝阈值；超出即 `Err`（与 `MAX_IO_LEN`
/// 同构，只是广义到三原语）。本期仅暴露结构，拒绝检查留作增量接口
/// （不引入新失败路径，保持最小原型可审计）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CostBudget {
    pub read: u64,
    pub write: u64,
    pub occupy: u64,
}

impl CostBudget {
    /// 不限制（所有维度 0 阈值 = 放行）。
    pub const UNBOUNDED: CostBudget = CostBudget {
        read: u64::MAX,
        write: u64::MAX,
        occupy: u64::MAX,
    };

    /// 是否超出预算：`max` 分量越界即超（保守）。
    pub fn exceeded_by(&self, cost: &EffectCost) -> bool {
        cost.read.max > self.read || cost.write.max > self.write || cost.occupy.max > self.occupy
    }
}
