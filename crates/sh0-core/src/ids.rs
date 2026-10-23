use ulid::Ulid;

/// 主机配置 ID（ULID 字符串，时间有序，便于日志排序）
pub type HostId = String;

/// 运行时会话 ID
pub type SessionId = String;

pub fn new_host_id() -> HostId {
    Ulid::generate().to_string()
}

pub fn new_session_id() -> SessionId {
    Ulid::generate().to_string()
}
