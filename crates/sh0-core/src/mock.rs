//! 用于测试与契约验证的假传输层（需求 NFR-10）。
//!
//! 放在 core 而不是测试目录里，是因为 M2 的契约测试、`ai` 子命令集成测试
//! 都要用它——没有它，"AI 接口是否真的能驱动一个会话"就只能靠真机验证。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::transport::{
    SessionTransport, TransportError, TransportEvent, TransportFactory, TransportResult,
};
use crate::Host;

/// 脚本耗尽后的行为
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnEmpty {
    /// 返回 None：连接正常关闭，不重连
    Close,
    /// 报告断开：触发重连流程
    Broken,
    /// 保持连接但不再产生数据。测试里很关键——脚本一播完会话就关闭的话，
    /// 后面的断言根本没机会执行。
    Idle,
}

/// 脚本化的假连接
pub struct MockTransport {
    script: VecDeque<TransportEvent>,
    on_empty: OnEmpty,
    pub sent: Vec<Vec<u8>>,
    pub resizes: Vec<(u16, u16)>,
    pub closed: bool,
}

impl MockTransport {
    /// 按脚本构造。脚本可以是 `TransportEvent`，也可以直接用字节切片字面量。
    pub fn new(events: Vec<TransportEvent>, on_empty: OnEmpty) -> Self {
        Self {
            script: events.into_iter().collect(),
            on_empty,
            sent: Vec::new(),
            resizes: Vec::new(),
            closed: false,
        }
    }

    pub fn from_bytes(chunks: &[&[u8]], on_empty: OnEmpty) -> Self {
        Self::new(
            chunks.iter().map(|b| TransportEvent::Data(b.to_vec())).collect(),
            on_empty,
        )
    }

    /// 播放完后转为 Box<dyn SessionTransport>
    pub fn boxed(self) -> Box<dyn SessionTransport + Send> {
        Box::new(self)
    }
}

#[async_trait]
impl SessionTransport for MockTransport {
    async fn wait(&mut self) -> Option<TransportEvent> {
        match self.script.pop_front() {
            Some(ev) => Some(ev),
            None => match self.on_empty {
                OnEmpty::Close => None,
                OnEmpty::Broken => {
                    // 只报一次，避免测试里无限重连
                    self.on_empty = OnEmpty::Close;
                    Some(TransportEvent::Broken("mock 断开".into()))
                }
                OnEmpty::Idle => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    Some(TransportEvent::Data(Vec::new()))
                }
            },
        }
    }

    async fn send(&mut self, data: &[u8]) -> TransportResult<()> {
        self.sent.push(data.to_vec());
        Ok(())
    }

    async fn resize(&mut self, cols: u16, rows: u16) -> TransportResult<()> {
        self.resizes.push((cols, rows));
        Ok(())
    }

    async fn close(&mut self) -> TransportResult<()> {
        self.closed = true;
        Ok(())
    }
}

/// 可控失败的工厂。
///
/// 三个维度可以分别注入：
///   - `fail_times`：前 N 次 `connect` 直接失败（模拟服务端不可达）
///   - `disconnect_times`：前 N 次连接播完脚本后**报断线**（模拟网络闪断，触发重连）
///   - 其余连接播完脚本后正常关闭
///
/// 必须能分别控制，否则"播完就断"会让每次重连都再断一次，
/// 测试里表现为无限重连，掩盖真正要验证的行为。
#[derive(Clone)]
pub struct MockFactory {
    scripts: Arc<Mutex<VecDeque<Vec<TransportEvent>>>>,
    fail_times: Arc<Mutex<u32>>,
    disconnect_times: Arc<Mutex<u32>>,
    pub connect_calls: Arc<Mutex<u32>>,
}

impl MockFactory {
    fn new(scripts: Vec<Vec<TransportEvent>>, fail_times: u32, disconnect_times: u32) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(VecDeque::from(scripts))),
            fail_times: Arc::new(Mutex::new(fail_times)),
            disconnect_times: Arc::new(Mutex::new(disconnect_times)),
            connect_calls: Arc::new(Mutex::new(0)),
        }
    }

    /// 每次连接播同一份脚本，播完正常关闭（不触发重连）
    pub fn repeating(script: Vec<TransportEvent>, fail_times: u32) -> Self {
        Self::new(vec![script], fail_times, 0)
    }

    /// 按序取脚本，播完正常关闭
    pub fn sequenced(scripts: Vec<Vec<TransportEvent>>, fail_times: u32) -> Self {
        Self::new(scripts, fail_times, 0)
    }

    /// 前 `disconnect_times` 次连接播完脚本后报断线，用于验证重连链路（AC-3）
    pub fn disconnecting(
        scripts: Vec<Vec<TransportEvent>>,
        fail_times: u32,
        disconnect_times: u32,
    ) -> Self {
        Self::new(scripts, fail_times, disconnect_times)
    }

    pub fn calls(&self) -> u32 {
        *self.connect_calls.lock().unwrap()
    }
}

#[async_trait]
impl TransportFactory for MockFactory {
    async fn connect(
        &self,
        _host: &Host,
        _cols: u16,
        _rows: u16,
    ) -> TransportResult<Box<dyn SessionTransport + Send>> {
        *self.connect_calls.lock().unwrap() += 1;

        let mut fails = self.fail_times.lock().unwrap();
        if *fails > 0 {
            *fails -= 1;
            return Err(TransportError::Connect("mock 连接失败".into()));
        }
        drop(fails);

        let mut scripts = self.scripts.lock().unwrap();
        let script = match scripts.pop_front() {
            Some(s) => {
                scripts.push_back(s.clone());
                s
            }
            None => Vec::new(),
        };
        drop(scripts);

        let mut disc = self.disconnect_times.lock().unwrap();
        let on_empty = if *disc > 0 {
            *disc -= 1;
            OnEmpty::Broken
        } else {
            OnEmpty::Idle
        };
        drop(disc);

        Ok(MockTransport::new(script, on_empty).boxed())
    }
}
