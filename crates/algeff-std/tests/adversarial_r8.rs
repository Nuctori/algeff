//! R8 对抗第一波（迭代 3-A7）——迭代 2/3 新攻击面的对抗审计：
//!
//! 审计焦点（4 面）：
//! 1. **DX 层**：do_! 宏展开的行为契约（值流/错误流/捕获语义）；`dx::catch`
//!    组合（嵌套 catch、catch 内 do_!、值贯穿）；`infer_usage` 全表与 A4
//!    语义一致性（TcpShutdown 空集后 shutdown→close→read 链、SendSignal 二次、
//!    MutexLock 重入）；`syscall_with` 显式覆盖与自动推导混合。
//! 2. **取消传播 + R7-A/B 修复面**：ForkJoinMerge「完成即合并」的确定性
//!    （右分支先完成走 r_stash 暂存、左分支后完成按序补合并）；宽限耗尽 vs
//!    join 两路径；取消后资源状态（fd 活性/锁占坑/undo 栈）组合行为。
//! 3. **阈值 64**：边界组合——Timeout/Catch 包装帧计入深度计数；Fork 分支
//!    内深度**独立从 0 起算**（与顺序路径的深度累计形成对照）。
//! 4. **性能二轮共享 reactor**：Shared{executor, reactor} 下分支 IO 的语义
//!    保持（分支打开的 fd 在 join 后仍可寻址/可 IO；嵌套 Fork 两层级联合并；
//!    reactor 句柄跨分支边界存活）。
//!
//! 审计方法：基线 7f7285a + main 合并后代码走读（dx.rs / executor.rs /
//! runtime.rs run_wall_timeout/run_fork_parallel/ForkJoinMerge）+ 行为探测 →
//! 断言锁定**当前行为**（src/ 冻结零改动，只写测试）。真实缺陷记录于
//! findings（不修）。
//!
//! 时域门控：取消/宽限测试依赖真实墙钟计时（宽限 500ms、超时 30ms），
//! `virtual-clock` 构建下 Timeout 走 `run_virtual_timeout` post-check 语义
//! （效果保留、无取消）——与既有套件（adversarial_r7.rs / r7ab.rs）同约定，
//! 本文件同样 `#[cfg(not(feature = "virtual-clock"))]` 门控相关测试。

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use algeff_core::prelude::*;
use algeff_core::{AccessMode, OpenFlags, SysError};
use algeff_macro::do_;
use algeff_std::dx;
use algeff_std::TokioExecutor;

use std::time::Duration;
#[cfg(not(feature = "virtual-clock"))]
use std::time::Instant;

// ── 本地辅助（src/ 冻结不可改，测试内复制；与既有对抗套件同约定）──────────

fn rt() -> Runtime {
    Runtime::new(Box::new(TokioExecutor::new()))
}

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
    ResourceUsage {
        resource: Resource::Fd(fd),
        mode: AccessMode::Read,
    }
}
fn wr(fd: u64) -> ResourceUsage {
    ResourceUsage {
        resource: Resource::Fd(fd),
        mode: AccessMode::Write,
    }
}
fn ow(fd: u64) -> ResourceUsage {
    ResourceUsage {
        resource: Resource::Fd(fd),
        mode: AccessMode::Own,
    }
}
fn wr_path(path: std::path::PathBuf) -> ResourceUsage {
    ResourceUsage {
        resource: Resource::Path(path.to_string_lossy().into_owned()),
        mode: AccessMode::Write,
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

fn rw_flags() -> OpenFlags {
    OpenFlags {
        read: true,
        write: true,
        create: true,
        ..Default::default()
    }
}

fn read_only_flags() -> OpenFlags {
    OpenFlags {
        read: true,
        ..Default::default()
    }
}

#[cfg(not(feature = "virtual-clock"))]
fn mutex_lock(id: u64) -> Action {
    syscall(DataOp::MutexLock { id }, vec![], Action::Pure)
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
// §1 DX 层：do_! 宏展开行为 + dx::catch 组合 + infer_usage/A4 一致性
// ══════════════════════════════════════════════════════════════════════

/// catch handler 返回**嵌套 do_! 块**：handler 内 mkdir→open→write→seek 全链
/// 真实执行，尾表达式（fd 值）从 handler 的 do_! **贯穿**到外层链的 `let`
/// 绑定；外层链继续用该 fd 写读——「catch 内 do_! + 值贯穿 + 错误流吸收」
/// 三合一（迭代 2/3 DX 新攻击面：handler 必须可任意组合 Action，宏只做
/// AST 拼接、不引入新节点）。
#[test]
fn dx_catch_handler_do_block_success_value_flows() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no_such.txt");
    let sub = dir.path().join("sub");
    let fb = sub.join("fallback.txt");
    let fbc = fb.clone();

    let blueprint = do_! {
        let fd = dx::catch(
            dx::open(&missing, read_only_flags()),
            move |_e| do_! {
                dx::mkdir(&sub, 0o755);
                let f = dx::open(fbc.clone(), rw_flags());
                dx::seek(&f, 0, std::io::SeekFrom::Start(0));
                f // 尾表达式 = fd 值贯穿出 handler 的 do_!
            },
        );
        dx::write(&fd, b"outer".to_vec());
        dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
        let data = dx::read(&fd, 64);
        dx::close(&fd);
        data
    };

    let v = rt().run_blocking(blueprint).unwrap();
    assert_eq!(
        v,
        Value::Bytes(b"outer".to_vec()),
        "handler do_! 的 fd 贯穿到外层链，外层写生效"
    );
    assert_eq!(
        std::fs::read(&fb).unwrap(),
        b"outer",
        "同一 fd 句柄：外层 write 真实落盘（handler 只做 mkdir/open/seek）"
    );
}

/// catch handler 内的 do_! **自身失败**（open 不存在）：错误沿链上抛——
/// 同一 catch 不二次捕获 handler 的错误（Catch 语义：仅处理 action 的失败，
/// handler 的替代 Action 由 Catch 臂执行，其错误继续传播）。副作用（mkdir）
/// 保留。
#[test]
fn dx_catch_handler_do_block_failure_propagates() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no_such.txt");
    let missing2 = dir.path().join("also_missing.txt");
    let sub = dir.path().join("sub");

    let sub_check = sub.clone();
    let blueprint = do_! {
        let fd = dx::catch(
            dx::open(&missing, read_only_flags()),
            move |_e| do_! {
                dx::mkdir(&sub, 0o755);
                let f = dx::open(missing2.clone(), read_only_flags());
                dx::write(&f, b"x".to_vec());
                f
            },
        );
        dx::write(&fd, b"x".to_vec());
        Value::Unit
    };

    let e = rt().run_blocking(blueprint).unwrap_err();
    assert!(
        matches!(e, SysError::NotFound),
        "handler 内 do_! 失败应上抛（同一 catch 不二次捕获），实测 {e:?}"
    );
    assert!(
        sub_check.exists(),
        "handler do_! 的 mkdir 副作用已执行（错误后保留）"
    );
}

/// **嵌套 catch**：内层 catch 的 handler（do_! 块）失败 → 错误穿透内层
/// Catch 臂 → 被外层 catch 捕获并恢复（fallback 文件）；fd 值贯穿两层
/// 到外层链继续写读。验证 Catch 组合的「错误只沿最近一层 action 捕获、
/// handler 错误向上传播」契约。
#[test]
fn dx_nested_catch_inner_handler_failure_outer_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let m1 = dir.path().join("m1.txt");
    let m2 = dir.path().join("m2.txt");
    let fb = dir.path().join("fallback.txt");
    let fbc = fb.clone();

    let blueprint = do_! {
        let fd = dx::catch(
            dx::catch(
                dx::open(&m1, read_only_flags()),
                move |_e| do_! {
                    let f = dx::open(m2.clone(), read_only_flags());
                    f
                },
            ),
            move |_e2| dx::open(fbc.clone(), rw_flags()),
        );
        dx::write(&fd, b"nested".to_vec());
        dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
        let data = dx::read(&fd, 64);
        dx::close(&fd);
        data
    };

    let v = rt().run_blocking(blueprint).unwrap();
    assert_eq!(
        v,
        Value::Bytes(b"nested".to_vec()),
        "外层 catch 恢复后 fd 贯穿并正常写读"
    );
    assert_eq!(std::fs::read(&fb).unwrap(), b"nested", "fallback 真实落盘");
}

/// R4-F1 + R7 修复的**全链真实执行**回归（半关闭数据流，全平台无阻塞）：
/// TcpShutdown 自动推导为空集——(a) 修复前 Write 声明会与 tcp_write 的
/// Write 消费冲突（A4 至多一次）→ 误拒 write→shutdown 合法链；(b) 修复前
/// Own 声明是终结语义 → 拒绝后续 close/read。本测试用 dx 全套包装（自动推
/// 导）跑真实 TCP 半关闭链：accept → tcp_write"PING" → tcp_shutdown(Write)
/// → tcp_read"pong"（读半端存活）→ close × 2，A4 零拦截、物理层全部成功、
/// shutdown 轮换后 read 仍可寻址。客户端在 EOF 后上行 "pong"。
#[test]
fn dx_tcp_shutdown_inferred_empty_write_shutdown_read_close() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let lfd = rt
        .run_blocking(dx::open_tcp("127.0.0.1:0".parse().unwrap()))
        .unwrap();
    let lfd = fd_of(&lfd);
    let addr: std::net::SocketAddr = match rt.registry().lookup(lfd).unwrap() {
        ResourceHandle::TcpListener(l) => l.local_addr().unwrap(),
        other => panic!("期望 TcpListener 句柄，得到 {other:?}"),
    };

    // 半关闭客户端：connect → 读 4 字节（期望 "PING"）→ 再读（期望 EOF）
    // → 上行 "pong"。10s 超时防悬挂。
    let client = std::thread::spawn(move || {
        let crt = tokio::runtime::Runtime::new().unwrap();
        crt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
                let mut ping = [0u8; 4];
                let mut got = 0usize;
                while got < 4 {
                    let n = s.read(&mut ping[got..]).await.unwrap();
                    assert!(n > 0, "客户端读到 EOF（未收到 PING）");
                    got += n;
                }
                let mut eof_buf = [0u8; 1];
                // 平台差异：读错误（如 RST）同样视为观察到终止。
                let eof = match s.read(&mut eof_buf).await {
                    Ok(0) => true,
                    Ok(_) => false,
                    Err(_) => true,
                };
                s.write_all(b"pong").await.unwrap();
                (ping.to_vec(), eof)
            })
            .await
            .expect("客户端 10s 超时")
        })
    });

    // 服务端 do_! 链（全部自动推导）：accept → tcp_write "PING" →
    // tcp_shutdown(Write)（空集）→ tcp_read "pong"（读半端存活）→ close × 2。
    // conn 为 accept 的 List([Fd, Addr])，逐语句内联提取 fd（do_! 语句槽只
    // 接受 Action 表达式，普通值提取只能内联）。
    let blueprint = do_! {
        let conn = dx::accept(&Value::Fd(lfd));
        dx::tcp_write(
            &Value::Fd(dx::expect_fd(&dx::expect_list(&conn)[0])),
            b"PING".to_vec(),
        );
        dx::tcp_shutdown(
            &Value::Fd(dx::expect_fd(&dx::expect_list(&conn)[0])),
            std::net::Shutdown::Write,
        );
        let pong = dx::tcp_read(&Value::Fd(dx::expect_fd(&dx::expect_list(&conn)[0])), 4);
        dx::close(&Value::Fd(dx::expect_fd(&dx::expect_list(&conn)[0])));
        dx::close(&Value::Fd(lfd));
        Value::List(vec![pong])
    };

    let v = rt.run_blocking(blueprint).unwrap();
    let pong = match v {
        Value::List(items) => match &items[0] {
            Value::Bytes(b) => b.clone(),
            other => panic!("期望 Bytes，得到 {other:?}"),
        },
        other => panic!("期望 List([Bytes])，得到 {other:?}"),
    };
    assert_eq!(
        pong, b"pong",
        "shutdown(Write) 后读半端仍存活（客户端 EOF 后上行 pong）"
    );

    let (ping, eof) = client.join().unwrap();
    assert_eq!(ping, b"PING", "客户端收到 shutdown(Write) 前写入的数据");
    assert!(eof, "客户端观察到 EOF（FIN 已发送）");
    assert!(
        rt.undo_stack().is_empty(),
        "TCP 链无 undo 残留（write 走流路径不产生 undo）"
    );
}

/// R7 DX 修复回归的**单蓝图**变体：spawn → send_signal(9) → send_signal(9)
/// → wait 一条 do_! 链内完成。SendSignal 自动推导空集 → A4 线性域不消费
/// Signal/Pid，二次发送**绝不**被 A4 拒绝（物理层成败由平台决定——已杀
/// 子进程的 start_kill 幂等或报错，用 dx::catch 吸收平台差异，链继续）。
/// 修复前：二次发送在 A4 层 InvalidInput 中断整链。
#[test]
fn dx_send_signal_twice_in_one_blueprint_with_catch() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    #[cfg(windows)]
    let cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "timeout", "60"]);
        c
    };
    #[cfg(not(windows))]
    let cmd = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", "sleep 60"]);
        c
    };

    let blueprint = do_! {
        let pid = dx::spawn(cmd);
        dx::send_signal(9, &pid);
        // 二次发送：catch 吸收物理层平台差异（幂等 Ok 或已终止报错），
        // 但绝不可能是 A4 的 InvalidInput（空集推导，R7）。
        dx::catch(dx::send_signal(9, &pid), |_e| dx::unit());
        let code = dx::wait(&pid);
        code
    };

    let v = rt.run_blocking(blueprint).unwrap();
    assert!(
        matches!(v, Value::U64(_)),
        "单蓝图双 SIGKILL + wait 全链成功（A4 不拦截二次发送）"
    );
}

/// `syscall_with` 显式覆盖与自动推导**混合**于同一 do_! 链：链首 Open 显式
/// 覆盖为 Read(path)（免写消费标记），其后 Write/Seek/Read/Close 全自动推导
/// ——两套资源声明机制在同一链内共存；结构断言链首节点资源 = 显式覆盖值。
#[test]
fn dx_syscall_with_override_mixed_with_inference() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("mix.txt");
    let pc = p.clone();

    let blueprint = do_! {
        let fd = dx::syscall_with(
            DataOp::Open {
                path: pc.clone(),
                flags: rw_flags(),
            },
            vec![dx::path_usage(&pc, AccessMode::Read)],
        );
        dx::write(&fd, b"mix".to_vec());
        dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
        let data = dx::read(&fd, 64);
        dx::close(&fd);
        data
    };

    // 结构断言（先借用于执行前）：链首节点（syscall_with）资源 =
    // 显式 Read(path) 覆盖。
    let Action::Sequential { current, .. } = &blueprint else {
        panic!("do_! 应展开为 Sequential 链");
    };
    let Action::Syscall { op, resources, .. } = &**current else {
        panic!("链首应为 Syscall(Open)");
    };
    assert!(matches!(op, DataOp::Open { .. }));
    assert_eq!(
        resources,
        &vec![dx::path_usage(&p, AccessMode::Read)],
        "显式覆盖应完全替换自动推导（Open(write) 本应推 Write(path)）"
    );

    let v = rt().run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::Bytes(b"mix".to_vec()), "混合声明链真实执行");
}

// ══════════════════════════════════════════════════════════════════════
// §2 取消传播 + R7-A/B 修复面：ForkJoinMerge 确定性 + 宽限两路径 + 组合状态
// ══════════════════════════════════════════════════════════════════════

/// ForkJoinMerge「完成即合并」的 **r_stash 暂存路径**确定性：右分支（纯锁
/// 操作）先完成 → 暂存；左分支（先睡 80ms 再持锁）后完成 → 合并 left，
/// 再按序补合并暂存的 right。黑盒可观察面：
/// - undo 栈长度 = 2（两分支 undo 均已并入父，含暂存路径）；
/// - 两把锁的 arbiter 占坑均对父可见 → 同 id 重入 WouldBlock（合并完整性：
///   若任一分支状态未合并，其锁立即可重入）；
/// - Replace（recover + reg.clear）执行合并后的 undo（LIFO：right 先、left
///   后，与「left 先执行」的观察序一致）→ 两锁释放 → 均立即可重入。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn fork_join_stash_right_first_merge_complete_locks_held_until_recover() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 左分支：Sleep(80ms) 保证右分支必然先完成（走 r_stash 暂存）。
    let left = Action::Sequential {
        current: Box::new(Action::Sleep {
            duration: Duration::from_millis(80),
            next: Box::new(|_| mutex_lock(1)),
        }),
        next: Box::new(|_| Action::Pure(Value::Unit)),
    };
    let right = mutex_lock(2);
    let blueprint = Action::Fork {
        left: Box::new(left),
        right: Box::new(right),
        combine: Box::new(|_, _| Action::Pure(Value::Unit)),
    };

    let v = rt.run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::Unit);
    assert_eq!(
        rt.undo_stack().len(),
        2,
        "两分支 undo 均已合并回父（含暂存路径的 right 分支）"
    );

    // 合并完整性：两把锁的占坑都可见于父 → 同 id 重入 WouldBlock。
    let e1 = rt.run_blocking(mutex_lock(1)).unwrap_err();
    assert_eq!(e1, SysError::WouldBlock, "左分支锁仍持有");
    let e2 = rt.run_blocking(mutex_lock(2)).unwrap_err();
    assert_eq!(e2, SysError::WouldBlock, "右分支（暂存合并）锁仍持有");

    // Replace：recover 执行合并后的 undo → 两锁释放 → 可重入。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(
        rt.run_blocking(mutex_lock(1)).unwrap(),
        Value::Unit,
        "recover 后锁 1 可重入"
    );
    assert_eq!(
        rt.run_blocking(mutex_lock(2)).unwrap(),
        Value::Unit,
        "recover 后锁 2 可重入"
    );
}

/// **宽限耗尽路径**的资源状态组合（R7-A + R7-B 修复面）：Timeout{Fork{
/// left: Open→Write 文件（快速完成即合并）, right: UdpRecvFrom（阻塞、不可
/// 取消）}}，30ms 超时 → 取消广播 → 宽限（500ms）耗尽 → inner 丢弃。已完成
/// 左分支的状态（registry 句柄 + undo + 线性标记）已被 ForkJoinMerge 合并回
/// 父，随后由 Timeout 臂统一回滚。黑盒断言三态组合：
/// - **fd 活性**：分支打开的 fd 合并后仍在父 registry，取消后同 fd 可再写；
/// - **undo 栈**：合并的 Write undo 被 rollback_from 执行 → 内容恢复原值、
///   栈空；
/// - **线性标记**：Write(fd) 消费标记被 rollback_linear_to 回滚 → 同 fd 再
///   声明 Write 通过 A4（可重试），物理写成功。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn timeout_grace_exhausted_left_branch_file_fd_undo_linear_combo() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("c2.txt");
    std::fs::write(&pa, b"abcdef").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let udp = udp_bind(&mut rt); // 父级 UDP 绑定：右分支阻塞于不可取消 IO

    // 左分支：Open→Write→把 fd 存入共享单元（闭包副作用，join 后取用）。
    let fd_cell: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let cell = fd_cell.clone();
    let left = syscall(
        DataOp::Open {
            path: pa.clone(),
            flags: rw_flags(),
        },
        vec![wr_path(pa.clone())],
        move |v| {
            let fd = fd_of(&v);
            let c = cell.clone();
            syscall(
                DataOp::Write {
                    fd,
                    data: b"123".to_vec(),
                },
                vec![wr(fd)],
                move |_| {
                    *c.lock().unwrap() = Some(fd);
                    Action::Pure(Value::Fd(fd))
                },
            )
        },
    );
    let right = syscall(
        DataOp::UdpRecvFrom { fd: udp, len: 8 },
        vec![rd(udp)],
        Action::Pure,
    );
    let inner = Action::Fork {
        left: Box::new(left),
        right: Box::new(right),
        combine: Box::new(|_, _| Action::Pure(Value::Unit)),
    };

    let t0 = Instant::now();
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(inner),
            duration: Duration::from_millis(30),
            on_timeout: Box::new(Action::Pure(Value::U64(42))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(42), "on_timeout 生效");
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_millis(450) && elapsed < Duration::from_millis(1400),
        "宽限耗尽：须耗尽 500ms 宽限（elapsed={elapsed:?}）"
    );

    // undo 回滚 + 内容恢复。
    assert!(
        rt.undo_stack().is_empty(),
        "Timeout 回滚已弹出合并的 Write undo"
    );
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"abcdef",
        "分支合并的 Write undo 被回滚 → 内容恢复写前原值"
    );

    // fd 活性 + 线性标记回滚：同 fd 再写通过 A4 且物理成功（游标经 undo
    // seek 回写前位置 0 → 覆盖前两字节）。
    let fd = fd_cell.lock().unwrap().unwrap();
    assert!(
        rt.registry().lookup(fd).is_some(),
        "分支 fd 已合并回父 registry（取消后仍存活）"
    );
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"XY".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    // 写后游标在 2：先 seek 0 再整读（验证 undo 已把游标复位到写前位置）。
    rt.run_blocking(syscall(
        DataOp::Seek {
            fd,
            offset: 0,
            whence: std::io::SeekFrom::Start(0),
        },
        vec![rd(fd)],
        Action::Pure,
    ))
    .unwrap();
    let rb = rt
        .run_blocking(syscall(
            DataOp::Read { fd, len: 6 },
            vec![rd(fd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(
        rb,
        Value::Bytes(b"XYcdef".to_vec()),
        "取消后同 fd 再写成功（线性标记已回滚，Write(fd) 可重消费）"
    );
    rt.run_blocking(syscall(DataOp::Close { fd }, vec![ow(fd)], Action::Pure))
        .unwrap();
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty(), "清理后撤销栈干净");
}

/// **宽限内 join 路径**的组合状态（R7-A + R7-B 同框）：Timeout{Fork{left:
/// MutexLock(5)→PipeOpen→Write"pre", right: Sleep(400ms)}}，30ms 超时 →
/// 取消广播 → Sleep 分支经 cancellable_sleep 立即醒来（join 路径，非宽限
/// 耗尽）→ 两分支在宽限内完成 join → 合并的 undo/线性标记由 Timeout 统一
/// 回滚。黑盒断言：
/// - 锁占坑回滚 → 同 id 立即可重入（RFC-09 在 Fork 并行路径成立）；
/// - 管道句柄活性 + 数据保留（分支写入的 "pre" 仍在管道中）+
///   线性标记回滚（Write(wfd) 可重消费 → 父级再写成功）。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn timeout_join_path_fork_lock_and_pipe_combined() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let cell: Arc<Mutex<Option<(u64, u64)>>> = Arc::new(Mutex::new(None));

    // 左分支：持锁 → 开管道 → 写 "pre"（管道在分支内创建，避免 RFC-07
    // 父级共享 Arc::get_mut 冲突），fd 对存入共享单元。
    let c1 = cell.clone();
    let left = dx::and_then(mutex_lock(5), move |_| {
        let c2 = c1.clone();
        dx::and_then(
            syscall(
                DataOp::PipeOpen {
                    flags: Default::default(),
                },
                vec![],
                move |v| {
                    let (rfd, wfd) = pair_of(&v);
                    let c3 = c2.clone();
                    syscall(
                        DataOp::Write {
                            fd: wfd,
                            data: b"pre".to_vec(),
                        },
                        vec![wr(wfd)],
                        move |_| {
                            *c3.lock().unwrap() = Some((rfd, wfd));
                            Action::Pure(Value::Unit)
                        },
                    )
                },
            ),
            |_| Action::Pure(Value::Unit),
        )
    });
    let right = Action::Sequential {
        current: Box::new(Action::Sleep {
            duration: Duration::from_millis(400),
            next: Box::new(Action::Pure),
        }),
        next: Box::new(|_| Action::Pure(Value::Unit)),
    };
    let inner = Action::Fork {
        left: Box::new(left),
        right: Box::new(right),
        combine: Box::new(|_, _| Action::Pure(Value::Unit)),
    };

    let t0 = Instant::now();
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(inner),
            duration: Duration::from_millis(30),
            on_timeout: Box::new(Action::Pure(Value::U64(3))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(3), "on_timeout 生效");
    assert!(
        t0.elapsed() < Duration::from_millis(400),
        "宽限内 join：Sleep 分支响应取消快速返回（非宽限耗尽）"
    );

    // 回滚后 undo 栈干净（合并的锁 undo 已被 rollback_from 弹出执行）。
    assert!(rt.undo_stack().is_empty(), "锁 undo 已被回滚弹出，栈干净");

    // R7-B：锁占坑已回滚 → 同 id 立即可重入。
    assert_eq!(
        rt.run_blocking(mutex_lock(5)).unwrap(),
        Value::Unit,
        "join 路径：Timeout 回滚已合并的分支 undo → 锁可立即重入"
    );

    // R7-A 面：管道句柄活性（分支内打开的管道已合并回父）+ 数据保留 +
    // 线性标记回滚（Write(wfd) 可重消费）。
    let Some((rfd, wfd)) = *cell.lock().unwrap() else {
        panic!("分支未写入管道 fd 对");
    };
    let rb = rt
        .run_blocking(syscall(
            DataOp::Read { fd: rfd, len: 3 },
            vec![rd(rfd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(
        rb,
        Value::Bytes(b"pre".to_vec()),
        "取消前分支写入的数据仍在管道中"
    );
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: wfd,
            data: b"post".to_vec(),
        },
        vec![wr(wfd)],
        Action::Pure,
    ))
    .unwrap();
    let rb2 = rt
        .run_blocking(syscall(
            DataOp::Read { fd: rfd, len: 4 },
            vec![rd(rfd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(
        rb2,
        Value::Bytes(b"post".to_vec()),
        "取消后管道写读正常（轮换映射一致）"
    );

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
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty(), "清理后撤销栈干净");
}

// ══════════════════════════════════════════════════════════════════════
// §3 阈值 64：包装帧计入深度 + Fork 分支深度独立起算
// ══════════════════════════════════════════════════════════════════════

/// 深度 depth 的嵌套 Sequential：current 为下一层；叶子返回 U64(300)。
/// 与既有套件（adversarial_r5a.rs / r6c.rs）同构：N 层嵌套 → 叶子进入解释
/// 器深度 N（RFC-11 守卫：depth ≥ 64 → Other(105)）。
fn nested_seq(depth: usize) -> Action {
    if depth == 0 {
        return Action::Pure(Value::U64(300));
    }
    Action::Sequential {
        current: Box::new(nested_seq(depth - 1)),
        next: Box::new(Action::Pure),
    }
}

/// 深度 depth 的嵌套 Sequential，最内层 current 换成 `terminal`（Fork 或
/// 另一段链）：叶子 terminal 进入解释器时深度 = depth。
fn nested_seq_then(depth: usize, terminal: Action) -> Action {
    if depth == 0 {
        return terminal;
    }
    Action::Sequential {
        current: Box::new(nested_seq_then(depth - 1, terminal)),
        next: Box::new(Action::Pure),
    }
}

/// 深度守卫 × Timeout/Catch **包装帧**边界（阈值 64 的组合攻击面）：
/// - Timeout 臂经 run_sub_impl(+1) 进入 inner：Timeout{nested_seq(62)} →
///   叶子深度 63 → 正常完成；Timeout{nested_seq(63)} → 叶子深度 64 →
///   Other(105)，**错误经 Timeout 原样透传**（Timeout 只拦截 Elapsed，
///   不吞其他错误）；
/// - Catch 臂再 +1：Catch{Timeout{nested_seq(61)}} → 63 OK（handler 不
///   执行）；Catch{Timeout{nested_seq(62)}} → 64 Err 且**可被捕获**。
/// 边界数值以实测为准（包装帧计数随解释器结构变化，注释给出推导）。
#[test]
fn depth_boundary_timeout_and_catch_wrappers() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let long = Duration::from_secs(10); // 远大于 inner 执行时间，绝不触发 Elapsed

    // Timeout 包装：叶子 63 OK。
    let ok = rt
        .run_blocking(Action::Timeout {
            action: Box::new(nested_seq(62)),
            duration: long,
            on_timeout: Box::new(Action::Pure(Value::U64(1))),
        })
        .unwrap();
    assert_eq!(ok, Value::U64(300), "Timeout 内叶子深度 63 应正常完成");

    // Timeout 包装：叶子 64 → 守卫错误经 Timeout 透传（非 Elapsed）。
    let e = rt
        .run_blocking(Action::Timeout {
            action: Box::new(nested_seq(63)),
            duration: long,
            on_timeout: Box::new(Action::Pure(Value::U64(1))),
        })
        .unwrap_err();
    assert_eq!(
        e,
        SysError::Other(105),
        "叶子深度 64 触发守卫，错误经 Timeout 原样透传"
    );

    // Catch + Timeout 双层包装：叶子 63 OK（handler 不执行）。
    let caught_ok = rt
        .run_blocking(Action::Catch {
            action: Box::new(Action::Timeout {
                action: Box::new(nested_seq(61)),
                duration: long,
                on_timeout: Box::new(Action::Pure(Value::U64(1))),
            }),
            handler: Box::new(|_| Action::Pure(Value::U64(9))),
        })
        .unwrap();
    assert_eq!(
        caught_ok,
        Value::U64(300),
        "Catch+Timeout 双层包装深度 63：handler 不执行"
    );

    // Catch + Timeout 双层包装：叶子 64 → 守卫错误可被外层 Catch 捕获。
    let caught = rt
        .run_blocking(Action::Catch {
            action: Box::new(Action::Timeout {
                action: Box::new(nested_seq(62)),
                duration: long,
                on_timeout: Box::new(Action::Pure(Value::U64(1))),
            }),
            handler: Box::new(|e| {
                assert_eq!(e, SysError::Other(105), "Catch 应收到深度守卫错误");
                Action::Pure(Value::U64(105))
            }),
        })
        .unwrap();
    assert_eq!(
        caught,
        Value::U64(105),
        "深度守卫错误可被外层 Catch 捕获并继续"
    );
}

/// **Fork 分支内深度独立起算**（run_fork_parallel 分支任务以 depth=0 进入
/// 解释器，tokio worker 线程独立栈预算）：
/// - 父链先沉到深度 40，再进入 Fork，分支内 nested_seq(63)（叶子深度 63）
///   **正常完成**——若深度继承（40+63=103 ≥ 64）必 Err；
/// - 对照（顺序路径）：同深度 40 的父链继续顺序执行 nested_seq(63) →
///   深度累计 103 → 守卫 Other(105)。
#[test]
fn fork_branch_depth_restarts_from_zero() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // Fork 版：父深度 40 → 分支独立 0 起算 → 叶子 63 OK。
    let fork_terminal = Action::Fork {
        left: Box::new(nested_seq(63)),
        right: Box::new(Action::Pure(Value::U64(7))),
        combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
    };
    let v = rt.run_blocking(nested_seq_then(40, fork_terminal)).unwrap();
    match v {
        Value::List(l) => {
            assert_eq!(
                l[0],
                Value::U64(300),
                "左分支叶子 63 正常完成（深度独立起算）"
            );
            assert_eq!(l[1], Value::U64(7), "右分支值贯穿");
        }
        other => panic!("期望 List([U64, U64])，得到 {other:?}"),
    }

    // 顺序对照：无 Fork 边界 → 深度累计 40+63=103 ≥ 64 → 守卫错误。
    let e = rt
        .run_blocking(nested_seq_then(40, nested_seq(63)))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::Other(105),
        "顺序路径深度累计（40+63=103）触发守卫——证明 Fork 分支确有独立深度"
    );
}

// ══════════════════════════════════════════════════════════════════════
// §4 性能二轮共享 reactor：Shared{executor, reactor} 分支 IO 语义保持
// ══════════════════════════════════════════════════════════════════════

/// 共享 reactor 下分支打开的 fd 在 join 后**仍可寻址/可 IO**：Fork 两分支
/// 各自 Open→Write（不同路径，无冲突 → 真并行，分支任务投递到 Runtime
/// 自持 reactor），combine 交出两个 fd；join 后父级用这两个 fd 做
/// Seek/Read/Close（registry 合并 + 共享执行器状态 + reactor 句柄跨分支
/// 边界存活）。同时断言物理文件落盘与值合并。
#[test]
fn shared_reactor_fork_branch_fds_usable_after_join() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("s1-a.txt");
    let pb = dir.path().join("s1-b.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let left = syscall(
        DataOp::Open {
            path: pa.clone(),
            flags: rw_flags(),
        },
        vec![wr_path(pa.clone())],
        move |v| {
            let fd = fd_of(&v);
            syscall(
                DataOp::Write {
                    fd,
                    data: b"L".to_vec(),
                },
                vec![wr(fd)],
                move |_| Action::Pure(Value::Fd(fd)),
            )
        },
    );
    let right = syscall(
        DataOp::Open {
            path: pb.clone(),
            flags: rw_flags(),
        },
        vec![wr_path(pb.clone())],
        move |v| {
            let fd = fd_of(&v);
            syscall(
                DataOp::Write {
                    fd,
                    data: b"R".to_vec(),
                },
                vec![wr(fd)],
                move |_| Action::Pure(Value::Fd(fd)),
            )
        },
    );
    let blueprint = Action::Fork {
        left: Box::new(left),
        right: Box::new(right),
        combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
    };

    let v = rt.run_blocking(blueprint).unwrap();
    let (lfd, rfd) = match &v {
        Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("期望 List([Fd, Fd])，得到 {other:?}"),
    };
    assert_ne!(lfd, rfd, "两分支 fd 区间互斥（F1 全局唯一区间）");

    // 父级继续使用分支 fd（游标在写后位置 1 → 先 seek 0 再读）。
    for (fd, expect) in [(lfd, b"L"), (rfd, b"R")] {
        rt.run_blocking(syscall(
            DataOp::Seek {
                fd,
                offset: 0,
                whence: std::io::SeekFrom::Start(0),
            },
            vec![rd(fd)],
            Action::Pure,
        ))
        .unwrap();
        let b = rt
            .run_blocking(syscall(
                DataOp::Read { fd, len: 1 },
                vec![rd(fd)],
                Action::Pure,
            ))
            .unwrap();
        assert_eq!(b, Value::Bytes(expect.to_vec()), "分支 fd join 后仍可读");
    }
    assert_eq!(std::fs::read(&pa).unwrap(), b"L", "左分支物理落盘");
    assert_eq!(std::fs::read(&pb).unwrap(), b"R", "右分支物理落盘");

    rt.run_blocking(syscall(
        DataOp::Close { fd: lfd },
        vec![ow(lfd)],
        Action::Pure,
    ))
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::Close { fd: rfd },
        vec![ow(rfd)],
        Action::Pure,
    ))
    .unwrap();
    // 分支 Write 产生 2 条 undo（Full 策略 <1MB 写前读）——断言并清理。
    assert_eq!(
        rt.undo_stack().len(),
        2,
        "两分支的 Write undo 均已合并回父（Fork 后合并完整性）"
    );
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty(), "Replace 清理后撤销栈干净");
}

/// 共享 reactor 下**分支内再 spawn（嵌套 Fork）**：外层左分支 = 内层 Fork
/// （两子分支各自 Open→Write），外层右分支 = Open→Write（不同路径，全级
/// 无冲突 → 真并行，任意嵌套深度保持并行）；两层 registry/undo/值逐级
/// 合并回父；join 后三级 fd 全部可寻址（reactor 句柄生命周期跨两层分支
/// 边界）。验证「分支内 spawn 行为」的语义保持。
#[test]
fn shared_reactor_nested_fork_two_level_io_merge() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("n-a.txt");
    let pb = dir.path().join("n-b.txt");
    let pc = dir.path().join("n-c.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let open_write = |path: std::path::PathBuf, data: u8| {
        let d = vec![data];
        syscall(
            DataOp::Open {
                path: path.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(path)],
            move |v| {
                let fd = fd_of(&v);
                syscall(DataOp::Write { fd, data: d }, vec![wr(fd)], move |_| {
                    Action::Pure(Value::Fd(fd))
                })
            },
        )
    };

    // 内层 Fork（在外层左分支内再并行）：a 与 b 互斥区间。
    let inner = Action::Fork {
        left: Box::new(open_write(pa.clone(), b'A')),
        right: Box::new(open_write(pb.clone(), b'B')),
        combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
    };
    // 外层 Fork：左 = 内层 Fork，右 = c。
    let blueprint = Action::Fork {
        left: Box::new(inner),
        right: Box::new(open_write(pc.clone(), b'C')),
        combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
    };

    let v = rt.run_blocking(blueprint).unwrap();
    let (inner_list, fdc) = match &v {
        Value::List(l) => (l[0].clone(), fd_of(&l[1])),
        other => panic!("期望 List([List, Fd])，得到 {other:?}"),
    };
    let (fda, fdb) = match &inner_list {
        Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("期望内层 List([Fd, Fd])，得到 {other:?}"),
    };
    assert_eq!(std::fs::read(&pa).unwrap(), b"A", "内层左分支落盘");
    assert_eq!(std::fs::read(&pb).unwrap(), b"B", "内层右分支落盘");
    assert_eq!(std::fs::read(&pc).unwrap(), b"C", "外层右分支落盘");

    // 三级 fd 在两轮 join 后全部可寻址（读回各自内容）。
    for (fd, expect) in [(fda, b"A"), (fdb, b"B"), (fdc, b"C")] {
        rt.run_blocking(syscall(
            DataOp::Seek {
                fd,
                offset: 0,
                whence: std::io::SeekFrom::Start(0),
            },
            vec![rd(fd)],
            Action::Pure,
        ))
        .unwrap();
        let b = rt
            .run_blocking(syscall(
                DataOp::Read { fd, len: 1 },
                vec![rd(fd)],
                Action::Pure,
            ))
            .unwrap();
        assert_eq!(
            b,
            Value::Bytes(expect.to_vec()),
            "嵌套分支 fd join 后仍可读"
        );
    }
    for fd in [fda, fdb, fdc] {
        rt.run_blocking(syscall(DataOp::Close { fd }, vec![ow(fd)], Action::Pure))
            .unwrap();
    }
    // 三个分支各 1 条 Write undo（Full 策略）→ 断言并 Replace 清理。
    assert_eq!(
        rt.undo_stack().len(),
        3,
        "三层分支的 Write undo 均逐级合并回父（两级 Fork 合并完整性）"
    );
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty(), "Replace 清理后撤销栈干净");
}
