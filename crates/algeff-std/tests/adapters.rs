//! 适配器冒烟测试：验证预包装函数构造出正确的 Action（op + 资源声明，pdr.md §14 风格）。

use algeff_core::{AccessMode, Action, DataOp, OpenFlags, Resource, Value};
use algeff_std::adapters;

#[test]
fn open_tcp_constructs_bind() {
    let a = adapters::open_tcp("127.0.0.1:8080".parse().unwrap());
    match a {
        Action::Syscall {
            op: DataOp::TcpBind { .. },
            resources,
            next,
        } => {
            assert!(
                resources.is_empty(),
                "bind 的新句柄运行时才分配，资源集应为空"
            );
            assert!(matches!(next(Value::Unit), Action::Pure(Value::Unit)));
        }
        _ => panic!("open_tcp 应构造 TcpBind Syscall 节点"),
    }
}

#[test]
fn read_declares_read_mode_on_fd() {
    let a = adapters::read(42, 1024);
    match a {
        Action::Syscall {
            op: DataOp::Read { fd, len },
            resources,
            ..
        } => {
            assert_eq!(fd, 42);
            assert_eq!(len, 1024);
            assert_eq!(resources.len(), 1);
            assert_eq!(resources[0].resource, Resource::Fd(42));
            assert_eq!(resources[0].mode, AccessMode::Read);
        }
        _ => panic!("read 应构造 Read Syscall 节点"),
    }
}

#[test]
fn close_declares_own_mode() {
    let a = adapters::close(7);
    match a {
        Action::Syscall {
            op: DataOp::Close { fd },
            resources,
            ..
        } => {
            assert_eq!(fd, 7);
            assert_eq!(resources[0].resource, Resource::Fd(7));
            assert_eq!(resources[0].mode, AccessMode::Own);
        }
        _ => panic!("close 应构造 Close Syscall 节点"),
    }
}

#[test]
fn open_file_declares_path_mode() {
    let a = adapters::open_file(
        "/tmp/f.txt".into(),
        OpenFlags {
            write: true,
            create: true,
            ..Default::default()
        },
    );
    match a {
        Action::Syscall {
            op: DataOp::Open { .. },
            resources,
            ..
        } => {
            assert_eq!(resources[0].resource, Resource::Path("/tmp/f.txt".into()));
            assert_eq!(resources[0].mode, AccessMode::Write);
        }
        _ => panic!("open_file 应构造 Open Syscall 节点"),
    }
}

#[test]
fn sleep_constructs_sleep_node() {
    let a = adapters::sleep(std::time::Duration::from_millis(10));
    match a {
        Action::Sleep { duration, next } => {
            assert_eq!(duration, std::time::Duration::from_millis(10));
            assert!(matches!(next(Value::Unit), Action::Pure(Value::Unit)));
        }
        _ => panic!("sleep 应构造 Sleep 节点"),
    }
}
