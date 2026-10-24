//! 本机 IPC 传输层。
//!
//! M0 落地的是 **loopback TCP + 一次性令牌** 后端：跨平台行为一致、tokio 原生
//! 支持、实现成本最低，让 M0 专注打通链路而不是和 Named Pipe 权限较劲。
//!
//! `IpcClient` / `serve` 只依赖下面的消息抽象，后续换 Unix Domain Socket
//! （Unix）或 Named Pipe（Windows）时，只需替换这两个函数内部，协议与
//! 上层代码不动 —— 对应需求文档 OQ-1 的选型待定项。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio::io::AsyncBufReadExt;

use crate::error::{IpcError, Result};
use crate::protocol::{ClientMsg, ErrorBody, ServerMsg};

/// 事件广播中心：daemon 侧 emit，所有已连接前端都会收到
#[derive(Debug, Clone)]
pub struct EventHub {
    tx: broadcast::Sender<ServerMsg>,
}

impl EventHub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { tx }
    }

    pub fn emit(&self, msg: ServerMsg) {
        // 无订阅者时 send 会报错，这是正常的
        let _ = self.tx.send(msg);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerMsg> {
        self.tx.subscribe()
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

/// daemon 侧的方法处理器。
///
/// 单独抽成 trait 是为了让 core 层不必知道任何网络细节（NFR-9）。
#[async_trait]
pub trait Handler: Send + Sync {
    async fn call(
        &self,
        method: &str,
        params: Value,
        hub: &EventHub,
    ) -> std::result::Result<Value, ErrorBody>;
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<ServerMsg>>>>;

/// 前端侧客户端（TUI 与 `sh0 ai` 共用）
pub struct IpcClient {
    out_tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
}

/// 一次连接：客户端句柄 + 事件流
pub struct IpcConnection {
    pub client: Arc<IpcClient>,
    pub events: mpsc::UnboundedReceiver<ServerMsg>,
}

impl IpcClient {
    /// 连接 daemon 并完成令牌握手
    pub async fn connect(addr: &str, token: &str) -> Result<IpcConnection> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| IpcError::Unreachable(format!("{addr}: {e}")))?;
        let (rh, mut wh) = stream.into_split();

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<ServerMsg>();

        // 写任务：把待发消息串行写出
        tokio::spawn(async move {
            while let Some(line) = out_rx.recv().await {
                if wh.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                let _ = wh.flush().await;
            }
        });

        // 读任务：区分应答与事件
        let pending2 = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(rh).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<ServerMsg>(&line) {
                            Ok(ServerMsg::Response { id, .. }) => {
                                if let Some(tx) = pending2.lock().await.remove(&id)
                                    && let Ok(msg) = serde_json::from_str::<ServerMsg>(&line) {
                                        let _ = tx.send(msg);
                                    }
                            }
                            Ok(evt @ ServerMsg::Event { .. }) => {
                                // 事件是**可选**流：一次性客户端（`sh0 ai`、脚本）不订阅，
                                // 通道早已随 receiver 一起丢弃。此时只能丢弃事件继续读——
                                // 一旦在此 break，读任务退出，后续所有应答再无人分发，
                                // 调用方就会永久挂起（M2 契约测试就是这么挂的）。
                                let _ = ev_tx.send(evt);
                            }
                            Err(e) => {
                                tracing::warn!("收到无法解析的帧: {e}");
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!("IPC 读连接关闭: {e}");
                        break;
                    }
                }
            }
        });

        let client = Arc::new(IpcClient {
            out_tx,
            pending,
            next_id: AtomicU64::new(1),
        });

        // 握手帧
        let auth = serde_json::to_string(&ClientMsg::Auth {
            token: token.to_string(),
        })?;
        client.raw_send(auth)?;

        Ok(IpcConnection {
            client,
            events: ev_rx,
        })
    }

    fn raw_send(&self, line: String) -> Result<()> {
        let mut line = line;
        line.push('\n');
        self.out_tx
            .send(line)
            .map_err(|_| IpcError::Disconnected)?;
        Ok(())
    }

    /// 调用一个方法并等待应答
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let frame = serde_json::to_string(&ClientMsg::Request {
            id,
            method: method.to_string(),
            params,
        })?;
        self.raw_send(frame)?;

        let resp = rx.await.map_err(|_| IpcError::Disconnected)?;
        match resp {
            ServerMsg::Response {
                ok: true, data, ..
            } => Ok(data.unwrap_or(Value::Null)),
            ServerMsg::Response {
                ok: false,
                error: Some(e),
                ..
            } => Err(IpcError::Remote {
                code: e.code,
                message: e.message,
                data: e.data,
            }),
            _ => Err(IpcError::Protocol("unexpected frame".to_string())),
        }
    }
}

/// 启动 daemon 端监听。绑定到 127.0.0.1，不对外可达（NFR-7）。
pub async fn serve<H>(addr: SocketAddr, token: String, handler: Arc<H>, hub: EventHub) -> Result<()>
where
    H: Handler + 'static,
{
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("daemon 监听 {addr}");

    loop {
        let (stream, peer) = listener.accept().await?;
        tracing::debug!("前端接入 {peer}");
        let token = token.clone();
        let handler = handler.clone();
        let hub = hub.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, token, handler, hub).await {
                tracing::debug!("连接处理结束: {e}");
            }
        });
    }
}

async fn handle_conn<H>(
    stream: TcpStream,
    token: String,
    handler: Arc<H>,
    hub: EventHub,
) -> Result<()>
where
    H: Handler + 'static,
{
    let (rh, mut wh) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

    // 写任务：所有下行帧统一经 out_tx 串行写出，避免多处持有 write half
    tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if wh.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = wh.flush().await;
        }
    });

    // 事件转发
    let mut ev_rx = hub.subscribe();
    let ev_tx = out_tx.clone();
    tokio::spawn(async move {
        while let Ok(msg) = ev_rx.recv().await {
            match serde_json::to_string(&msg) {
                Ok(mut s) => {
                    s.push('\n');
                    if ev_tx.send(s).is_err() {
                        break;
                    }
                }
                Err(e) => tracing::warn!("事件序列化失败: {e}"),
            }
        }
    });

    let mut authed = false;
    let mut lines = BufReader::new(rh).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: ClientMsg = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("协议解析失败: {e}");
                continue;
            }
        };

        match msg {
            ClientMsg::Auth { token: got } => {
                if got != token {
                    let mut s = serde_json::to_string(&ServerMsg::err(
                        0,
                        "unauthorized",
                        "令牌不匹配",
                    ))?;
                    s.push('\n');
                    let _ = out_tx.send(s);
                    return Err(IpcError::Unauthorized("token mismatch".into()));
                }
                authed = true;
            }
            ClientMsg::Request { id, method, params } => {
                if !authed {
                    let mut s = serde_json::to_string(&ServerMsg::err(
                        id,
                        "unauthorized",
                        "未鉴权",
                    ))?;
                    s.push('\n');
                    let _ = out_tx.send(s);
                    continue;
                }
                tracing::debug!("收到请求 id={id} method={method}");
                let resp = match handler.call(&method, params, &hub).await {
                    Ok(data) => ServerMsg::ok(id, data),
                    Err(e) => match e.data {
                        Some(d) => ServerMsg::err_with_data(id, e.code, e.message, d),
                        None => ServerMsg::err(id, e.code, e.message),
                    },
                };
                match serde_json::to_string(&resp) {
                    Ok(mut s) => {
                        s.push('\n');
                        if out_tx.send(s).is_err() {
                            return Err(IpcError::Disconnected);
                        }
                        tracing::debug!("已应答 id={id} method={method}");
                    }
                    Err(e) => tracing::warn!("应答序列化失败: {e}"),
                }
            }
        }
    }

    Ok(())
}
