//! 临时探针：寻找当前栈溢出边界（RFC-11 守卫回归排查用，验收后删除）。
use algeff_core::{Action, BoxFuture, ResourceRegistry, Runtime, SysError, SyscallExecutor, UndoOp, Value};

struct NoopExecutor;
impl SyscallExecutor for NoopExecutor {
    fn execute<'a>(
        &'a mut self,
        _op: &'a algeff_core::DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, Option<UndoOp>), SysError>> {
        Box::pin(async { Ok((Value::Unit, None)) })
    }
}

fn nested_seq(depth: u64) -> Action {
    if depth == 0 {
        return Action::Pure(Value::U64(300));
    }
    Action::Sequential {
        current: Box::new(nested_seq(depth - 1)),
        next: Box::new(|v| Action::Pure(v)),
    }
}

#[test]
fn probe_crash_boundary() {
    let mut rt = Runtime::new(Box::new(NoopExecutor));
    let mut ok_high = 0u64;
    for d in 95..=160u64 {
        match rt.run_blocking(nested_seq(d)) {
            Ok(_) => {
                ok_high = d;
                eprintln!("depth {d}: ok");
            }
            Err(SysError::Other(105)) => {
                eprintln!("depth {d}: guard fired (boundary between {ok_high} and {d})");
                return;
            }
            Err(e) => panic!("depth {d}: unexpected {e:?}"),
        }
    }
    eprintln!("probe: no guard through 90 (max ok {ok_high})");
}
