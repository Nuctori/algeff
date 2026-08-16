# 使用示例

> 本页是 do_! 新语法下的完整示例集。**全部示例与 `crates/algeff-std/tests/docs_examples.rs` / `dx_examples.rs` / `readme_examples.rs` 逐字一致**（照抄可跑，改动测试即失败）。手写 CPS 旧写法（`Action::Syscall` + `next` 闭包嵌套）见 README §3 折叠对比块与 `pdr.md` §14。

## 1. 完整文件面操作：一个 do_! 块覆盖

Mkdir / Open / Write / Seek / Read / Stat / Close / Unlink，尾表达式用 `Value::List` 组合两个链内值，全链真实执行：

```rust
let blueprint = do_! {
    dx::mkdir(&sub, 0o755);
    let fd = dx::open(fc.clone(), open_rw_create());
    dx::write(&fd, b"dx roundtrip".to_vec());
    dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
    let data = dx::read(&fd, 64);
    let meta = dx::stat(fc.clone());
    dx::close(&fd);
    dx::unlink(fc.clone());
    Value::List(vec![data, meta])
};
```

其中 `open_rw_create` 与 `fc` 的约定（`dx_examples.rs::file_ops_comprehensive_roundtrip`）：

```rust
fn open_rw_create() -> OpenFlags {
    OpenFlags { read: true, write: true, create: true, ..Default::default() }
}

// 文件在链内多处使用：clone 后 move 进闭包（'static），外部保留原件断言。
let fc = file.clone();
```

执行结果：`data` = `Value::Bytes(b"dx roundtrip")`；`meta` = `Value::List([len, is_dir, is_file])` = `List([12, false, true])`；`unlink` 后文件已删除、目录仍在。

## 2. TCP echo do_! 版

绑定 → accept → read → write(echo) → shutdown，服务端用 do_! 蓝图，客户端用 tokio 原生 `TcpStream`（测试 `docs_tcp_echo_do`，真实端到端）：

```rust
// 1. 先绑定（127.0.0.1:0 → 内核分配端口），从 registry 取回真实地址。
let lfd = rt.run_blocking(dx::open_tcp(addr)).unwrap();
let lfd = dx::expect_fd(&lfd);
let real_addr: std::net::SocketAddr = match rt.registry().lookup(lfd).unwrap() {
    ResourceHandle::TcpListener(l) => l.local_addr().unwrap(),
    other => panic!("期望 TcpListener 句柄，得到 {other:?}"),
};

// 2. 客户端线程：tokio 原生 TcpStream 连接并收发小 payload（10s 超时防悬挂）。
let payload: Vec<u8> = b"echo-do".to_vec(); // 小 payload：单次 TcpRead 即收满
let n = payload.len();
let client_payload = payload.clone();
let client = std::thread::spawn(move || {
    let client_rt = tokio::runtime::Runtime::new().unwrap();
    client_rt.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut s = tokio::net::TcpStream::connect(real_addr).await.unwrap();
            s.write_all(&client_payload).await.unwrap();
            let mut buf = vec![0u8; client_payload.len()];
            s.read_exact(&mut buf).await.unwrap();
            buf
        })
        .await
        .expect("客户端连接/收发 10s 超时")
    })
});

// 3. 服务端 do_! 蓝图：accept → read → write(echo) → shutdown。
let blueprint = do_! {
    let conn = dx::accept(&Value::Fd(lfd));
    let sfd = dx::pure(Value::Fd(dx::expect_fd(&dx::expect_list(&conn)[0])));
    let data = dx::tcp_read(&sfd, n);
    dx::tcp_write(&sfd, dx::expect_bytes(&data));
    dx::tcp_shutdown(&sfd, std::net::Shutdown::Both);
    Value::U64(n as u64)
};
let v = rt.run_blocking(blueprint).unwrap();
```

两个诚实的注记：

- **第 3 步的提取行**：`accept` 返回 `Value::List([Fd, Addr])`，而 do_! 语句槽只接受 `Action` 表达式，所以用 `dx::pure(Value::Fd(dx::expect_fd(...)))` 包一层运行时提取——这是 DX 层的**已知丑点**（`dx-design.md` §7：`expect_*` 系列是为此设计的，但包装样板欠佳）；
- **单次 read**：本示例读一次即 echo（适合小 payload）。需要「循环收满再回写」时，do_! 没有循环节点，请用手写 CPS 版本（`tests/e2e.rs::echo_server_chain` 的 `tcp_read_all` 循环，1KB 真实对测）。

## 3. 锁：同 id 可重入

`lock → unlock → 再 lock` 同 id 成功（R7 修复回归语义）：锁仲裁在 executor 层 arbiter（占坑 ⟺ 持锁），A4 线性域不消费锁 id（空声明）——**可重入**：

```rust
let blueprint = do_! {
    dx::mutex_lock(7);
    dx::mutex_unlock(7);
    dx::mutex_lock(7);
    dx::mutex_unlock(7);
    Value::Unit
};
rt.run_blocking(blueprint).unwrap();
```

## 4. 信号：二次发送不被 A4 拒绝

派生长存活子进程 → 两次 `SIGKILL`（9）→ 收割。`SendSignal` 空声明（Signal 全局资源无仲裁层），二次发送语义上允许（SIGTERM→SIGKILL 优雅停机模式），成败由物理层决定，**绝不由 A4 线性域拒绝**：

```rust
// 长存活子进程（平台差异：Unix sh -c sleep；Windows cmd timeout）。
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
let pid = rt.run_blocking(dx::spawn(cmd)).unwrap();
assert!(matches!(pid, Value::Pid(_)));

// 第一次 SIGKILL（9）：路由 op_kill → 物理杀进程。
rt.run_blocking(dx::send_signal(9, &pid)).unwrap();

// 第二次 SIGKILL：修复前在 A4 层被拒（InvalidInput）；修复后进入物理层
// （已终止子进程 start_kill 幂等成功，或平台层错误），绝不可能是 InvalidInput。
let second = rt.run_blocking(dx::send_signal(9, &pid));
match second {
    Ok(_) => {}
    Err(e) => assert!(!matches!(e, SysError::InvalidInput)),
}

// 收割子进程（退出码 1 = 信号终止，平台差异不断言具体值）。
let v = rt.run_blocking(dx::wait(&pid)).unwrap();
assert!(matches!(v, Value::U64(_)));
```

## 5. do_! 与 plan!/fork! 混合

命令式骨架（do_!）内嵌声明式子步骤（plan!）与并发分叉（fork!），左右分支写不同文件（无资源冲突），真实执行：

```rust
// do_!/plan!/fork! 内嵌的 do_! 均预构建 Action 值（do_! 展开闭包要求
// 'static、plan!/fork! 只做值组合）。
let mkdir_act = do_! { dx::mkdir(&sub, 0o755); Value::Unit };
let left_act = do_! {
    let f = dx::open(fa.clone(), open_rw_create());
    dx::write(&f, b"A".to_vec());
    dx::close(&f);
    Value::Unit
};
let right_act = do_! {
    let f = dx::open(fb.clone(), open_rw_create());
    dx::write(&f, b"B".to_vec());
    dx::close(&f);
    Value::Unit
};
let stat_act = do_! {
    let _ = dx::stat(&sub);
    Value::Unit
};

let blueprint = do_! {
    plan! { mkdir_act };
    fork! {
        left: left_act,
        right: right_act,
    };
    stat_act;
    Value::U64(42)
};

let v = rt.run_blocking(blueprint).unwrap();
assert_eq!(v, Value::U64(42));
assert_eq!(std::fs::read(&fa).unwrap(), b"A");
assert_eq!(std::fs::read(&fb).unwrap(), b"B");
```

要点：

- **预构建规则**：`plan!`/`fork!` 内嵌 `do_!` 需先构建成 `Action` 值（`plan!` continuation 闭包非 `move`、`do_!` 展开闭包要求 `'static`）——「蓝图即值、再组合」；
- `plan!` 链收敛为 `Pure(Unit)`（元素值被忽略）：需要值传递时用 `do_!` 作外层；
- `do_!` 内也可嵌 `choose!` 条件分支（`dx_examples.rs::do_block_embeds_plan_and_choose`）。

## 6. 资源自动推导与显式覆盖

`dx` 每个操作按 `DataOp` 自动推导资源声明（`infer_usage` 模式表）。需要精确控制时用 `dx::syscall_with` 显式覆盖（`syscall_with` > `infer_usage` > 空集）：

```rust
// 自动推导：write 模式 → Write(path)
let auto = dx::open(
    "hello.txt",
    OpenFlags { write: true, ..Default::default() },
);

// 显式覆盖：自定义资源声明完全替换默认推导
let custom = dx::syscall_with(
    DataOp::Open {
        path: "hello.txt".into(),
        flags: OpenFlags { write: true, ..Default::default() },
    },
    vec![ResourceUsage {
        resource: Resource::Path("/custom".into()),
        mode: AccessMode::Read,
    }],
);
```

完整的推导模式表（含空集边界：TcpBind/Spawn 等运行时句柄、TcpShutdown 半关闭、Mutex 锁仲裁、SendSignal 可重复）见 [DX 语法糖层设计](dx-design.md) §3。

## 文档入口

- `tests/docs_examples.rs`：本页示例的可执行镜像（逐字护栏）；
- `tests/dx_examples.rs`：DX 迭代 1→2 全量示例测试（15 项，含结构断言）；
- `tests/e2e.rs`：真实端到端（文件/管道/TCP echo 循环版/撤销）；
- `tests/readme_examples.rs`：README 示例镜像。
