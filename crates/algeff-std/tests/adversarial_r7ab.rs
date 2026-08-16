//! R7-A/B 核销轮（迭代 3-A3）——取消传播协议（run_wall_timeout，668b7ed 合入）
//! 合并后这两面的回归盲区补齐：
//!
//! - **R7-A [take→await→put_back 旋转窗口取消泄漏]**：`op_read`/`op_write` 的
//!   轮换型管道路径为 `take_pipe_reader/writer → IO await → put_back`，无「已取
//!   句柄 RAII 守卫」（ArbiterClaimGuard 只覆盖 MutexLock 占坑）。Timeout 取消
//!   飞行中 IO → `put_back` 永不执行：注册表条目已取走、`pipe_reader_fds` 映射
//!   残留陈旧项 → 后续同 fd 一律 NotFound。adversarial_r7.rs §4 F-R7-2 已锁定
//!   **写端**变体（wfd lookup None）；本文件补**读端**变体（§2）与无取消的正向
//!   轮换回归（§3：重复读同变体 + Close 正常）。
//! - **R7-B [Timeout{Fork{MutexLock}} 孤儿分支占坑永久泄漏]**：`run_fork_parallel`
//!   只在**两分支均完成后**才合并 registry/undo 回父；宽限耗尽丢弃 inner 时，
//!   已完成分支（已持锁）的释放 undo 随被丢弃的局部状态一并消失 → arbiter 占坑
//!   + 物理锁永久残留（Replace/recover 够不到）。宽限内 join 路径（分支可取消、
//!   快速返回 → 合并 → rollback_from 执行释放 undo）锁立即可重入（RFC-09 主场景
//!   在 executor.rs `mutex_claim_released_on_timeout_cancel` 为**线性**路径；本
//!   文件补 **Fork 并行**路径两变体，§4）。
//!
//! 判定（与交付报告一致）：**R7-A/B 未核销（仍开放）**——§2/§4 耗尽变体为缺陷
//! 行为锁定（断言当前行为，src/ 冻结不修，CTO 派发修复轮）；§3/§4 join 变体为
//! 正向回归（当前行为符合文档承诺）。
//!
//! 时域门控：取消/宽限测试依赖真实墙钟计时（宽限 500ms、超时 30ms），
//! `virtual-clock` 构建下 Timeout 走 `run_virtual_timeout` post-check 语义（效果
//! 保留、无取消）——与既有宽限测试（adversarial_r7.rs §4）同约定，本文件同样
//! `#[cfg(not(feature = "virtual-clock"))]` 门控。
//!
//! 审计方法：基线 7f7285a（iter3/it3-a3）代码走读（executor.rs 轮换机制 +
//! runtime.rs run_wall_timeout/run_fork_parallel）+ 行为探测 → 断言锁定**当前
//! 行为**（src/ 冻结零改动，只写测试与文档）。

use algeff_core::{
    Action, DataOp, Owned, ReadOnly, ResourceInner, ResourceUsage, Runtime, SysError,
    TypedResource, Value, WriteOnly,
};
use algeff_std::TokioExecutor;

// 时域门控：取消/宽限测试依赖真实墙钟计时，virtual-clock 构建下被 cfg 裁掉；
// 对应导入与辅助函数同门控，避免 VC 构建下未使用告警（CI clippy -D warnings）。
#[cfg(not(feature = "virtual-clock"))]
use std::time::{Duration, Instant};

// ── 本地辅助（src/ 冻结不可改，测试内复制；与既有对抗套件同约定）──────────

fn fd_of(v: &Value) -> u64 {
    match v {
        Value::Fd(f) => *f,
        other => panic!("期望 Fd，得到 {other:?}"),
    }
}

fn pair_of(v: &Value) -> (u64, u64) {
    match v {
        Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("期望 List([Fd, Fd])，得到 {other:?}"),
    }
}

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn ow(fd: u64) -> ResourceUsage {
    TypedResource::<Owned>::new_owned(ResourceInner::Fd(fd)).into_usage()
}

fn syscall(
    op: DataOp,
    resources: Vec<ResourceUsage>,
    next: impl FnOnce(Value) -> Action + Send + 'static,
) -> Action {
    Action::Syscall {
        op,
        resources,
        next: Box::new(next),
    }
}

#[cfg(not(feature = "virtual-clock"))]
fn mutex_lock(id: u64) -> Action {
    syscall(DataOp::MutexLock { id }, vec![], Action::Pure)
}
#[cfg(not(feature = "virtual-clock"))]
fn mutex_unlock(id: u64) -> Action {
    syscall(DataOp::MutexUnlock { id }, vec![], Action::Pure)
}

// ══════════════════════════════════════════════════════════════════════
// §1 辅助：管道三连（PipeOpen → Write → Read）与 UdpBind
// ══════════════════════════════════════════════════════════════════════

/// PipeOpen + 一次 Write + 一次 Read 的联合动作：一次性把数据灌入管道并读回。
/// 返回 (rfd, wfd)。
fn pipe_with_data(rt: &mut Runtime, data: &[u8]) -> (u64, u64) {
    // next 闭包需 'static：先转属主数据再捕获。
    let payload = data.to_vec();
    let v = rt
        .run_blocking(syscall(
            DataOp::PipeOpen {
                flags: Default::default(),
            },
            vec![],
            move |v| {
                let (rfd, wfd) = pair_of(&v);
                syscall(
                    DataOp::Write {
                        fd: wfd,
                        data: payload.clone(),
                    },
                    vec![wr(wfd)],
                    move |_| Action::Pure(Value::List(vec![Value::Fd(rfd), Value::Fd(wfd)])),
                )
            },
        ))
        .unwrap();
    pair_of(&v)
}

#[cfg(not(feature = "virtual-clock"))]
fn udp_bind(rt: &mut Runtime) -> u64 {
    let v = rt
        .run_blocking(syscall(
            DataOp::UdpBind {
                addr: "127.0.0.1:0".parse().unwrap(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    fd_of(&v)
}

// ══════════════════════════════════════════════════════════════════════
// §2 R7-A 缺陷锁定：Timeout 取消**飞行中管道读**（take 后 put_back 前）
// ══════════════════════════════════════════════════════════════════════

/// R7-A 读端变体（写端变体 = adversarial_r7.rs §4 F-R7-2）：
/// 无数据管道 → 读端 `Read` 阻塞于 `take_pipe_reader → rh.read().await`
/// 窗口 → Timeout(30ms) 触发 → 取消广播不打断 executor 内 await → 宽限
/// （CANCEL_JOIN_GRACE=500ms）耗尽 → inner future 被丢弃 → **put_back 永不
/// 执行**。当前行为锁定（缺陷，R7-A 未核销）：
/// - 注册表句柄泄漏：`lookup(rfd) == None`（条目已取走、未放回）；
/// - 执行器 `pipe_reader_fds` 映射残留陈旧项 → 后续同 fd Read / Close 均
///   NotFound（句柄不可寻址）；
/// - undo 栈干净（阻塞读未入栈）、线性标记无残留（Read 不消费标记）。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn r7a_timeout_cancels_inflight_pipe_read_leak_locked() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let (rfd, wfd) = {
        let v = rt
            .run_blocking(syscall(
                DataOp::PipeOpen {
                    flags: Default::default(),
                },
                vec![],
                Action::Pure,
            ))
            .unwrap();
        pair_of(&v)
    };

    // 无数据 → 读端 Read 永久阻塞（对端写端存活）；Timeout 30ms 必触发。
    let inner = syscall(
        DataOp::Read { fd: rfd, len: 16 },
        vec![rd(rfd)],
        Action::Pure,
    );
    let t0 = Instant::now();
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(inner),
            duration: Duration::from_millis(30),
            on_timeout: Box::new(Action::Pure(Value::U64(1))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(1), "on_timeout 生效");
    assert!(
        t0.elapsed() >= Duration::from_millis(450),
        "飞行中管道读不可取消 → 宽限耗尽后丢弃（elapsed={:?}）",
        t0.elapsed()
    );

    // 取消路径干净面：undo 无残留、线性标记无残留（Read 不消费，trivial）。
    assert!(rt.undo_stack().is_empty(), "取消路径不产生 undo");
    assert!(
        rt.registry().check_linear(&rd(rfd)).is_ok(),
        "取消后 Read 声明应通过 A4（线性标记已回滚）"
    );

    // R7-A 缺陷锁定（读端变体）：
    assert!(
        rt.registry().lookup(rfd).is_none(),
        "R7-A 行为锁定：取消飞行中管道读 → 注册表读端句柄泄漏（take 未 put_back）"
    );

    // 执行器映射残留陈旧项 → 同逻辑 fd 不可寻址。
    let r = rt.run_blocking(syscall(
        DataOp::Read { fd: rfd, len: 1 },
        vec![rd(rfd)],
        Action::Pure,
    ));
    assert_eq!(
        r.unwrap_err(),
        SysError::NotFound,
        "R7-A 行为锁定：泄漏后同 fd 重复读应 NotFound（映射陈旧、注册表无条目）"
    );
    let r = rt.run_blocking(syscall(
        DataOp::Close { fd: rfd },
        vec![ow(rfd)],
        Action::Pure,
    ));
    assert_eq!(
        r.unwrap_err(),
        SysError::NotFound,
        "R7-A 行为锁定：泄漏后同 fd Close 应 NotFound（句柄不可寻址）"
    );

    // 写端不受牵连：Close 正常（写端从未被 take）。
    rt.run_blocking(syscall(
        DataOp::Close { fd: wfd },
        vec![ow(wfd)],
        Action::Pure,
    ))
    .unwrap();
    assert!(rt.registry().lookup(wfd).is_none(), "写端已释放");
}

// ══════════════════════════════════════════════════════════════════════
// §3 R7-A 正向回归：轮换机制无取消时正常工作（重复读同变体 + Close）
// ══════════════════════════════════════════════════════════════════════

/// 无取消时轮换机制必须完整：同逻辑 fd 重复读（每次 take→put_back 分配新
/// 注册表 fd）不丢数据、不串流；Close 两端正常释放（无泄漏）。本测试为
/// R7-A 修复后「句柄可寻址」基准的对照面——若修复引入回归（如 put_back
/// 丢失数据/游标错位），此处先红。
#[test]
fn r7a_pipe_rotation_repeated_reads_close_normal() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    // 一次写入 "aaabbb"（Write 每资源至多一次，A4），随后两次 Read 各取 3 字节。
    let (rfd, wfd) = pipe_with_data(&mut rt, b"aaabbb");

    let read3 = |rt: &mut Runtime| -> Vec<u8> {
        let v = rt
            .run_blocking(syscall(
                DataOp::Read { fd: rfd, len: 3 },
                vec![rd(rfd)],
                Action::Pure,
            ))
            .unwrap();
        match v {
            Value::Bytes(b) => b,
            other => panic!("期望 Bytes，得到 {other:?}"),
        }
    };
    assert_eq!(
        read3(&mut rt),
        b"aaa",
        "第一次轮换读（take→put_back→新 fd）"
    );
    assert_eq!(
        read3(&mut rt),
        b"bbb",
        "第二次轮换读（同逻辑 fd 重复读不串流）"
    );
    // 注：轮换后注册表键是每次 put_back 新分配的 fd，逻辑 fd rfd 经
    // 执行器 pipe_reader_fds 映射寻址——「可寻址」由上面第二次 Read 成功证明
    // （若映射陈旧/句柄泄漏，第二次读会 NotFound，见 §2 缺陷锁定）。

    // Close 两端：轮换后 Close 必须正常（映射一致 → Ok；若轮换泄漏映射残留
    // 陈旧项，此处将 NotFound，见 §2）且无 undo（关闭不可逆）。
    rt.run_blocking(syscall(
        DataOp::Close { fd: rfd },
        vec![ow(rfd)],
        Action::Pure,
    ))
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::Close { fd: wfd },
        vec![ow(wfd)],
        Action::Pure,
    ))
    .unwrap();
    assert!(rt.undo_stack().is_empty(), "管道 IO/Close 不产生 undo");

    // Close(Own) 后同 fd 任何 usage 被 A4 Own 终结拒绝（与 rfc07 轮换回归
    // 同约定；两错误面：Own 终结 InvalidInput vs 泄漏 NotFound）。
    let r = rt.run_blocking(syscall(
        DataOp::Read { fd: rfd, len: 1 },
        vec![rd(rfd)],
        Action::Pure,
    ));
    assert_eq!(
        r.unwrap_err(),
        SysError::InvalidInput,
        "Close(Own) 后同 fd 任何 usage 应被 A4 Own 终结拒绝"
    );
}

// ══════════════════════════════════════════════════════════════════════
// §4 R7-B：Timeout{Fork{MutexLock, …}} 两变体
// ══════════════════════════════════════════════════════════════════════

/// R7-B 宽限内 join 路径（Fork 并行变体）：Timeout{Fork{MutexLock(id) →
/// Pure, Sleep(400ms)}}，30ms 超时 → 取消广播 → Sleep 分支经 cancellable_sleep
/// 立即醒来 → 循环顶检查快速返回 → 两分支均在宽限（500ms）内完成 join → 分支
/// undo（含 MutexLock 释放）合并回父 → `rollback_from` 立即执行释放 → **同 id
/// 锁立即可重入**（RFC-09 目标在 Fork 并行路径成立；executor.rs
/// `mutex_claim_released_on_timeout_cancel` 为线性路径的既有覆盖）。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn r7b_timeout_fork_lock_join_path_lock_reentrant() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let id = 90u64;

    let t0 = Instant::now();
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(Action::Fork {
                left: Box::new(mutex_lock(id)),
                right: Box::new(Action::Sequential {
                    current: Box::new(Action::Sleep {
                        duration: Duration::from_millis(400),
                        next: Box::new(Action::Pure),
                    }),
                    next: Box::new(|_| Action::Pure(Value::Unit)),
                }),
                combine: Box::new(|_, _| Action::Pure(Value::Unit)),
            }),
            duration: Duration::from_millis(30),
            on_timeout: Box::new(Action::Pure(Value::U64(2))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(2), "on_timeout 生效");
    assert!(
        t0.elapsed() < Duration::from_millis(400),
        "宽限内 join：Sleep 分支响应取消快速返回（elapsed={:?} < 500ms 宽限）",
        t0.elapsed()
    );

    // 取消传播回滚分支 undo → 锁立即可重入（非永久 WouldBlock）。
    assert_eq!(
        rt.run_blocking(mutex_lock(id)).unwrap(),
        Value::Unit,
        "宽限内 join：Timeout 回滚已合并的分支 undo → 同 id 可立即重入"
    );

    // Replace（recover + reg.clear）清理残余 undo，重复验证不毒化。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(
        rt.run_blocking(mutex_lock(id)).unwrap(),
        Value::Unit,
        "Replace 后同 id 仍可重入"
    );
}

/// R7-B 宽限耗尽路径（缺陷锁定）：Timeout{Fork{MutexLock(id) → Pure,
/// UdpRecvFrom(阻塞、不可取消)}}，30ms 超时 → 取消广播 → MutexLock 分支（已
/// 完成、持锁入槽）的结果因 `run_fork_parallel` 只 await 完 UdpRecvFrom 分支
/// 才合并而滞留于被丢弃的 inner 局部 → 释放 undo 随任务结果丢弃、**从不并入
/// 父** → arbiter 占坑 + 物理锁永久残留。当前行为锁定（缺陷，R7-B 未核销）：
/// - 同 id 后续 MutexLock → WouldBlock（有限重试后，A7 不挂死）；
/// - Replace（recover + reg.clear）后仍 WouldBlock（父 undo 栈无孤儿分支的
///   undo → recover 够不到；reg.clear 不清 executor 的 arbiter/held_locks）——
///   RFC-09「recover 可恢复」对宽限耗尽孤儿分支不成立（登记原文）；
/// - 显式 MutexUnlock 是唯一逃逸通道（经共享 held_locks 停车位取走 guard +
///   幂等 release）→ 之后同 id 可重入（非绝对死锁，但默认蓝图路径永久饥饿）。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn r7b_timeout_fork_lock_grace_exhausted_orphan_leak_locked() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let id = 91u64;
    // 父级 UDP 绑定（&self 型句柄，分支内 lookup 直用无共享冲突），
    // 分支 UdpRecvFrom 无数据 → 永久阻塞（不可取消，宽限必耗尽）。
    let udp = udp_bind(&mut rt);

    let t0 = Instant::now();
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(Action::Fork {
                left: Box::new(mutex_lock(id)),
                right: Box::new(syscall(
                    DataOp::UdpRecvFrom { fd: udp, len: 8 },
                    vec![rd(udp)],
                    Action::Pure,
                )),
                combine: Box::new(|_, _| Action::Pure(Value::Unit)),
            }),
            duration: Duration::from_millis(30),
            on_timeout: Box::new(Action::Pure(Value::U64(3))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(3), "on_timeout 生效");
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_millis(450) && elapsed < Duration::from_millis(1400),
        "宽限耗尽：须耗尽 500ms 宽限（elapsed={elapsed:?}）"
    );

    // 缺陷锁定：孤儿分支持锁占坑 → 同 id WouldBlock（有限重试，非挂死）。
    let r = rt.run_blocking(mutex_lock(id));
    assert_eq!(
        r.unwrap_err(),
        SysError::WouldBlock,
        "R7-B 行为锁定：宽限耗尽孤儿分支占坑残留 → 同 id Lock WouldBlock"
    );

    // Replace（recover + reg.clear）也无法释放（父 undo 栈为空，孤儿 undo 未合并）。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    let r = rt.run_blocking(mutex_lock(id));
    assert_eq!(
        r.unwrap_err(),
        SysError::WouldBlock,
        "R7-B 行为锁定：Replace/recover 后同 id 仍 WouldBlock（孤儿占坑够不到）"
    );

    // 唯一逃逸通道：显式 MutexUnlock（经共享 held_locks 停车位）→ 之后可重入。
    rt.run_blocking(mutex_unlock(id)).unwrap();
    assert_eq!(
        rt.run_blocking(mutex_lock(id)).unwrap(),
        Value::Unit,
        "显式 MutexUnlock 释放孤儿占坑后同 id 可重入（逃逸通道存在）"
    );
}
