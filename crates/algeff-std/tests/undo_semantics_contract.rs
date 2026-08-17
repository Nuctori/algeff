//! 修复后语义锁定（与 gap 文档 spec/semantic-undo-gaps.md 对应）：
//!
//! 1. **write-only 句柄写 → 写前读失败 → Err**：无法构造逆 → 报错（不再静默降级）。
//! 2. **undo 闭包失败上报**：mkdir 逆（remove_dir 非空失败）→ recover 返回 Err。
//! 3. **A4 use/move 拆分（已反转）**：Write 是 use 语义可多次（独立 undo +
//!    LIFO 撤销）；Own 是 move 语义终结一次。二写允许且撤销正确。
//! 4. **open+create 不可逆显式化**：Open(create) 标 NonInvertible → Replace 闸门拒绝
//!    （不再静默残留）。

use std::path::PathBuf;

use algeff_core::{
    Action, DataOp, OpenFlags, ResourceInner, ResourceUsage, Runtime, SysError, TypedResource,
    Value, WriteOnly,
};
use algeff_macro::do_;
use algeff_std::dx;
use algeff_std::TokioExecutor;

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1-R4 相同约定）──────────────

fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn wr_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(path)).into_usage()
}

fn fd_of(v: &Value) -> u64 {
    match v {
        Value::Fd(f) => *f,
        other => panic!("期望 Fd，得到 {other:?}"),
    }
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

// ══════════════════════════════════════════════════════════════════════
// 修复 1：write-only 句柄 → 写前读失败 → 无法构造逆 → Err（不再静默降级）
// ══════════════════════════════════════════════════════════════════════

#[test]
fn write_only_fd_write_rejected_when_undo_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("wo.txt");
    std::fs::write(&p, b"original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // write-only 打开（无 read）→ op_write 写前读失败（Windows ACCESS_DENIED
    // / Unix EBADF）→ 无法构造逆 → Err（语义真回归：不静默降级）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: OpenFlags {
                    write: true,
                    create: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);

    let e = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd,
                data: b"CHANGED".to_vec(),
            },
            vec![wr(fd)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::PermissionDenied,
        "写前读失败（无法构造撤销）→ 必须报错，而非带副作用无声成功"
    );
    assert_eq!(
        std::fs::read(&p).unwrap(),
        b"original".to_vec(),
        "写未生效（Err 前无副作用）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 修复 2：mkdir 逆（remove_dir）+ create 逆（unlink）组合 → 完全回归
// ══════════════════════════════════════════════════════════════════════

#[test]
fn mkdir_inverse_removes_dir_when_emptied_by_create_undo() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("sub");
    let f = d.join("file.txt");

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let f1 = f.clone();
    rt.run_blocking(do_! {
        dx::mkdir(&d, 0o755);
        let fd = dx::open(
            &f1,
            OpenFlags {
                read: true,
                write: true,
                create: true,
                ..Default::default()
            },
        );
        dx::write(&fd, b"data".to_vec());
        dx::close(&fd);
        Value::Unit
    })
    .unwrap();
    assert!(d.exists(), "前置：目录已创建");

    // Replace = recover（LIFO）：write 逆 → create 逆（unlink 删除文件，目录变空）
    // → mkdir 逆（remove_dir 成功）→ 完全回归：目录与文件都被删除。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(
        !d.exists() && !f.exists(),
        "create 逆删除文件 → 目录空 → mkdir 逆成功 → 完全回归（真回归）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 偏差 3：A4 过度拒绝——顺序多次 Write 同 fd（运行时本可确保每次独立撤销）
// ══════════════════════════════════════════════════════════════════════

#[test]
fn sequential_multi_write_same_fd_allowed_use_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("multi.txt");
    std::fs::write(&p, b"original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    create: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);

    // 第一次写：成功，Full 撤销入栈。
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"first".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(rt.undo_stack().len(), 1, "首次写有 undo");

    // 第二次写：允许（A4 use/move 拆分，D-0xx：Write 是 use 语义可多次）
    // → 独立 undo 入栈（写前读第二次覆盖区域）。
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"second".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(rt.undo_stack().len(), 2, "两次写各一个独立 undo");

    // LIFO 撤销正确：先还原第二次写（写回 first 覆盖区），再还原第一次
    // （写回 original 覆盖区）→ 回到 open 前状态（真回归）。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(
        std::fs::read(&p).unwrap(),
        b"original".to_vec(),
        "两次写都被独立撤销（LIFO，语义真回归）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// P2：静态代数角色分类（DataOp::role）+ 显式不可逆声明（dx::irreversible）
// ══════════════════════════════════════════════════════════════════════

#[test]
fn dataop_static_role_direct() {
    use algeff_core::{DataOp, OpenFlags, UndoRole};
    use std::path::PathBuf;

    let p = PathBuf::from("/x");
    assert_eq!(
        DataOp::Stat { path: p.clone() }.role(),
        UndoRole::Identity,
        "Stat 无副作用 → Identity"
    );
    assert_eq!(
        DataOp::Write { fd: 1, data: b"d".to_vec() }.role(),
        UndoRole::Invertible,
        "Write 可逆 → Invertible（静态，运行时写前读决定）"
    );
    assert_eq!(
        DataOp::Unlink { path: p.clone() }.role(),
        UndoRole::NonInvertible,
        "Unlink 删除不可逆 → NonInvertible"
    );
    assert_eq!(
        DataOp::Open { path: p, flags: OpenFlags::default() }.role(),
        UndoRole::Invertible,
        "Open 静态可逆（运行时按 flags/existed 细分）"
    );
    assert!(!DataOp::GetTime.is_deterministic(), "GetTime 不确定（P3）");
    assert!(
        DataOp::Stat { path: PathBuf::from("/y") }.is_deterministic(),
        "Stat 确定（P3）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 修复 4：open+create 逆 = unlink（P1）→ Replace 完全回归
#[test]
fn create_open_inverse_removes_new_file_on_replace() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("created.txt");
    assert!(!f.exists());

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let p = f.clone();
    rt.run_blocking(do_! {
        let fd = dx::open(
            &p,
            OpenFlags {
                read: true,
                write: true,
                create: true,
                ..Default::default()
            },
        );
        dx::write(&fd, b"data".to_vec());
        dx::close(&fd);
        Value::Unit
    })
    .unwrap();
    assert!(f.exists(), "前置：文件已创建");

    // Open(create) 逆 = unlink（文件原不存在时，P1 已补）→ Replace recover
    // 执行 write 逆 + unlink 逆 → 新建文件被删除（真回归：回到 open 前状态）。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(
        !f.exists(),
        "Replace 后新建文件被删除（create 逆生效，真回归）"
    );
}
