//! 核心错误枚举 —— 契约冻结（pdr.md §10，14 种 POSIX 错误 + Other 兜底）。

use std::fmt;

/// 14 种 POSIX 错误 + 兜底（pdr.md §10.1）。`Other` 保留原始 errno，
/// 但破坏编译期穷尽性检查，用户需自行处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SysError {
    NotFound,          // ENOENT
    PermissionDenied,  // EACCES
    WouldBlock,        // EAGAIN / EWOULDBLOCK
    Interrupted,       // EINTR
    TimedOut,          // ETIMEDOUT
    ConnectionReset,   // ECONNRESET
    ConnectionRefused, // ECONNREFUSED
    BrokenPipe,        // EPIPE
    StorageFull,       // ENOSPC / EDQUOT
    InvalidInput,      // EINVAL
    AlreadyExists,     // EEXIST
    NotADirectory,     // ENOTDIR
    IsADirectory,      // EISDIR
    CrossDevice,       // EXDEV
    Other(i32),        // 兜底，不参与穷尽性检查
}

impl SysError {
    /// 原始 errno；`Other(n)` 返回 n，其余返回语义映射（ENOENT=2 等，见 libc 惯例）。
    pub fn code(&self) -> i32 {
        match self {
            SysError::NotFound => 2,
            SysError::PermissionDenied => 13,
            SysError::WouldBlock => 11,
            SysError::Interrupted => 4,
            SysError::TimedOut => 110,
            SysError::ConnectionReset => 104,
            SysError::ConnectionRefused => 111,
            SysError::BrokenPipe => 32,
            SysError::StorageFull => 28,
            SysError::InvalidInput => 22,
            SysError::AlreadyExists => 17,
            SysError::NotADirectory => 20,
            SysError::IsADirectory => 21,
            SysError::CrossDevice => 18,
            SysError::Other(n) => *n,
        }
    }

    pub fn from_errno(errno: i32) -> Self {
        match errno {
            2 => SysError::NotFound,
            13 => SysError::PermissionDenied,
            11 => SysError::WouldBlock,
            4 => SysError::Interrupted,
            110 => SysError::TimedOut,
            104 => SysError::ConnectionReset,
            111 => SysError::ConnectionRefused,
            32 => SysError::BrokenPipe,
            28 => SysError::StorageFull,
            22 => SysError::InvalidInput,
            17 => SysError::AlreadyExists,
            20 => SysError::NotADirectory,
            21 => SysError::IsADirectory,
            18 => SysError::CrossDevice,
            n => SysError::Other(n),
        }
    }
}

impl fmt::Display for SysError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            SysError::NotFound => "NotFound",
            SysError::PermissionDenied => "PermissionDenied",
            SysError::WouldBlock => "WouldBlock",
            SysError::Interrupted => "Interrupted",
            SysError::TimedOut => "TimedOut",
            SysError::ConnectionReset => "ConnectionReset",
            SysError::ConnectionRefused => "ConnectionRefused",
            SysError::BrokenPipe => "BrokenPipe",
            SysError::StorageFull => "StorageFull",
            SysError::InvalidInput => "InvalidInput",
            SysError::AlreadyExists => "AlreadyExists",
            SysError::NotADirectory => "NotADirectory",
            SysError::IsADirectory => "IsADirectory",
            SysError::CrossDevice => "CrossDevice",
            SysError::Other(n) => return write!(f, "Other({n})"),
        };
        write!(f, "{name}(errno {})", self.code())
    }
}

impl std::error::Error for SysError {}

impl From<std::io::Error> for SysError {
    fn from(e: std::io::Error) -> Self {
        match e.raw_os_error() {
            Some(n) => SysError::from_errno(n),
            None => SysError::Other(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_roundtrip() {
        assert_eq!(SysError::from_errno(2), SysError::NotFound);
        assert_eq!(SysError::from_errno(999), SysError::Other(999));
        assert_eq!(SysError::NotFound.code(), 2);
        assert_eq!(SysError::Other(7).code(), 7);
    }

    #[test]
    fn io_error_maps_errno() {
        let e = std::io::Error::from_raw_os_error(13);
        assert_eq!(SysError::from(e), SysError::PermissionDenied);
    }
}
