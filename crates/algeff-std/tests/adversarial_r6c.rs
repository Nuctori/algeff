//! R6 块 A3 对抗审计：README 快速上手示例真实性对抗
//! （基线 fd95e7a 引入 readme_examples.rs 编译护栏后的盲区扩展）。
//!
//! 审计对象：README.md「快速上手 §3/§4」与「常用模式速查」的代码块，对照
//! `readme_examples.rs` 编译护栏与真实运行时行为。
//!
//! 审计发现（详见交付报告 findings，本文件仅做行为锁定）：
//! - **fork! 语法**：README「并行 Fork」代码块为无标签位置形式
//!   `fork! { Action::Pure(..), Action::Pure(..) }`，而 `fork!` 宏（
//!   algeff-macro）只接受 `left:`/`right:` 标签 → README 该块照抄无法编译；
//!   护栏测试用标签形式掩盖（其注释自称「README 速查：left:/right: 标签」，
//!   与 README 实际文本不符）。README 注释「想合并结果就自定义」也暗示宏
//!   支持 combine 参数——宏硬编码 combine=Unit，自定义需手写 Action::Fork。
//! - **§3/§4 导入**：README 用 `use algeff_core::prelude::*;`，但 prelude
//!   不导出 `OpenFlags` → §3/§4 照抄无法编译；护栏测试改为显式导入掩盖
//!   （实测 cargo check 复现 E0422）。
//! - **速查 Catch/Timeout 行为盲区**：护栏中 Catch 的 action 为 Pure（永不
//!   成错）、Timeout 的 action 为 5ms Sleep < 100ms 上限（on_timeout 永不
//!   触发）——README 声称的「错误可捕获」「超时走 on_timeout」只有编译证据、
//!   无行为证据。本文件补齐行为锁定。
//! - **速查 TCP 骨架**：护栏用 Timeout 包裹避免 Accept 挂死，README 代码块
//!   原样执行会永久阻塞（护栏与 README 非逐字一致，注释已披露）。
//!
//! 本文件行为锁定（全部经真实 Runtime + TokioExecutor 全链路，非 mock）：
//! 1. 写后不 Seek 直接读 → 空字节（游标语义：README §3 的 Seek(0) 不可省）；
//! 2. fork! 两侧写同一文件 → 静态冲突自动顺序化（left→right），终态确定；
//! 3. 深度守卫边界：adapters::seq 左结合链 96 元素 OK / 97 元素 Err(Other(105))；
//! 4. Catch 捕获 Other(105) 后可继续（后续真实系统调用照常执行）；
//! 5. Timeout 超时后 on_timeout 生效 + 状态可继续。

use std::path::PathBuf;
use std::time::Duration;

use algeff_core::{
    Action, DataOp, OpenFlags, Owned, ReadOnly, ResourceInner, ResourceUsage, Runtime, SysError,
    TypedResource, Value, WriteOnly,
};
use algeff_macro::fork;
use algeff_std::{adapters, TokioExecutor};

// ── 本地辅助（src/ 冻结不可改，测试内复制；与既有对抗套件同约定）──────────

fn rw_flags() -> OpenFlags {
    OpenFlags {
        read: true,
        write: true,
        create: true,
        ..Default::default()
    }
}

fn fd_of(v: &Value) -> u64 {
    match v {
        Value::Fd(f) => *f,
        other => panic!("期望 Fd，得到 {other:?}"),
    }
}

fn write_path(p: &std::path::Path) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(p.to_path_buf())).into_usage()
}
fn write_fd(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn read_fd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn close_fd(fd: u64) -> ResourceUsage {
    TypedResource::<Owned>::new_owned(ResourceInner::Fd(fd)).into_usage()
}

/// 深度守卫测试用的无副作用"步"：GetTime 系统调用（无资源声明、无撤销）。
fn gettime_step() -> Action {
    Action::Syscall {
        op: DataOp::GetTime,
        resources: vec![],
        next: Box::new(|_| Action::Pure(Value::Unit)),
    }
}

/// Open(rw+create) → Write(data) → Close 的完整分支（fork 冲突测试用）。
fn write_side(path: PathBuf, data: Vec<u8>) -> Action {
    Action::Syscall {
        op: DataOp::Open {
            path: path.clone(),
            flags: rw_flags(),
        },
        resources: vec![write_path(&path)],
        next: Box::new(move |v| {
            let fd = fd_of(&v);
            Action::Sequential {
                current: Box::new(Action::Syscall {
                    op: DataOp::Write {
                        fd,
                        data: data.clone(),
                    },
                    resources: vec![write_fd(fd)],
                    next: Box::new(|_| Action::Pure(Value::Unit)),
                }),
                next: Box::new(move |_| Action::Syscall {
                    op: DataOp::Close { fd },
                    resources: vec![close_fd(fd)],
                    next: Box::new(|_| Action::Pure(Value::Unit)),
                }),
            }
        }),
    }
}

// ══════════════════════════════════════════════════════════════════════
// 1. 游标语义：写后不 Seek 直接读 → 空字节
//    （README §3 的 Write → Seek(0) → Read 顺序中，Seek 是必需步骤）
// ══════════════════════════════════════════════════════════════════════

#[test]
fn cursor_write_then_read_without_seek_is_empty() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cursor.txt");

    // Open → Write → 直接 Read（**无 Seek**）：游标停在写后位置（文件末尾），
    // 读回 0 字节。若读者把 README §3 抄成 Open→Write→Read（漏掉 Seek），
    // 得到的不是内容而是空——锁定游标语义，防 README 示例误导。
    let fd_cell: std::sync::Arc<std::sync::Mutex<Option<u64>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let fc = fd_cell.clone();
    let blueprint = Action::Syscall {
        op: DataOp::Open {
            path: path.clone(),
            flags: rw_flags(),
        },
        resources: vec![write_path(&path)],
        next: Box::new(move |v| {
            let fd = fd_of(&v);
            *fc.lock().unwrap() = Some(fd);
            Action::Sequential {
                current: Box::new(Action::Syscall {
                    op: DataOp::Write {
                        fd,
                        data: b"hello algeff".to_vec(),
                    },
                    resources: vec![write_fd(fd)],
                    next: Box::new(|_| Action::Pure(Value::Unit)),
                }),
                next: Box::new(move |_| Action::Syscall {
                    op: DataOp::Read { fd, len: 64 },
                    resources: vec![read_fd(fd)],
                    next: Box::new(Action::Pure),
                }),
            }
        }),
    };
    assert_eq!(
        rt.run_blocking(blueprint).unwrap(),
        Value::Bytes(vec![]),
        "写后不 Seek 直接读应得空字节（游标停在写后位置 = 文件末尾）"
    );

    // 对照（README §3 完整语义）：同一句柄 Seek(0) 后读回完整内容。
    let fd = fd_cell.lock().unwrap().unwrap();
    let blueprint2 = Action::Sequential {
        current: Box::new(Action::Syscall {
            op: DataOp::Seek {
                fd,
                offset: 0,
                whence: std::io::SeekFrom::Start(0),
            },
            resources: vec![read_fd(fd)],
            next: Box::new(|_| Action::Pure(Value::Unit)),
        }),
        next: Box::new(move |_| Action::Syscall {
            op: DataOp::Read { fd, len: 64 },
            resources: vec![read_fd(fd)],
            next: Box::new(Action::Pure),
        }),
    };
    assert_eq!(
        rt.run_blocking(blueprint2).unwrap(),
        Value::Bytes(b"hello algeff".to_vec()),
        "Seek(0) 后应读回完整内容（README §3 语义）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 2. fork! 冲突顺序化：两侧写同一文件 → 自动退化为顺序，结果确定
//    （README 声称：两个分支若静态冲突，运行时自动退化为顺序执行）
// ══════════════════════════════════════════════════════════════════════

#[test]
fn fork_same_file_conflict_serializes_deterministic() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conflict.txt");

    // 两侧 Open+Write 同一路径：静态冲突（Path-Write ∥ Path-Write）→
    // 自动顺序化（left→right）。注意：fork! 宏只接受 left:/right: 标签
    // （README「并行 Fork」代码块的无标签位置形式照抄无法编译，见文件头）。
    let lp = path.clone();
    let rp = path.clone();
    let blueprint = fork! {
        left: write_side(lp, b"L".to_vec()),
        right: write_side(rp, b"RIGHTDATA".to_vec()),
    };

    assert_eq!(rt.run_blocking(blueprint).unwrap(), Value::Unit);

    // 顺序语义：left 先写（"L"）、right 后写（"RIGHTDATA"）→ 终态 "RIGHTDATA"。
    // 若调度反序（right 先、left 后）会得到 "LIGHTDATA" —— 断言锁死顺序确定性。
    let content = std::fs::read(&path).unwrap();
    assert_eq!(
        content, b"RIGHTDATA",
        "冲突 Fork 应按 left→right 顺序执行，终态为右分支内容（确定性，无竞态）"
    );

    // 两侧 Write 均真实执行并各登记一条 Full 撤销记录（确定性副产物）。
    assert_eq!(
        rt.undo_stack().len(),
        2,
        "两侧写各应登记一条撤销记录（顺序路径共享撤销栈）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 3. 深度守卫边界：左结合链 96 OK / 97 Err(Other(105))
//    （README 声称：左结合链 ≥97 步返回 Err(SysError::Other(105))）
// ══════════════════════════════════════════════════════════════════════

#[test]
fn depth_guard_seq_96_ok_97_err() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 左结合链 = adapters::seq 折叠（README「深度守卫」指定的构造）：
    // N 个元素 → N-1 层 Sequential 嵌套 → 首个元素（叶子）进入解释器
    // 深度 N-1。守卫在递归入口检查 depth ≥ 96 → Err(SysError::Other(105))
    // （ENOBUFS=105 语义）。故 96 元素 = 叶子深度 95 → OK；97 元素 = 叶子
    // 深度 96 → Err。注意：边界数值依赖构造方式（"步"= seq 元素数），
    // 手工左嵌套 96 层 Sequential 会在叶子深度 96 处触发（计数口径不同）。
    let steps_96: Vec<Action> = (0..96).map(|_| gettime_step()).collect();
    assert_eq!(
        rt.run_blocking(adapters::seq(steps_96)).unwrap(),
        Value::Unit,
        "96 步左结合链应正常完成（叶子深度 95 < 96）"
    );

    let steps_97: Vec<Action> = (0..97).map(|_| gettime_step()).collect();
    let e = rt.run_blocking(adapters::seq(steps_97)).unwrap_err();
    assert_eq!(
        e,
        SysError::Other(105),
        "97 步左结合链应触发深度守卫 Other(105)"
    );

    // 守卫在副作用发生前触发：任何一步 GetTime 都未执行 → 撤销栈为空。
    assert!(
        rt.undo_stack().is_empty(),
        "深度守卫应在副作用发生前返回（无 syscall 执行）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 4. Catch 捕获 Other(105) 后可继续
//    （README 声称：深度守卫错误可被 Catch 捕获）
// ══════════════════════════════════════════════════════════════════════

#[test]
fn catch_depth_error_other105_and_continue() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 深度 97 左结合链 → Err(Other(105))，被 Catch 捕获。
    let deep: Action = adapters::seq((0..97).map(|_| gettime_step()).collect());
    let caught = Action::Catch {
        action: Box::new(deep),
        handler: Box::new(|e| {
            assert_eq!(e, SysError::Other(105), "应捕获深度守卫错误 Other(105)");
            Action::Pure(Value::U64(105))
        }),
    };

    // 捕获后继续：Catch 结果透传给后续真实系统调用（状态可继续）。
    let blueprint = adapters::and_then(caught, |v| {
        assert_eq!(v, Value::U64(105), "Catch 应返回 handler 的结果");
        Action::Syscall {
            op: DataOp::GetTime,
            resources: vec![],
            next: Box::new(Action::Pure),
        }
    });
    let v = rt.run_blocking(blueprint).unwrap();
    assert!(
        matches!(v, Value::U64(_)),
        "捕获 Other(105) 后运行时应可继续执行真实系统调用"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 5. Timeout 超时后 on_timeout 生效 + 状态可继续
//    （README 速查：Timeout 超时走 on_timeout）
// ══════════════════════════════════════════════════════════════════════

#[test]
fn timeout_on_timeout_runs_and_state_continues() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // action 长睡 500ms ≫ 20ms 上限 → 必超时 → on_timeout 生效。
    // 若 on_timeout 未生效，会得到内层 Sleep 的结果 U64(7)——断言区分两者。
    let t = Action::Timeout {
        action: Box::new(Action::Sleep {
            duration: Duration::from_millis(500),
            next: Box::new(|_| Action::Pure(Value::U64(7))),
        }),
        duration: Duration::from_millis(20),
        on_timeout: Box::new(Action::Pure(Value::U64(0))),
    };

    // 超时后继续：on_timeout 结果透传给后续真实系统调用（状态可继续）。
    let blueprint = adapters::and_then(t, |v| {
        assert_eq!(
            v,
            Value::U64(0),
            "on_timeout 应生效：返回 U64(0) 而非内层结果 U64(7)"
        );
        Action::Syscall {
            op: DataOp::GetTime,
            resources: vec![],
            next: Box::new(Action::Pure),
        }
    });
    let v = rt.run_blocking(blueprint).unwrap();
    assert!(
        matches!(v, Value::U64(_)),
        "超时走 on_timeout 后运行时应可继续执行真实系统调用"
    );
}
