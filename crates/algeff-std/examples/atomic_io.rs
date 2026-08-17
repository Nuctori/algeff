//! 集中演示：Algeff 解决的普通 IO 库痛点
//!
//! 一个 main，四个场景：
//!   1. 原子文件写入（中途失败自动回滚）
//!   2. 同一蓝图可重放
//!   3. 一键撤销全部副作用
//!   4. 小蓝图自由组合成大蓝图

use algeff_core::prelude::*;
use algeff_core::OpenFlags;
use algeff_macro::do_;
use algeff_std::dx;
use algeff_std::TokioExecutor;

fn main() {
    let dir = std::env::temp_dir().join("algeff-demo");
    std::fs::create_dir_all(&dir).unwrap();

    // ================================================================
    // 痛点 1：原子文件写入
    //
    // 普通 std::fs：open → write → close，中途崩溃？文件半写状态。
    //
    // Algeff：蓝图 = 数据，运行时自动追踪逆操作。
    //         任何一步失败，自动回滚已做的全部副作用。
    // ================================================================
    println!("=== 痛点 1：原子文件写入 ===");

    let path1 = dir.join("atomic.txt");
    std::fs::write(&path1, "before").unwrap();
    println!("  写入前: {:?}", std::fs::read_to_string(&path1).unwrap());

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let p = path1.clone();
    let blueprint = do_! {
        let fd = dx::open(&p, OpenFlags {
            read: true, write: true, create: true, truncate: true, ..Default::default()
        });
        dx::write(&fd, b"hello algeff atomic write".to_vec());
        dx::close(&fd);
        Value::Unit
    };

    rt.run_blocking(blueprint).unwrap();
    println!("  写入后: {:?}", std::fs::read_to_string(&path1).unwrap());
    println!();

    // ================================================================
    // 痛点 2：同一蓝图可重放
    //
    // 普通 IO：想重跑一遍？复制粘贴函数，或手动 mock。
    //
    // Algeff：蓝图是数据，跑多少次都行，结果完全一致。
    // ================================================================
    println!("=== 痛点 2：操作可重放 ===");

    let replay_path = dir.join("replay.txt");
    for i in 0..3 {
        let p = replay_path.clone();
        let mut rt2 = Runtime::new(Box::new(TokioExecutor::new()));
        let result = rt2.run_blocking(do_! {
            let fd = dx::open(&p, OpenFlags {
                read: true, write: true, create: true, truncate: true, ..Default::default()
            });
            dx::write(&fd, format!("round {i}").into_bytes());
            dx::close(&fd);

            let fd2 = dx::open(&p, OpenFlags { read: true, ..Default::default() });
            let data = dx::read(&fd2, 64);
            dx::close(&fd2);
            data
        }).unwrap();
        println!("  第 {} 次: {:?}", i + 1, result);
    }
    println!();

    // ================================================================
    // 痛点 3：一键撤销
    //
    // 普通 IO：做了 N 步想回滚？手动写 N 个补偿逻辑，还容易漏。
    //
    // Algeff：Replace 节点 = recover(回滚全部已做操作) + 执行新蓝图。
    //         这是 Algeff 的核心能力：副作用可撤销。
    //
    // 关键：Open 必须带 read: true，否则 Write 的 Full 撤销
    //       因写前读失败降级为 None（Windows 只写句柄不支持读）。
    // ================================================================
    println!("=== 痛点 3：一键撤销 ===");

    let undo_path = dir.join("undo.txt");
    std::fs::write(&undo_path, "original").unwrap();
    println!("  撤销前: {:?}", std::fs::read_to_string(&undo_path).unwrap());

    let mut rt3 = Runtime::new(Box::new(TokioExecutor::new()));
    let p = undo_path.clone();

    // 第一步：做有副作用的操作（read: true → Full 撤销策略 → undo 压栈）
    rt3.run_blocking(do_! {
        let fd = dx::open(&p, OpenFlags {
            read: true, write: true, create: true, ..Default::default()
        });
        dx::write(&fd, b"this will be undone".to_vec());
        dx::close(&fd);
        Value::Unit
    }).unwrap();
    println!("  做完副作用: {:?}", std::fs::read_to_string(&undo_path).unwrap());

    // 第二步：Replace 一键回滚（撤销步骤 1 的全部效果）
    rt3.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    }).unwrap();
    println!("  撤销后: {:?}", std::fs::read_to_string(&undo_path).unwrap());
    println!();

    // ================================================================
    // 痛点 4：组合性
    //
    // 普通 IO：函数调用耦合在控制流里，没法拆分复用。
    //
    // Algeff：每个操作是独立的数据片段，自由组合。
    //         5 个小蓝图 → reduce 成一条大蓝图 → 一次执行。
    // ================================================================
    println!("=== 痛点 4：小蓝图拼大蓝图 ===");

    let batch_dir = dir.join("batch");
    std::fs::create_dir_all(&batch_dir).unwrap();

    let mut steps: Vec<Action> = Vec::new();
    for i in 0..5 {
        let p = batch_dir.join(format!("file_{i}.txt"));
        let content = format!("content of file {i}");
        steps.push(do_! {
            let fd = dx::open(&p, OpenFlags {
                read: true, write: true, create: true, ..Default::default()
            });
            dx::write(&fd, content.into_bytes());
            dx::close(&fd);
            Value::Unit
        });
    }

    // 顺序组合：step1 ; step2 ; step3 ; ...
    let combined = steps
        .into_iter()
        .reduce(|acc, step| {
            Action::Sequential {
                current: Box::new(acc),
                next: Box::new(move |_| step),
            }
        })
        .unwrap();

    let mut rt4 = Runtime::new(Box::new(TokioExecutor::new()));
    rt4.run_blocking(combined).unwrap();

    for i in 0..5 {
        let content = std::fs::read_to_string(batch_dir.join(format!("file_{i}.txt"))).unwrap();
        println!("  file_{i}.txt = {:?}", content);
    }

    // 清理
    let _ = std::fs::remove_dir_all(&dir);
    println!("\nDone.");
}
