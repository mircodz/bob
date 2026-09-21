use crate::core::process::ManagedProcess;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;

pub(crate) struct RpcConnection {
    writer: tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    next_id: AtomicI64,
    closed: AtomicBool,
    process: Mutex<Option<ManagedProcess>>,
    framed: bool,
    timeout: Duration,
}

struct PendingRequest<'a> {
    connection: &'a RpcConnection,
    id: i64,
}

impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        self.connection.pending.lock().unwrap().remove(&self.id);
    }
}

// Cancellation during a write can leave a partial frame on the wire.
struct InFlightWrite<'a>(Option<&'a RpcConnection>);

impl Drop for InFlightWrite<'_> {
    fn drop(&mut self) {
        if let Some(connection) = self.0 {
            connection.disconnect();
        }
    }
}

impl RpcConnection {
    pub(crate) fn new(
        writer: impl AsyncWrite + Send + Unpin + 'static,
        process: Option<ManagedProcess>,
        framed: bool,
        timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            writer: tokio::sync::Mutex::new(Box::new(writer)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            closed: AtomicBool::new(false),
            process: Mutex::new(process),
            framed,
            timeout,
        })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn disconnect(&self) {
        self.closed.store(true, Ordering::Release);
        self.pending.lock().unwrap().clear();
        self.process.lock().unwrap().take();
    }

    pub(crate) fn receive(&self, message: Value) {
        if message.get("method").is_some()
            || (message.get("result").is_none() && message.get("error").is_none())
        {
            return;
        }
        if let Some(id) = message["id"].as_i64() {
            if let Some(sender) = self.pending.lock().unwrap().remove(&id) {
                let _ = sender.send(message);
            }
        }
    }

    pub(crate) async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        tokio::time::timeout(self.timeout, async {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (sender, receiver) = oneshot::channel();
            {
                let mut pending = self.pending.lock().unwrap();
                anyhow::ensure!(!self.is_closed(), "connection closed");
                pending.insert(id, sender);
            }
            let _pending = PendingRequest {
                connection: self,
                id,
            };
            self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .await?;
            let reply = receiver
                .await
                .map_err(|_| anyhow::anyhow!("connection closed"))?;
            if let Some(error) = reply.get("error") {
                anyhow::bail!("JSON-RPC error: {}", error);
            }
            Ok(reply.get("result").cloned().unwrap_or(Value::Null))
        })
        .await
        .map_err(|_| anyhow::anyhow!("request '{}' timed out", method))?
    }

    pub(crate) async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        tokio::time::timeout(
            self.timeout,
            self.send(&json!({
                "jsonrpc":"2.0", "method":method, "params":params
            })),
        )
        .await
        .map_err(|_| anyhow::anyhow!("notification '{}' timed out", method))?
    }

    async fn send(&self, message: &Value) -> anyhow::Result<()> {
        let body = serde_json::to_string(message)?;
        let frame = if self.framed {
            format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
        } else {
            format!("{}\n", body)
        };
        let mut writer = self.writer.lock().await;
        anyhow::ensure!(!self.is_closed(), "connection closed");
        let mut in_flight = InFlightWrite(Some(self));
        writer.write_all(frame.as_bytes()).await?;
        writer.flush().await?;
        in_flight.0 = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::poll;
    use std::task::Poll;

    #[tokio::test]
    async fn wire_formats_count_utf8_bytes_and_notifications_have_no_id() {
        use tokio::io::AsyncReadExt;

        for framed in [false, true] {
            let (writer, mut reader) = tokio::io::duplex(4096);
            let connection = RpcConnection::new(writer, None, framed, Duration::from_secs(1));
            let params = json!({"text":"é😀\nnext"});
            let mut request = Box::pin(connection.request("fixture", params.clone()));
            assert!(poll!(&mut request).is_pending());
            connection.notify("notice", params.clone()).await.unwrap();
            let messages = [
                json!({"jsonrpc":"2.0","id":1,"method":"fixture","params":params}),
                json!({"jsonrpc":"2.0","method":"notice","params":params}),
            ];
            for message in messages {
                let body = message.to_string();
                let expected = if framed {
                    format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
                } else {
                    format!("{body}\n")
                };
                let mut bytes = vec![0; expected.len()];
                tokio::time::timeout(Duration::from_secs(1), reader.read_exact(&mut bytes))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(bytes, expected.as_bytes());
            }
            assert_eq!(connection.pending.lock().unwrap().len(), 1);
            connection.receive(json!({"id":1,"result":null}));
            assert_eq!(request.await.unwrap(), Value::Null);
        }
    }

    #[tokio::test]
    async fn out_of_order_replies_keep_rpc_errors_separate_from_successful_nulls() {
        let connection = RpcConnection::new(tokio::io::sink(), None, false, Duration::from_secs(1));
        let mut first = Box::pin(connection.request("first", json!({})));
        let mut second = Box::pin(connection.request("second", json!({})));
        assert!(poll!(&mut first).is_pending());
        assert!(poll!(&mut second).is_pending());
        connection.receive(json!({"id":2,"error":{"code":-32602,"message":"bad arguments","data":{"field":"path"}}}));
        let error = second.await.unwrap_err().to_string();
        assert!(error.starts_with("JSON-RPC error:"));
        assert!(error.contains("-32602"));
        assert!(error.contains("bad arguments"));
        assert!(error.contains("path"));
        assert!(poll!(&mut first).is_pending());
        connection.receive(json!({"id":2,"result":"duplicate"}));
        connection.receive(json!({"id":1,"result":null}));
        assert_eq!(first.await.unwrap(), Value::Null);
        assert!(!connection.is_closed());
        assert!(connection.pending.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn reply_timeout_cleans_pending_without_poisoning_the_connection() {
        let connection = RpcConnection::new(tokio::io::sink(), None, false, Duration::from_secs(1));
        let error = connection.request("slow", Value::Null).await.unwrap_err();
        assert_eq!(error.to_string(), "request 'slow' timed out");
        assert!(connection.pending.lock().unwrap().is_empty());
        assert!(!connection.is_closed());
        let mut next = Box::pin(connection.request("next", Value::Null));
        assert!(poll!(&mut next).is_pending());
        connection.receive(json!({"id":1,"result":"late"}));
        assert!(poll!(&mut next).is_pending());
        connection.receive(json!({"id":2,"result":"current"}));
        assert_eq!(next.await.unwrap(), "current");
    }

    #[tokio::test]
    async fn write_failure_preserves_io_error_and_closes_connection() {
        let (writer, reader) = tokio::io::duplex(256);
        drop(reader);
        let connection = RpcConnection::new(writer, None, true, Duration::from_secs(1));
        let error = connection
            .request("fixture", Value::Null)
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert!(connection.pending.lock().unwrap().is_empty());
        assert!(connection.is_closed());
        assert_eq!(
            connection
                .notify("notice", Value::Null)
                .await
                .unwrap_err()
                .to_string(),
            "connection closed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_write_and_mutex_wait_are_inside_deadline() {
        let (writer, _unread) = tokio::io::duplex(8);
        let connection = RpcConnection::new(writer, None, true, Duration::from_millis(10));
        let error = connection
            .request("fixture", Value::Null)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(connection.is_closed());
        assert!(connection.pending.lock().unwrap().is_empty());

        let connection =
            RpcConnection::new(tokio::io::sink(), None, false, Duration::from_millis(10));
        let lock = connection.writer.lock().await;
        assert!(connection
            .request("fixture", Value::Null)
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        assert!(connection.pending.lock().unwrap().is_empty());
        assert!(!connection.is_closed());
        drop(lock);
    }

    #[tokio::test]
    async fn cancellation_cleans_pending_and_partial_write_disconnects() {
        let connection = RpcConnection::new(tokio::io::sink(), None, false, Duration::from_secs(1));
        let mut request = Box::pin(connection.request("fixture", Value::Null));
        assert!(poll!(&mut request).is_pending());
        assert_eq!(connection.pending.lock().unwrap().len(), 1);
        drop(request);
        assert!(connection.pending.lock().unwrap().is_empty());
        assert!(!connection.is_closed());

        let (writer, _unread) = tokio::io::duplex(1);
        let connection = RpcConnection::new(writer, None, true, Duration::from_secs(1));
        let mut request = Box::pin(connection.request("fixture", Value::Null));
        assert!(poll!(&mut request).is_pending());
        drop(request);
        assert!(connection.pending.lock().unwrap().is_empty());
        assert!(connection.is_closed());
    }

    #[tokio::test]
    async fn wrong_ids_and_server_requests_do_not_complete_reply() {
        let connection = RpcConnection::new(tokio::io::sink(), None, false, Duration::from_secs(1));
        let mut request = Box::pin(connection.request("fixture", Value::Null));
        assert!(poll!(&mut request).is_pending());
        for message in [
            json!({"id":2,"result":"wrong"}),
            json!({"result":"missing"}),
            json!({"id":1,"method":"server/request"}),
            json!({"id":1,"method":"server/request","result":"not a reply"}),
            json!({"id":"1","result":"string id"}),
            json!({"id":1}),
            json!({"id":null,"error":{"code":-1}}),
        ] {
            connection.receive(message);
            assert!(poll!(&mut request).is_pending());
        }
        connection.receive(json!({"id":1,"result":"right"}));
        assert!(matches!(poll!(&mut request), Poll::Ready(Ok(value)) if value == "right"));
        assert!(connection.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disconnect_releases_waiters_without_timeout() {
        let connection = RpcConnection::new(tokio::io::sink(), None, false, Duration::from_secs(1));
        let mut request = Box::pin(connection.request("fixture", Value::Null));
        assert!(poll!(&mut request).is_pending());
        connection.disconnect();
        assert!(request.await.unwrap_err().to_string().contains("closed"));
        assert!(connection.pending.lock().unwrap().is_empty());
        assert!(connection.request("fixture", Value::Null).await.is_err());
    }
}
